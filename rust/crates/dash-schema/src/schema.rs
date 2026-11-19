//! The dashboard's field descriptors (src/dashboard/schema.js): one declarative description per editable value (a
//! GuildSettings column, a part of a JSON column, or a field of a per-server record), a strict validator and a lenient
//! reader for each, and the check of channel and role ids against the server's real channels and roles.
//! The same descriptors are served to the browser ([`public_descriptor`]), which renders a form from them.
//!
//! A descriptor ([`Field`]) is kept as the JSON object the JS writes (same keys, same key order), so the JSON the
//! front end receives is that object; typed accessors read it. Every descriptor type and its keys:
//!
//! ```text
//!   boolean                         strict true/false
//!   int      min max unit?          an integer in [min, max]; unit is a display hint ('seconds', 'minutes', ...)
//!   float    min max step?          a finite number in [min, max]
//!   text     maxLength minLength? multiline? placeholders? pattern?
//!                                   a string; with `nullable` an empty or blank string is stored as null.
//!                                   `pattern` is a RegExp source the whole value must match (flags 'u').
//!   enum     options                one of options[].value
//!   multiEnum options maxItems?     unique values from options[].value
//!   color    format ('int'|'hex') min?
//!                                   accepts '#RRGGBB', 'RRGGBB' or an integer; stored as an int or '#RRGGBB'
//!   timezone                        an IANA zone name or 'UTC'
//!   channel  channelTypes           a channel id of this server whose kind is listed (CHANNEL_KINDS keys)
//!   channels channelTypes maxItems  unique channel ids
//!   role     assignable? allowEveryone? allowDangerous?
//!   roles    assignable? allowEveryone? allowDangerous? maxItems
//!   user                            a user id
//!   template requireContent?        a MessageTemplate { content, embed, buttons }; requireContent (default true)
//!   embed                           an EmbedSpec (the `embed` of a template) or null
//!   stringList maxItems itemMaxLength itemMinLength? lowercase? itemPattern?
//!   object   fields                 a plain object; every child is validated, missing children take their default,
//!                                   unknown keys are refused
//!   objectList itemFields maxItems minItems?
//!   datetime                        an ISO-8601 date-time, stored as an ISO string (UTC)
//!   time                            'HH:MM', 24 h
//!   emoji                           one Unicode emoji or a custom emoji '<:name:id>' / '<a:name:id>'
//!   url      maxLength              an absolute http(s) URL
//! ```
//! Common keys: `field`, `type`, `label`, `description?`, `default` (used when the stored value is missing or
//! malformed), `nullable?`, `required?` (object children only).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use bot_util::js::{trim as js_trim, utf16_len, utf16_prefix};
use bot_util::jsre::JsRegex;
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::js::{canon, is_finite_number, is_integer, js_keys, js_num, strict_eq, to_js_string, truthy};

/// Every descriptor type (`FIELD_TYPES`).
pub const FIELD_TYPES: [&str; 22] = [
    "boolean", "int", "float", "text", "enum", "multiEnum", "color", "timezone", "channel", "channels", "role", "roles",
    "user", "template", "embed", "stringList", "object", "objectList", "datetime", "time", "emoji", "url",
];

/// Discord channel type numbers per channel kind (`CHANNEL_KINDS`; threads are never offered).
pub const CHANNEL_KINDS: [(&str, &[i64]); 7] = [
    ("text", &[0]),
    ("voice", &[2]),
    ("category", &[4]),
    ("announcement", &[5]),
    ("stage", &[13]),
    ("forum", &[15]),
    ("media", &[16]),
];

/// The kind of a Discord channel type number, or 'other' (`channelKind`).
pub fn channel_kind(channel_type: i64) -> &'static str {
    CHANNEL_KINDS.iter().find(|(_, t)| t.contains(&channel_type)).map_or("other", |(k, _)| k)
}

/// True when `kind` is a key of [`CHANNEL_KINDS`].
pub fn is_channel_kind(kind: &str) -> bool {
    CHANNEL_KINDS.iter().any(|(k, _)| *k == kind)
}

static TIME_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^([01]\d|2[0-3]):([0-5]\d)$", ""));
static HEX_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^#?([0-9a-f]{6})$", "i"));
static CUSTOM_EMOJI_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^<a?:\w{2,32}:\d{17,20}>$", ""));
const FORBIDDEN_KEYS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// `isSnowflake(v)`: a string of 17 to 20 ASCII digits.
pub fn is_snowflake(v: &Value) -> bool {
    v.as_str().is_some_and(is_snowflake_str)
}

/// [`is_snowflake`] for a string.
pub fn is_snowflake_str(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `isPlainObject(v)`: a JSON object.
pub fn is_plain_object(v: &Value) -> bool {
    v.is_object()
}

/// True when `s` holds a control character other than tab and newline (`CONTROL_RE`).
fn has_control(s: &str) -> bool {
    s.chars().any(|c| matches!(c as u32, 0..=8 | 0x0b..=0x1f | 0x7f))
}

/// The compiled `^(?:pattern)$` (flags 'u') of a descriptor, cached; `None` when the JS would throw a SyntaxError.
fn anchored(pattern: &str) -> Option<Arc<JsRegex>> {
    static CACHE: LazyLock<Mutex<HashMap<String, Option<Arc<JsRegex>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entry(pattern.to_string())
        .or_insert_with(|| JsRegex::try_new(&format!("^(?:{pattern})$"), "u").ok().map(Arc::new))
        .clone()
}

// ── Descriptors ─────────────────────────────────────────────────────────────────────────────────────────────

/// One field descriptor: the JSON object of the JS descriptor (without its server-only function hooks), with its
/// nested descriptors (`fields` of an object, `itemFields` of an objectList) parsed too.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    raw: Map<String, Value>,
    children: Vec<Field>,
}

impl Field {
    /// A descriptor from its JSON object, written in the key order of the JS source (the order the front end
    /// receives). `Err` when `v` is not an object.
    pub fn from_json(v: Value) -> Result<Self, String> {
        match v {
            Value::Object(raw) => Ok(Self::from_map(raw)),
            other => Err(format!("a descriptor must be an object, not {other}")),
        }
    }

    /// [`Field::from_json`] for a map.
    pub fn from_map(raw: Map<String, Value>) -> Self {
        let children = ["fields", "itemFields"]
            .iter()
            .find_map(|k| raw.get(*k).and_then(Value::as_array))
            .map(|list| list.iter().filter_map(|c| c.as_object().cloned().map(Self::from_map)).collect())
            .unwrap_or_default();
        Self { raw, children }
    }

    /// The descriptor's JSON object.
    pub fn raw(&self) -> &Map<String, Value> {
        &self.raw
    }

    /// One key of the descriptor (`desc[key]`; `None` is undefined).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// A copy with one key set (`{ ...desc, [key]: value }`).
    pub fn with(&self, key: &str, value: Value) -> Self {
        let mut raw = self.raw.clone();
        raw.insert(key.to_string(), value);
        Self::from_map(raw)
    }

    /// `desc.field` ('' when missing).
    pub fn field(&self) -> &str {
        self.raw.get("field").and_then(Value::as_str).unwrap_or("")
    }

    /// `desc.type` ('' when missing).
    pub fn ty(&self) -> &str {
        self.raw.get("type").and_then(Value::as_str).unwrap_or("")
    }

    /// `desc.label`, when it is a string.
    pub fn label(&self) -> Option<&str> {
        self.raw.get("label").and_then(Value::as_str)
    }

    /// `desc.description`, when it is a string.
    pub fn description(&self) -> Option<&str> {
        self.raw.get("description").and_then(Value::as_str)
    }

    /// `desc.default` (`None` is undefined).
    pub fn default_value(&self) -> Option<&Value> {
        self.raw.get("default")
    }

    /// `structuredClone(desc.default ?? null)`.
    pub fn default_or_null(&self) -> Value {
        self.raw.get("default").cloned().unwrap_or(Value::Null)
    }

    /// `desc.nullable` (truthy).
    pub fn nullable(&self) -> bool {
        truthy(self.raw.get("nullable"))
    }

    /// `desc.required` (truthy).
    pub fn required(&self) -> bool {
        truthy(self.raw.get("required"))
    }

    fn flag(&self, key: &str) -> bool {
        self.raw.get(key) == Some(&Value::Bool(true))
    }

    /// `desc.assignable === true`.
    pub fn assignable(&self) -> bool {
        self.flag("assignable")
    }

    /// A numeric key (`min`, `max`, `maxLength`, ...), when it is a number.
    pub fn number(&self, key: &str) -> Option<f64> {
        self.raw.get(key).and_then(Value::as_f64)
    }

    /// A string key (`format`, `pattern`, ...), when it is a string.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.raw.get(key).and_then(Value::as_str)
    }

    /// `desc.channelTypes` (the kinds a channel may be), when it is a list.
    pub fn channel_types(&self) -> Option<Vec<String>> {
        self.raw.get("channelTypes").and_then(Value::as_array).map(|a| a.iter().map(|k| to_js_string(Some(k))).collect())
    }

    /// The `value`s of `desc.options`.
    pub fn option_values(&self) -> Vec<&Value> {
        self.raw
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|o| o.get("value")).collect())
            .unwrap_or_default()
    }

    /// The nested descriptors: `fields` of an object, `itemFields` of an objectList (empty otherwise).
    pub fn children(&self) -> &[Field] {
        &self.children
    }

    /// The nested descriptor named `field`.
    pub fn child(&self, field: &str) -> Option<&Field> {
        self.children.iter().find(|c| c.field() == field)
    }

    /// The JSON-safe copy served to the browser ([`public_descriptor`]).
    pub fn public_json(&self) -> Value {
        public_descriptor(self)
    }
}

/// A validation failure: `path` is the dotted location ('welcomeCardStyle.background', 'tiers.2.action').
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

impl FieldError {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { path: path.into(), message: message.into() }
    }
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FieldError {}

fn label_of(desc: &Field, path: &str) -> String {
    match desc.get("label") {
        Some(l) if truthy(Some(l)) => format!("\"{}\"", to_js_string(Some(l))),
        _ => format!("\"{path}\""),
    }
}

fn join(path: &str, key: impl std::fmt::Display) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

fn fmt_num(n: Option<f64>) -> String {
    n.map_or_else(|| "undefined".into(), bot_util::js::number_to_string)
}

/// A channel id met while validating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRef {
    pub id: String,
    pub path: String,
    pub kinds: Vec<String>,
}

/// A role id met while validating, with the checks its field asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleRef {
    pub id: String,
    pub path: String,
    pub assignable: bool,
    pub allow_everyone: bool,
    pub allow_dangerous: bool,
}

/// Ids a value refers to, collected while validating, so they can be checked against the server in one go (`Refs`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refs {
    pub channels: Vec<ChannelRef>,
    pub roles: Vec<RoleRef>,
}

/// `emptyRefs()`.
pub fn empty_refs() -> Refs {
    Refs::default()
}

fn check_channel(desc: &Field, id: &Value, path: &str, refs: &mut Refs) -> Result<(), FieldError> {
    let Some(id) = id.as_str().filter(|s| is_snowflake_str(s)) else {
        return Err(FieldError::new(path, format!("{} must be a channel of this server.", label_of(desc, path))));
    };
    let kinds = desc.channel_types().unwrap_or_else(|| CHANNEL_KINDS.iter().map(|(k, _)| (*k).to_string()).collect());
    refs.channels.push(ChannelRef { id: id.to_string(), path: path.to_string(), kinds });
    Ok(())
}

fn check_role(desc: &Field, id: &Value, path: &str, refs: &mut Refs) -> Result<(), FieldError> {
    let Some(id) = id.as_str().filter(|s| is_snowflake_str(s)) else {
        return Err(FieldError::new(path, format!("{} must be a role of this server.", label_of(desc, path))));
    };
    refs.roles.push(RoleRef {
        id: id.to_string(),
        path: path.to_string(),
        assignable: desc.flag("assignable"),
        allow_everyone: desc.flag("allowEveryone"),
        allow_dangerous: desc.flag("allowDangerous"),
    });
    Ok(())
}

fn check_list<'a>(desc: &Field, value: &'a Value, path: &str) -> Result<&'a Vec<Value>, FieldError> {
    let Some(list) = value.as_array() else {
        return Err(FieldError::new(path, format!("{} must be a list.", label_of(desc, path))));
    };
    let max = match desc.get("maxItems").filter(|v| !v.is_null()) {
        Some(v) => v.as_f64().unwrap_or(f64::NAN),
        None => desc.get("options").and_then(Value::as_array).map_or(100.0, |o| o.len() as f64),
    };
    if list.len() as f64 > max {
        return Err(FieldError::new(path, format!("{} can hold at most {} entries.", label_of(desc, path), fmt_num(Some(max)))));
    }
    if let Some(min) = desc.number("minItems").filter(|m| *m != 0.0)
        && (list.len() as f64) < min
    {
        let plural = if min == 1.0 { "y" } else { "ies" };
        return Err(FieldError::new(
            path,
            format!("{} needs at least {} entr{plural}.", label_of(desc, path), fmt_num(Some(min))),
        ));
    }
    Ok(list)
}

type IdCheck = fn(&Field, &Value, &str, &mut Refs) -> Result<(), FieldError>;

fn unique_ids(desc: &Field, value: &Value, path: &str, check: IdCheck, refs: &mut Refs) -> Result<Value, FieldError> {
    let list = check_list(desc, value, path)?;
    let mut seen: IndexSet<String> = IndexSet::new();
    for (i, id) in list.iter().enumerate() {
        check(desc, id, &join(path, i), refs)?;
        seen.insert(id.as_str().unwrap_or_default().to_string());
    }
    Ok(Value::Array(seen.into_iter().map(Value::String).collect()))
}

/// `validTimezone(tz)`: 'UTC', or a zone `Intl.DateTimeFormat` knows whose name has a '/'.
pub fn valid_timezone(tz: &Value) -> bool {
    let Some(tz) = tz.as_str() else { return false };
    let n = utf16_len(tz);
    if n == 0 || n > 64 {
        return false;
    }
    tz == "UTC" || (bot_util::time::is_valid_time_zone(tz) && tz.contains('/'))
}

/// `parseColorValue(v)`: an integer 0..=0xFFFFFF, or '#RRGGBB' / 'RRGGBB'.
pub fn parse_color_value(v: &Value) -> Option<i64> {
    if is_integer(v) {
        let n = v.as_f64()?;
        return (0.0..=16_777_215.0).contains(&n).then_some(n as i64);
    }
    let s = v.as_str()?;
    let t = js_trim(s);
    if HEX_RE.test(t) {
        let hex = t.strip_prefix('#').unwrap_or(t);
        return i64::from_str_radix(hex, 16).ok();
    }
    None
}

/// `#RRGGBB` (upper case) of a colour number.
pub fn to_hex(n: i64) -> String {
    format!("#{n:06X}")
}

/// Validates one value against its descriptor and returns the value to store (`validateValue`). `path` defaults to
/// the descriptor's field; every channel and role id met is added to `refs`.
pub fn validate_value(desc: &Field, value: &Value, path: Option<&str>, refs: &mut Refs) -> Result<Value, FieldError> {
    let path = path.unwrap_or_else(|| desc.field());
    if value.is_null() && desc.nullable() {
        return Ok(Value::Null);
    }
    let name = label_of(desc, path);
    let err = |m: String| FieldError::new(path, m);
    if value.is_null() && desc.ty() == "text" {
        return Err(err(format!("{name} cannot be empty.")));
    }
    match desc.ty() {
        "boolean" => match value {
            Value::Bool(_) => Ok(value.clone()),
            _ => Err(err(format!("{name} must be on or off."))),
        },
        ty @ ("int" | "float") => {
            let ok = if ty == "int" { is_integer(value) } else { is_finite_number(value) };
            if !ok {
                return Err(err(format!("{name} must be {}.", if ty == "int" { "a whole number" } else { "a number" })));
            }
            let n = value.as_f64().unwrap_or_default();
            let (min, max) = (desc.number("min"), desc.number("max"));
            if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                return Err(err(format!("{name} must be between {} and {}.", fmt_num(min), fmt_num(max))));
            }
            Ok(js_num(n))
        }
        "text" => {
            let Some(s) = value.as_str() else { return Err(err(format!("{name} must be text."))) };
            if has_control(s) {
                return Err(err(format!("{name} contains characters that are not allowed.")));
            }
            let multiline = truthy(desc.get("multiline"));
            let text = if multiline { s.replace("\r\n", "\n").replace('\r', "\n") } else { js_trim(s).to_string() };
            if desc.nullable() && js_trim(&text).is_empty() {
                return Ok(Value::Null);
            }
            if !multiline && text.contains('\n') {
                return Err(err(format!("{name} must be a single line.")));
            }
            if let Some(max) = desc.number("maxLength")
                && utf16_len(&text) as f64 > max
            {
                return Err(err(format!("{name} can be at most {} characters.", fmt_num(Some(max)))));
            }
            if let Some(min) = desc.number("minLength").filter(|m| *m > 0.0)
                && (utf16_len(js_trim(&text)) as f64) < min
            {
                return Err(err(if min == 1.0 {
                    format!("{name} cannot be empty.")
                } else {
                    format!("{name} needs at least {} characters.", fmt_num(Some(min)))
                }));
            }
            if let Some(p) = desc.get("pattern").filter(|p| truthy(Some(p))) {
                let ok = anchored(&to_js_string(Some(p))).is_some_and(|re| re.test(&text));
                if !ok {
                    let msg = desc.string("patternMessage").map_or_else(|| format!("{name} has an invalid format."), String::from);
                    return Err(err(msg));
                }
            }
            Ok(Value::String(text))
        }
        "enum" => {
            if desc.option_values().iter().any(|o| strict_eq(o, value)) {
                Ok(canon(value.clone()))
            } else {
                Err(err(format!("{name} must be one of the listed choices.")))
            }
        }
        "multiEnum" => {
            let list = check_list(desc, value, path)?;
            let allowed = desc.option_values();
            let mut out: Vec<Value> = Vec::new();
            for (i, v) in list.iter().enumerate() {
                if !allowed.iter().any(|o| same_value_zero(o, v)) {
                    return Err(FieldError::new(join(path, i), format!("{name} contains an unknown choice.")));
                }
                if !out.iter().any(|o| same_value_zero(o, v)) {
                    out.push(canon(v.clone()));
                }
            }
            Ok(Value::Array(out))
        }
        "color" => {
            let Some(n) = parse_color_value(value) else {
                return Err(err(format!("{name} must be a colour like #5865F2.")));
            };
            if let Some(min) = desc.number("min")
                && (n as f64) < min
            {
                return Err(err(format!("{name} cannot be pure black (#000000); pick #010101 instead.")));
            }
            Ok(if desc.string("format") == Some("hex") { Value::String(to_hex(n)) } else { Value::from(n) })
        }
        "timezone" => {
            if valid_timezone(value) {
                Ok(value.clone())
            } else {
                Err(err(format!("{name} must be a time zone such as Europe/Berlin or UTC.")))
            }
        }
        "channel" => {
            check_channel(desc, value, path, refs)?;
            Ok(value.clone())
        }
        "channels" => unique_ids(desc, value, path, check_channel, refs),
        "role" => {
            check_role(desc, value, path, refs)?;
            Ok(value.clone())
        }
        "roles" => unique_ids(desc, value, path, check_role, refs),
        "user" => {
            if is_snowflake(value) {
                Ok(value.clone())
            } else {
                Err(err(format!("{name} must be a member of this server (pick one, or paste a user id).")))
            }
        }
        "template" => {
            if !value.is_object() {
                return Err(err(format!("{name} must be a message.")));
            }
            let tpl = bot_core::custom_embed::normalize_template(value);
            let require = desc.get("requireContent") != Some(&Value::Bool(false));
            let v = bot_core::custom_embed::validate_template(&tpl, require);
            if !v.ok {
                return Err(err(format!("{name}: {}", v.errors.first().map(String::as_str).unwrap_or("undefined"))));
            }
            Ok(tpl)
        }
        "embed" => {
            if !value.is_object() {
                return Err(err(format!("{name} must be an embed.")));
            }
            let tpl = bot_core::custom_embed::normalize_template(&json!({ "content": null, "embed": value, "buttons": [] }));
            let v = bot_core::custom_embed::validate_template(&tpl, false);
            if !v.ok {
                return Err(err(format!("{name}: {}", v.errors.first().map(String::as_str).unwrap_or("undefined"))));
            }
            Ok(tpl.get("embed").cloned().unwrap_or(Value::Null))
        }
        "stringList" => {
            let list = check_list(desc, value, path)?;
            let lowercase = truthy(desc.get("lowercase"));
            let min = desc.number("itemMinLength").unwrap_or(1.0);
            let max = desc.number("itemMaxLength");
            let mut out: Vec<Value> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for (i, raw) in list.iter().enumerate() {
                let p = join(path, i);
                let Some(raw) = raw.as_str().filter(|s| !has_control(s) && !s.contains('\n')) else {
                    return Err(FieldError::new(p, format!("{name} entries must be single lines of text.")));
                };
                let mut item = js_trim(raw).to_string();
                if lowercase {
                    item = bot_util::js::lower(&item);
                }
                let len = utf16_len(&item) as f64;
                if len < min || max.is_some_and(|m| len > m) {
                    return Err(FieldError::new(
                        p,
                        format!(
                            "Each entry of {name} must be {} to {} characters (\"{}\").",
                            fmt_num(Some(min)),
                            fmt_num(max),
                            utf16_prefix(&item, 40)
                        ),
                    ));
                }
                if let Some(ip) = desc.get("itemPattern").filter(|p| truthy(Some(p)))
                    && !anchored(&to_js_string(Some(ip))).is_some_and(|re| re.test(&item))
                {
                    let tail = desc.string("itemPatternMessage").unwrap_or("has an invalid format.");
                    return Err(FieldError::new(p, format!("{name}: \"{}\" {tail}", utf16_prefix(&item, 40))));
                }
                let key = if lowercase { item.clone() } else { bot_util::js::lower(&item) };
                if !seen.insert(key) {
                    continue;
                }
                out.push(Value::String(item));
            }
            Ok(Value::Array(out))
        }
        "object" => validate_object(desc.children(), value, path, refs, &name),
        "objectList" => {
            let list = check_list(desc, value, path)?;
            let mut out = Vec::with_capacity(list.len());
            for (i, item) in list.iter().enumerate() {
                out.push(validate_object(desc.children(), item, &join(path, i), refs, &name)?);
            }
            Ok(Value::Array(out))
        }
        "datetime" => {
            let t = match value.as_str() {
                Some(s) if utf16_len(s) <= 40 => bot_util::js::date_parse(s),
                _ => f64::NAN,
            };
            match bot_util::js::to_iso_string(t) {
                Some(iso) if t.is_finite() => Ok(Value::String(iso)),
                _ => Err(err(format!("{name} must be a date and time."))),
            }
        }
        "time" => match value.as_str() {
            Some(s) if TIME_RE.test(s) => Ok(value.clone()),
            _ => Err(err(format!("{name} must be a time like 09:30."))),
        },
        "emoji" => {
            let v = value.as_str().map(js_trim).unwrap_or("");
            if !(CUSTOM_EMOJI_RE.test(v) || (utf16_len(v) <= 16 && bot_core::custom_embed::is_unicode_emoji_exact(v))) {
                return Err(err(format!("{name} must be one emoji.")));
            }
            Ok(Value::String(v.to_string()))
        }
        "url" => {
            let v = value.as_str().map(js_trim).unwrap_or("");
            if desc.nullable() && v.is_empty() {
                return Ok(Value::Null);
            }
            let max = desc.number("maxLength").unwrap_or(2048.0);
            if !bot_core::custom_embed::is_http_url(v) || utf16_len(v) as f64 > max {
                return Err(err(format!("{name} must be an http(s) link.")));
            }
            Ok(Value::String(v.to_string()))
        }
        _ => Err(err(format!("{name} has an unknown type."))),
    }
}

/// `SameValueZero` for JSON values (what `Set` and `includes` use).
fn same_value_zero(a: &Value, b: &Value) -> bool {
    strict_eq(a, b)
}

fn validate_object(fields: &[Field], value: &Value, path: &str, refs: &mut Refs, name: &str) -> Result<Value, FieldError> {
    let Some(obj) = value.as_object() else {
        return Err(FieldError::new(path, format!("{name} must be an object.")));
    };
    for key in js_keys(obj) {
        if FORBIDDEN_KEYS.contains(&key.as_str()) || !fields.iter().any(|f| f.field() == key) {
            return Err(FieldError::new(join(path, key), format!("\"{}\" is not a known option.", utf16_prefix(key, 40))));
        }
    }
    let mut out = Map::new();
    for f in fields {
        let p = join(path, f.field());
        if let Some(v) = obj.get(f.field()) {
            out.insert(f.field().to_string(), validate_value(f, v, Some(&p), refs)?);
        } else if f.required() {
            return Err(FieldError::new(p.clone(), format!("{} is required.", label_of(f, &p))));
        } else {
            out.insert(f.field().to_string(), f.default_or_null());
        }
    }
    Ok(Value::Object(out))
}

/// Reads a stored value leniently for display (`readValue`): anything malformed reads as the descriptor's default.
/// Channel and role ids are kept even when they no longer exist (the browser marks them as deleted). `raw` `None` is
/// undefined (a missing key). Never fails.
pub fn read_value(desc: &Field, raw: Option<&Value>) -> Value {
    let structured = matches!(desc.ty(), "template" | "embed" | "object" | "objectList");
    let Some(raw) = raw else { return desc.default_or_null() };
    if raw.is_null() && !desc.nullable() && !structured {
        return desc.default_or_null();
    }
    if raw.is_null() && desc.nullable() {
        return Value::Null;
    }
    match desc.ty() {
        "template" => {
            if raw.is_null() {
                desc.default_or_null()
            } else {
                bot_core::custom_embed::normalize_template(raw)
            }
        }
        "embed" => {
            if raw.is_null() {
                return Value::Null;
            }
            let tpl = bot_core::custom_embed::normalize_template(&json!({ "content": null, "embed": raw, "buttons": [] }));
            tpl.get("embed").cloned().unwrap_or(Value::Null)
        }
        "object" => {
            let empty = Map::new();
            let src = raw.as_object().unwrap_or(&empty);
            Value::Object(desc.children().iter().map(|f| (f.field().to_string(), read_value(f, src.get(f.field())))).collect())
        }
        "objectList" => match raw.as_array() {
            Some(list) => {
                let cap = desc.number("maxItems").unwrap_or(100.0).max(0.0) as usize;
                Value::Array(
                    list.iter()
                        .filter_map(Value::as_object)
                        .take(cap)
                        .map(|item| {
                            Value::Object(
                                desc.children().iter().map(|f| (f.field().to_string(), read_value(f, item.get(f.field())))).collect(),
                            )
                        })
                        .collect(),
                )
            }
            None => desc.default_value().cloned().unwrap_or_else(|| json!([])),
        },
        "channels" | "roles" => match raw.as_array() {
            Some(list) => Value::Array(list.iter().filter(|v| is_snowflake(v)).cloned().collect()),
            None => desc.default_value().cloned().unwrap_or_else(|| json!([])),
        },
        "channel" | "role" | "user" => {
            if is_snowflake(raw) {
                raw.clone()
            } else {
                desc.default_or_null()
            }
        }
        "color" => match parse_color_value(raw) {
            None => desc.default_or_null(),
            Some(n) if desc.string("format") == Some("hex") => Value::String(to_hex(n)),
            Some(n) => Value::from(n),
        },
        "datetime" => {
            let t = bot_util::js::date_parse(&to_js_string(Some(raw)));
            match bot_util::js::to_iso_string(t) {
                Some(iso) if t.is_finite() => Value::String(iso),
                _ => desc.default_or_null(),
            }
        }
        _ => validate_value(desc, raw, Some(desc.field()), &mut Refs::default()).unwrap_or_else(|_| desc.default_or_null()),
    }
}

/// One channel as the reference check needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    /// [`channel_kind`] of its type.
    pub kind: String,
}

/// One role as the reference check needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoleInfo {
    /// The @everyone role.
    pub everyone: bool,
    /// Managed by an integration.
    pub managed: bool,
    /// Below the bot's highest role (the bot can give it).
    pub assignable: bool,
}

/// The server's channels and roles, as the reference check needs them (`GuildResources`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuildResources {
    pub channels: HashMap<String, ChannelInfo>,
    pub roles: HashMap<String, RoleInfo>,
}

/// Ids that were already stored (`checkRefs`' `keep`): they pass even when they are gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeepRefs {
    pub channels: HashSet<String>,
    pub roles: HashSet<String>,
}

/// Checks collected ids against the server (`checkRefs`); the first problem is returned. Ids in `keep` pass even
/// when they are gone, so saving an unrelated change never fails because of an old, deleted channel.
pub fn check_refs(refs: &Refs, resources: &GuildResources, keep: &KeepRefs) -> Result<(), FieldError> {
    for r in &refs.channels {
        let Some(channel) = resources.channels.get(&r.id) else {
            if keep.channels.contains(&r.id) {
                continue;
            }
            return Err(FieldError::new(&r.path, "That channel does not exist in this server any more. Pick another one."));
        };
        if !r.kinds.contains(&channel.kind) {
            return Err(FieldError::new(&r.path, format!("That channel is the wrong kind here (expected {}).", r.kinds.join(" or "))));
        }
    }
    for r in &refs.roles {
        let Some(role) = resources.roles.get(&r.id) else {
            if keep.roles.contains(&r.id) {
                continue;
            }
            return Err(FieldError::new(&r.path, "That role does not exist in this server any more. Pick another one."));
        };
        if role.everyone && !r.allow_everyone {
            return Err(FieldError::new(&r.path, "@everyone cannot be used here."));
        }
        if r.assignable && (role.managed || !role.assignable) {
            return Err(FieldError::new(
                &r.path,
                if role.managed {
                    "That role is managed by an integration and cannot be given by the bot."
                } else {
                    "That role is above the bot's highest role. Move the bot's role higher in Server Settings → Roles, or pick a lower role."
                },
            ));
        }
    }
    Ok(())
}

/// Every channel and role id inside stored values (`collectStoredRefs`' `{ channels: Set, roles: Set }`), in the order
/// met.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredRefs {
    pub channels: IndexSet<String>,
    pub roles: IndexSet<String>,
}

impl StoredRefs {
    /// The `keep` sets of [`check_refs`].
    pub fn to_keep(&self) -> KeepRefs {
        KeepRefs { channels: self.channels.iter().cloned().collect(), roles: self.roles.iter().cloned().collect() }
    }
}

fn snowflakes(raw: &Value) -> impl Iterator<Item = &str> {
    raw.as_array().into_iter().flatten().filter_map(Value::as_str).filter(|s| is_snowflake_str(s))
}

/// Every channel and role id inside a stored value, added to `out` (`collectStoredRefs`). Never fails.
pub fn collect_stored_refs(desc: &Field, raw: Option<&Value>, out: &mut StoredRefs) {
    let Some(raw) = raw.filter(|r| !r.is_null()) else { return };
    match desc.ty() {
        "channel" => {
            if let Some(s) = raw.as_str().filter(|s| is_snowflake_str(s)) {
                out.channels.insert(s.to_string());
            }
        }
        "role" => {
            if let Some(s) = raw.as_str().filter(|s| is_snowflake_str(s)) {
                out.roles.insert(s.to_string());
            }
        }
        "channels" => out.channels.extend(snowflakes(raw).map(String::from)),
        "roles" => out.roles.extend(snowflakes(raw).map(String::from)),
        "object" => {
            if let Some(o) = raw.as_object() {
                for f in desc.children() {
                    collect_stored_refs(f, o.get(f.field()), out);
                }
            }
        }
        "objectList" => {
            for item in raw.as_array().into_iter().flatten().filter_map(Value::as_object) {
                for f in desc.children() {
                    collect_stored_refs(f, item.get(f.field()), out);
                }
            }
        }
        _ => {}
    }
}

/// A role id in an `assignable` field of a stored record, with the path of the field that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssignableRole {
    pub id: String,
    pub path: String,
}

/// Every role id of a stored record that sits in an `assignable` field (a role the bot gives or takes), with the
/// path of the field that holds it (`collectAssignableRoles`). Never fails.
pub fn collect_assignable_roles(fields: &[Field], record: Option<&Value>) -> Vec<AssignableRole> {
    fn walk(desc: &Field, raw: Option<&Value>, path: &str, out: &mut Vec<AssignableRole>) {
        let Some(raw) = raw.filter(|r| !r.is_null()) else { return };
        match desc.ty() {
            "role" => {
                if desc.assignable()
                    && let Some(s) = raw.as_str().filter(|s| is_snowflake_str(s))
                {
                    out.push(AssignableRole { id: s.to_string(), path: path.to_string() });
                }
            }
            "roles" => {
                if desc.assignable()
                    && let Some(list) = raw.as_array()
                {
                    for (i, id) in list.iter().enumerate() {
                        if let Some(s) = id.as_str().filter(|s| is_snowflake_str(s)) {
                            out.push(AssignableRole { id: s.to_string(), path: format!("{path}.{i}") });
                        }
                    }
                }
            }
            "object" => {
                if let Some(o) = raw.as_object() {
                    for f in desc.children() {
                        walk(f, o.get(f.field()), &format!("{path}.{}", f.field()), out);
                    }
                }
            }
            "objectList" => {
                if let Some(list) = raw.as_array() {
                    for (i, item) in list.iter().enumerate() {
                        if let Some(o) = item.as_object() {
                            for f in desc.children() {
                                walk(f, o.get(f.field()), &format!("{path}.{i}.{}", f.field()), out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for desc in fields {
        let raw = record.and_then(Value::as_object).and_then(|r| r.get(desc.field()));
        walk(desc, raw, desc.field(), &mut out);
    }
    out
}

/// The key of an id at a place in a value, with list indexes dropped (`refKey`): `ref_key("tiers.2.roleAdd", id)`
/// equals `ref_key("tiers.0.roleAdd", id)` but not `ref_key("tiers.0.roleRemove", id)`.
pub fn ref_key(path: &str, id: &str) -> String {
    let kept: Vec<&str> = path.split('.').filter(|seg| seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit())).collect();
    format!("{}|{id}", kept.join("."))
}

/// The refKeys of every channel and role id inside a stored value, added to `out` (`storedRefKeys`): which id sits in
/// which field. `path` defaults to the descriptor's field. Never fails.
pub fn stored_ref_keys(desc: &Field, raw: Option<&Value>, path: Option<&str>, out: &mut IndexSet<String>) {
    let path = path.unwrap_or_else(|| desc.field());
    let Some(raw) = raw.filter(|r| !r.is_null()) else { return };
    match desc.ty() {
        "channel" | "role" => {
            if let Some(s) = raw.as_str().filter(|s| is_snowflake_str(s)) {
                out.insert(ref_key(path, s));
            }
        }
        "channels" | "roles" => {
            for s in snowflakes(raw) {
                out.insert(ref_key(path, s));
            }
        }
        "object" => {
            if let Some(o) = raw.as_object() {
                for f in desc.children() {
                    stored_ref_keys(f, o.get(f.field()), Some(&format!("{path}.{}", f.field())), out);
                }
            }
        }
        "objectList" => {
            for item in raw.as_array().into_iter().flatten().filter_map(Value::as_object) {
                for f in desc.children() {
                    stored_ref_keys(f, item.get(f.field()), Some(&format!("{path}.{}", f.field())), out);
                }
            }
        }
        _ => {}
    }
}

/// Keys that never reach the browser.
pub const SERVER_ONLY_KEYS: [&str; 6] = ["check", "read", "write", "toStored", "fromStored", "normalize"];

/// The JSON-safe copy of a descriptor that is served to the browser (`publicDescriptor`): server-only keys removed,
/// nested descriptors converted too, keys in the descriptor's own order.
pub fn public_descriptor(desc: &Field) -> Value {
    let mut out = Map::new();
    for (key, value) in &desc.raw {
        if SERVER_ONLY_KEYS.contains(&key.as_str()) {
            continue;
        }
        if (key == "fields" || key == "itemFields") && value.is_array() {
            out.insert(key.clone(), Value::Array(desc.children.iter().map(public_descriptor).collect()));
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}

/// Checks a descriptor list (`lintDescriptors`): known types, the keys each type needs, unique field names, defaults
/// that validate. Returns the problems (empty = fine).
pub fn lint_descriptors(fields: &[Field], at_prefix: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen: Vec<Option<Value>> = Vec::new();
    for d in fields {
        let field = d.get("field");
        let at = format!("{at_prefix}{}", to_js_string(field));
        if !matches!(field, Some(Value::String(s)) if !s.is_empty()) {
            problems.push(format!("{at_prefix}: a descriptor has no field"));
        }
        let key = field.cloned();
        if seen.iter().any(|s| match (s, &key) {
            (None, None) => true,
            (Some(a), Some(b)) => strict_eq(a, b),
            _ => false,
        }) {
            problems.push(format!("{at}: duplicate field"));
        }
        seen.push(key);
        let ty = d.get("type");
        let ty_str = ty.and_then(Value::as_str).unwrap_or("");
        if !FIELD_TYPES.contains(&ty_str) || !matches!(ty, Some(Value::String(_))) {
            problems.push(format!("{at}: unknown type {}", to_js_string(ty)));
            continue;
        }
        if !matches!(d.get("label"), Some(Value::String(s)) if !s.is_empty()) {
            problems.push(format!("{at}: no label"));
        }
        let need: &[&str] = match ty_str {
            "int" | "float" => &["min", "max"],
            "text" => &["maxLength"],
            "enum" | "multiEnum" => &["options"],
            "color" => &["format"],
            "channel" => &["channelTypes"],
            "channels" => &["channelTypes", "maxItems"],
            "roles" => &["maxItems"],
            "stringList" => &["maxItems", "itemMaxLength"],
            "object" => &["fields"],
            "objectList" => &["itemFields", "maxItems"],
            _ => &[],
        };
        for key in need {
            if d.get(key).is_none() {
                problems.push(format!("{at}: {ty_str} needs {key}"));
            }
        }
        for kind in d.get("channelTypes").and_then(Value::as_array).into_iter().flatten() {
            let k = to_js_string(Some(kind));
            if !is_channel_kind(&k) {
                problems.push(format!("{at}: unknown channel kind {k}"));
            }
        }
        if ty_str == "object" {
            problems.extend(lint_descriptors(d.children(), &format!("{at}.")));
        }
        if ty_str == "objectList" {
            problems.extend(lint_descriptors(d.children(), &format!("{at}[].")));
        }
        if let Some(def) = d.default_value().filter(|v| !v.is_null())
            && !matches!(ty_str, "template" | "embed" | "channel" | "channels" | "role" | "roles" | "user")
            && let Err(e) = validate_value(d, def, Some(&at), &mut Refs::default())
        {
            problems.push(format!("{at}: the default does not validate ({})", e.message));
        }
    }
    problems
}
