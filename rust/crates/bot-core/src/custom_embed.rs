//! Port of `src/utils/customEmbed.js`: message templates, the model for admin-configured messages (welcome,
//! reminders, tickets, ...). A MessageTemplate is `{ content, embed, buttons }` with placeholders, validated against
//! Discord's limits and rendered into a message payload. Rendering builds plain JSON that is clamped and URL-checked
//! first and wrapped afterwards. Nothing here fails.
//!
//! Templates are stored as JSON (settings columns, saved embeds), so they are handled as [`serde_json::Value`] with
//! the JS key order: [`normalize_template`] returns exactly what `normalizeTemplate` returns, and [`template_to_json`]
//! what `templateToJson` prints. Every length is counted in UTF-16 units and every `trim` / `\s` is JavaScript's.

use std::collections::HashSet;
use std::sync::LazyLock;

use bot_config::colors;
use bot_util::js::{is_js_space, utf16_len, utf16_prefix};
use bot_util::placeholder_parser::{RenderContext, render_placeholders};
use bot_util::text::clamp_text;
use serde_json::{Map, Value, json};
use serenity::all::{EmojiId, InputTextStyle, ReactionType};
use serenity::builder::{CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateInputText, CreateModal};

use crate::interaction_helpers::Payload;

/// Embed and message limits (`LIMITS`). Per-guild entity caps are a different table (`bot_config::LIMITS`).
pub mod limits {
    pub const CONTENT: usize = 2000;
    pub const TITLE: usize = 256;
    pub const DESCRIPTION: usize = 4096;
    pub const FIELD_NAME: usize = 256;
    pub const FIELD_VALUE: usize = 1024;
    pub const FIELDS: usize = 25;
    pub const FOOTER: usize = 2048;
    pub const AUTHOR: usize = 256;
    pub const TOTAL: usize = 6000;
    pub const BUTTONS: usize = 25;
    pub const BUTTON_LABEL: usize = 80;
    pub const URL: usize = 2048;
    pub const BUTTON_URL: usize = 512;
}

/// Modal input ids (`TEMPLATE_FIELD_IDS`, `kind: 'field'`).
pub const TEMPLATE_FIELD_IDS: [(&str, &str); 5] =
    [("content", "tpl-content"), ("title", "tpl-title"), ("description", "tpl-description"), ("color", "tpl-color"), ("footer", "tpl-footer")];

/// Maximum length of each modal input (`MODAL_INPUT_MAX`).
pub const MODAL_INPUT_MAX: [(&str, usize); 5] = [("content", 2000), ("title", 256), ("description", 4000), ("color", 16), ("footer", 2048)];

const MAX_JSON_BYTES: usize = 20_480;
const BUTTONS_PER_ROW: usize = 5;
const MAX_ROWS: usize = 5;

fn field_id(key: &str) -> &'static str {
    TEMPLATE_FIELD_IDS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or("")
}

fn input_max(key: &str) -> usize {
    MODAL_INPUT_MAX.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(0)
}

// ── Small helpers ──────────────────────────────────────────────────────────────

fn len(s: &str) -> usize {
    utf16_len(s)
}

fn js_trim(s: &str) -> &str {
    bot_util::js::trim(s)
}

fn is_blank(s: &str) -> bool {
    js_trim(s).is_empty()
}

/// `Number.isInteger(n) && n >= 0 && n <= 0xffffff`.
fn int24(v: &Value) -> Option<u32> {
    let n = v.as_f64()?;
    (n.fract() == 0.0 && (0.0..=16_777_215.0).contains(&n)).then_some(n as u32)
}

/// `Number.isInteger(v)`.
fn is_integer(v: Option<&Value>) -> bool {
    v.and_then(Value::as_f64).is_some_and(|n| n.is_finite() && n.fract() == 0.0)
}

/// `isHttpUrl(str)`: an absolute http(s) URL of at most 2048 characters without whitespace (WHATWG parsing).
pub fn is_http_url(s: &str) -> bool {
    if len(s) > limits::URL || s.chars().any(is_js_space) {
        return false;
    }
    match url::Url::parse(s) {
        Ok(u) => (u.scheme() == "http" || u.scheme() == "https") && u.host_str().is_some_and(|h| !h.is_empty()),
        Err(_) => false,
    }
}

/// `ATTACHMENT_RE` (`^attachment:\/\/[^\s/]{1,200}$`, counted in UTF-16 units).
fn is_attachment_url(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("attachment://") else { return false };
    let n = len(rest);
    (1..=200).contains(&n) && !rest.chars().any(|c| c == '/' || is_js_space(c))
}

fn image_url_ok(s: &str) -> bool {
    is_http_url(s) || is_attachment_url(s)
}

fn is_placeholder_url(s: &str) -> bool {
    s.contains('{')
}

/// `text(v)`: a string with content, or a finite number as text; anything else is absent.
fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()).map(bot_util::js::number_to_string),
        Value::String(s) if !is_blank(s) => Some(s.clone()),
        _ => None,
    }
}

fn str_or_null(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => Value::String(s.clone()),
        _ => Value::Null,
    }
}

/// The emoji table generated from the JS regex engine (`rust/fixtures/gen/bot-core-emoji.mjs`).
struct EmojiData {
    pictographic: Vec<(u32, u32)>,
    rgi: HashSet<String>,
}

static EMOJI: LazyLock<EmojiData> = LazyLock::new(|| {
    let raw: Value = serde_json::from_str(include_str!("../data/emoji.json")).unwrap_or(Value::Null);
    let pictographic = raw["extendedPictographic"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| Some((r[0].as_u64()? as u32, r[1].as_u64()? as u32))).collect())
        .unwrap_or_default();
    let rgi = raw["rgi"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
    EmojiData { pictographic, rgi }
});

/// The RGI emoji ZWJ sequences of V8's `\p{RGI_Emoji}` (`rust/fixtures/gen/bot-core-emoji-zwj.mjs`).
static RGI_ZWJ: LazyLock<HashSet<String>> = LazyLock::new(|| {
    let raw: Value = serde_json::from_str(include_str!("../data/emoji-zwj.json")).unwrap_or(Value::Null);
    raw["zwj"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
});

/// `/^\p{Extended_Pictographic}$/u` for one code point (the table V8 uses).
pub fn is_extended_pictographic(c: char) -> bool {
    is_pictographic(c)
}

/// `/^\p{RGI_Emoji}$/v`: V8's set (single emoji, presentation and modifier sequences, keycaps, flags, tag sequences
/// and the RGI ZWJ sequences). A ZWJ chain that is not RGI (two animals joined, say) is refused.
pub fn is_rgi_emoji(s: &str) -> bool {
    EMOJI.rgi.contains(s) || (s.contains('\u{200D}') && RGI_ZWJ.contains(s))
}

/// `/^(?:\p{RGI_Emoji}|\p{Extended_Pictographic}️?)$/v` exactly (no structural ZWJ guess, unlike
/// [`is_unicode_emoji`]).
pub fn is_unicode_emoji_exact(s: &str) -> bool {
    if is_rgi_emoji(s) {
        return true;
    }
    let chars: Vec<char> = s.chars().collect();
    matches!(chars.as_slice(), [c] | [c, '\u{FE0F}'] if is_pictographic(*c))
}

fn is_pictographic(c: char) -> bool {
    let cp = c as u32;
    EMOJI.pictographic.binary_search_by(|(lo, hi)| if cp < *lo { std::cmp::Ordering::Greater } else if cp > *hi { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }).is_ok()
}

/// One element of a ZWJ sequence: an RGI emoji (single, modifier sequence, ...) or a pictographic code point with
/// an optional U+FE0F.
fn zwj_element_ok(s: &str) -> bool {
    if EMOJI.rgi.contains(s) {
        return true;
    }
    let chars: Vec<char> = s.chars().collect();
    match chars.as_slice() {
        [c] => is_pictographic(*c),
        [c, '\u{FE0F}'] => is_pictographic(*c),
        _ => false,
    }
}

/// `/^(?:\p{RGI_Emoji}|\p{Extended_Pictographic}️?)$/v`. RGI ZWJ sequences are recognised by their structure (two or
/// more emoji joined by U+200D), which is wider than the RGI list.
pub fn is_unicode_emoji(s: &str) -> bool {
    if EMOJI.rgi.contains(s) {
        return true;
    }
    let chars: Vec<char> = s.chars().collect();
    match chars.as_slice() {
        [c] if is_pictographic(*c) => return true,
        [c, '\u{FE0F}'] if is_pictographic(*c) => return true,
        _ => {}
    }
    let parts: Vec<&str> = s.split('\u{200D}').collect();
    parts.len() >= 2 && parts.iter().all(|p| zwj_element_ok(p))
}

/// `parseButtonEmoji(str)`: `{ id, name, animated }` for a custom emoji, `{ name }` for a single Unicode emoji, `None`
/// otherwise.
pub fn parse_button_emoji(s: &str) -> Option<Value> {
    let s = js_trim(s);
    if let Some(inner) = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        let parts: Vec<&str> = inner.split(':').collect();
        if let [a, name, id] = parts.as_slice() {
            let name_ok = (2..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            let id_ok = (17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
            if (a.is_empty() || *a == "a") && name_ok && id_ok {
                return Some(json!({ "id": id, "name": name, "animated": *a == "a" }));
            }
        }
    }
    if len(s) <= 16 && is_unicode_emoji(s) {
        return Some(json!({ "name": s }));
    }
    None
}

fn reaction_of(emoji: &Value) -> Option<ReactionType> {
    match emoji.get("id").and_then(Value::as_str) {
        Some(id) => Some(ReactionType::Custom {
            animated: emoji["animated"].as_bool().unwrap_or(false),
            id: EmojiId::new(id.parse::<u64>().ok().filter(|v| *v != 0)?),
            name: emoji["name"].as_str().map(String::from),
        }),
        None => emoji["name"].as_str().map(|n| ReactionType::Unicode(n.to_string())),
    }
}

// ── Colours ────────────────────────────────────────────────────────────────────

/// `COLOR_NAMES`.
pub const COLOR_NAMES: [(&str, u32); 12] = [
    ("red", 0xed4245),
    ("green", 0x57f287),
    ("blue", 0x3498db),
    ("blurple", 0x5865f2),
    ("yellow", 0xfee75c),
    ("orange", 0xe67e22),
    ("purple", 0x9b59b6),
    ("pink", 0xeb459e),
    ("white", 0xffffff),
    ("black", 0x010101),
    ("grey", 0x95a5a6),
    ("gray", 0x95a5a6),
];

const BLACK: u32 = 0x010101;

/// `parseColor(input)`: `'#5865F2'`, `'5865f2'`, `'0x5865F2'`, `'#fff'`, a colour name, or an integer.
pub fn parse_color(input: &Value) -> Option<u32> {
    match input {
        Value::Number(_) => int24(input),
        Value::String(s) => parse_color_str(s),
        _ => None,
    }
}

/// [`parse_color`] of a string.
pub fn parse_color_str(s: &str) -> Option<u32> {
    let s = bot_util::js::lower(js_trim(s));
    if let Some((_, c)) = COLOR_NAMES.iter().find(|(n, _)| *n == s) {
        return Some(*c);
    }
    let hex = s.strip_prefix('#').or_else(|| s.strip_prefix("0x")).unwrap_or(&s);
    if !(hex.len() == 3 || hex.len() == 6) || !hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return None;
    }
    let full: String = if hex.len() == 3 { hex.chars().flat_map(|c| [c, c]).collect() } else { hex.to_string() };
    u32::from_str_radix(&full, 16).ok()
}

/// `formatColor(n)`: `'#RRGGBB'` (`'#000000'` for anything that is not a colour).
pub fn format_color(n: &Value) -> String {
    match int24(n) {
        Some(c) => format!("#{c:06X}"),
        None => "#000000".into(),
    }
}

// ── Normalisation ──────────────────────────────────────────────────────────────

/// `emptyTemplate()`.
pub fn empty_template() -> Value {
    json!({ "content": null, "embed": null, "buttons": [] })
}

fn icon_of(o: &Value) -> Value {
    for k in ["iconUrl", "iconURL", "icon_url"] {
        match o.get(k) {
            Some(Value::Null) | None => continue,
            Some(Value::String(s)) => return Value::String(s.clone()),
            Some(_) => return Value::Null,
        }
    }
    Value::Null
}

/// `normalizeLegacyEmbed(legacy)`: an embed in any known shape (our EmbedSpec, an APIEmbed, the legacy welcome
/// shape) as an EmbedSpec. Unknown keys are dropped; the field count is not truncated.
pub fn normalize_legacy_embed(legacy: &Value) -> Value {
    let Some(obj) = legacy.as_object() else { return json!({}) };
    let mut out = Map::new();
    for key in ["title", "description", "url"] {
        if let Some(v) = text(obj.get(key)) {
            out.insert(key.into(), Value::String(v));
        }
    }
    if let Some(c) = obj.get("color").and_then(parse_color) {
        out.insert("color".into(), json!(c));
    }
    match obj.get("author") {
        Some(Value::String(a)) if !is_blank(a) => {
            out.insert("author".into(), json!({ "name": a }));
        }
        Some(a @ Value::Object(ao)) => {
            if let Some(Value::String(name)) = ao.get("name")
                && !is_blank(name)
            {
                out.insert("author".into(), json!({ "name": name, "iconUrl": icon_of(a), "url": str_or_null(ao.get("url")) }));
            }
        }
        _ => {}
    }
    match obj.get("footer") {
        Some(Value::String(f)) if !is_blank(f) => {
            out.insert("footer".into(), json!({ "text": f, "iconUrl": null }));
        }
        Some(f @ Value::Object(fo)) => {
            if let Some(Value::String(t)) = fo.get("text")
                && !is_blank(t)
            {
                out.insert("footer".into(), json!({ "text": t, "iconUrl": icon_of(f) }));
            }
        }
        _ => {}
    }
    let pick_url = |v: Option<&Value>| -> Option<Value> {
        match v {
            Some(Value::Object(o)) => o.get("url").cloned(),
            other => other.cloned(),
        }
    };
    if let Some(Value::String(thumb)) = pick_url(obj.get("thumbnail")) {
        if thumb == "user_avatar" {
            out.insert("thumbnail".into(), json!("{user.avatar}"));
        } else if thumb == "server_icon" {
            out.insert("thumbnail".into(), json!("{server.icon}"));
        } else if thumb != "none" && !is_blank(&thumb) {
            out.insert("thumbnail".into(), Value::String(thumb));
        }
    }
    if let Some(Value::String(image)) = pick_url(obj.get("image"))
        && !is_blank(&image)
    {
        out.insert("image".into(), Value::String(image));
    }
    if let Some(Value::Array(fields)) = obj.get("fields") {
        let mut list = Vec::new();
        for f in fields {
            let Some(fo) = f.as_object() else { continue };
            if let (Some(name), Some(value)) = (text(fo.get("name")), text(fo.get("value"))) {
                list.push(json!({ "name": name, "value": value, "inline": truthy(fo.get("inline")) }));
            }
        }
        if !list.is_empty() {
            out.insert("fields".into(), Value::Array(list));
        }
    }
    let ts = obj.get("timestamp");
    let stamp = match ts {
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(f64::is_finite),
        _ => false,
    };
    if stamp {
        out.insert("timestamp".into(), json!(true));
    }
    Value::Object(out)
}

/// `Boolean(v)` of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `isVisible(spec)`: Discord would draw something (a colour alone is not visible).
pub fn is_visible(spec: &Value) -> bool {
    let s = |k: &str| spec.get(k).is_some_and(|v| truthy(Some(v)));
    s("title")
        || s("description")
        || spec.get("fields").and_then(Value::as_array).is_some_and(|f| !f.is_empty())
        || spec.get("author").and_then(|a| a.get("name")).is_some_and(|v| truthy(Some(v)))
        || spec.get("footer").and_then(|a| a.get("text")).is_some_and(|v| truthy(Some(v)))
        || s("image")
        || s("thumbnail")
}

/// `specFromEmbed(embed)`: an APIEmbed (Discord JSON) as an EmbedSpec.
pub fn spec_from_embed(d: &Value) -> Value {
    if !d.is_object() {
        return json!({});
    }
    let author = d.get("author").filter(|a| truthy(Some(a))).map(|a| json!({ "name": a.get("name"), "iconUrl": a.get("icon_url"), "url": a.get("url") }));
    let footer = d.get("footer").filter(|a| truthy(Some(a))).map(|f| json!({ "text": f.get("text"), "iconUrl": f.get("icon_url") }));
    let mut m = Map::new();
    m.insert("title".into(), d.get("title").cloned().unwrap_or(Value::Null));
    m.insert("description".into(), d.get("description").cloned().unwrap_or(Value::Null));
    m.insert("url".into(), d.get("url").cloned().unwrap_or(Value::Null));
    m.insert("color".into(), d.get("color").cloned().unwrap_or(Value::Null));
    m.insert("author".into(), author.unwrap_or(Value::Null));
    m.insert("footer".into(), footer.unwrap_or(Value::Null));
    m.insert("thumbnail".into(), d.get("thumbnail").and_then(|t| t.get("url")).cloned().unwrap_or(Value::Null));
    m.insert("image".into(), d.get("image").and_then(|t| t.get("url")).cloned().unwrap_or(Value::Null));
    m.insert("fields".into(), d.get("fields").cloned().unwrap_or(Value::Null));
    m.insert("timestamp".into(), json!(truthy(d.get("timestamp"))));
    normalize_legacy_embed(&Value::Object(m))
}

fn button_emoji(emoji: Option<&Value>) -> Value {
    match emoji {
        Some(Value::String(s)) if !is_blank(s) => Value::String(js_trim(s).to_string()),
        Some(Value::Object(o)) if o.get("id").is_some_and(|v| truthy(Some(v))) => {
            let a = if truthy(o.get("animated")) { "a" } else { "" };
            let name = o.get("name").map(js_string).unwrap_or_else(|| "undefined".into());
            let id = o.get("id").map(js_string).unwrap_or_default();
            Value::String(format!("<{a}:{name}:{id}>"))
        }
        Some(Value::Object(o)) => match o.get("name") {
            Some(Value::String(n)) if !n.is_empty() => Value::String(n.clone()),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

/// `String(v)` for a JSON value.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

fn normalize_buttons(input: &Map<String, Value>) -> Value {
    let mut raw: Vec<Value> = Vec::new();
    if let Some(Value::Array(b)) = input.get("buttons") {
        raw = b.clone();
    } else if let Some(Value::Array(rows)) = input.get("components") {
        for row in rows {
            for c in row.get("components").and_then(Value::as_array).into_iter().flatten() {
                if c.get("type").and_then(Value::as_f64) == Some(2.0) && c.get("style").and_then(Value::as_f64) == Some(5.0) {
                    raw.push(c.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    for b in raw {
        let Some(o) = b.as_object() else { continue };
        let field = |k: &str| match o.get(k) {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_trim(&js_string(v)).to_string(),
        };
        let (label, url) = (field("label"), field("url"));
        if !label.is_empty() && !url.is_empty() {
            out.push(json!({ "label": label, "url": url, "emoji": button_emoji(o.get("emoji")) }));
        }
    }
    Value::Array(out)
}

/// `normalizeTemplate(input)`: anything a template may be stored or imported as (a template, a bare EmbedSpec, the
/// legacy welcome shape, a plain string, Discord / Discohook message JSON) as a MessageTemplate. A colour-only
/// embed is kept so a modal edit keeps its colour.
pub fn normalize_template(input: &Value) -> Value {
    match input {
        Value::String(s) => json!({ "content": if is_blank(s) { Value::Null } else { Value::String(s.clone()) }, "embed": null, "buttons": [] }),
        Value::Object(o) => {
            let is_template = ["content", "embed", "buttons", "embeds", "components"].iter().any(|k| o.contains_key(*k));
            if !is_template {
                let spec = normalize_legacy_embed(input);
                return json!({ "content": null, "embed": if is_visible(&spec) { spec } else { Value::Null }, "buttons": [] });
            }
            let mut spec: Option<Value> = None;
            if let Some(e @ Value::Object(_)) = o.get("embed") {
                spec = Some(normalize_legacy_embed(e));
            } else if let Some(Value::Array(embeds)) = o.get("embeds")
                && let Some(first) = embeds.first().filter(|e| truthy(Some(e)))
            {
                spec = Some(spec_from_embed(first));
            }
            if let Some(s) = &spec
                && !is_visible(s)
                && !is_integer(s.get("color"))
            {
                spec = None;
            }
            json!({ "content": text(o.get("content")), "embed": spec, "buttons": normalize_buttons(o) })
        }
        _ => empty_template(),
    }
}

// ── Validation ─────────────────────────────────────────────────────────────────

/// The error [`validate_template`] reports for an empty template (`EMPTY_MESSAGE_ERROR`).
pub const EMPTY_MESSAGE_ERROR: &str = "The message is empty. Add some text or an embed.";

fn s_len(v: Option<&Value>) -> usize {
    v.and_then(Value::as_str).map(len).unwrap_or(0)
}

fn embed_text_length(e: &Value) -> usize {
    let mut n = s_len(e.get("title")) + s_len(e.get("description")) + s_len(e.get("author").and_then(|a| a.get("name")))
        + s_len(e.get("footer").and_then(|f| f.get("text")));
    for f in e.get("fields").and_then(Value::as_array).into_iter().flatten() {
        n += s_len(f.get("name")) + s_len(f.get("value"));
    }
    n
}

/// The result of [`validate_template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub ok: bool,
    pub errors: Vec<String>,
}

/// `validateTemplate(tpl, { requireSomething })`: the raw (pre-render) lengths and URLs against Discord's limits.
pub fn validate_template(tpl: &Value, require_something: bool) -> Validation {
    let mut errors = Vec::new();
    let t = normalize_template(tpl);
    if let Some(c) = t["content"].as_str()
        && len(c) > limits::CONTENT
    {
        errors.push(format!("Message content is {} characters (max {}).", len(c), limits::CONTENT));
    }
    let e = &t["embed"];
    if e.is_object() {
        let check = |errors: &mut Vec<String>, v: Option<&Value>, label: &str, max: usize| {
            if let Some(s) = v.and_then(Value::as_str).filter(|s| !s.is_empty())
                && len(s) > max
            {
                errors.push(format!("{label} is {} characters (max {max}).", len(s)));
            }
        };
        check(&mut errors, e.get("title"), "Title", limits::TITLE);
        check(&mut errors, e.get("description"), "Description", limits::DESCRIPTION);
        check(&mut errors, e.get("author").and_then(|a| a.get("name")), "Author name", limits::AUTHOR);
        check(&mut errors, e.get("footer").and_then(|f| f.get("text")), "Footer", limits::FOOTER);
        let fields = e.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
        if fields.len() > limits::FIELDS {
            errors.push(format!("Embed has {} fields (max {}).", fields.len(), limits::FIELDS));
        }
        for (idx, f) in fields.iter().enumerate() {
            let (n, v) = (s_len(f.get("name")), s_len(f.get("value")));
            if n > limits::FIELD_NAME {
                errors.push(format!("Field {} name is {n} characters (max {}).", idx + 1, limits::FIELD_NAME));
            }
            if v > limits::FIELD_VALUE {
                errors.push(format!("Field {} value is {v} characters (max {}).", idx + 1, limits::FIELD_VALUE));
            }
        }
        let total = embed_text_length(e);
        if total > limits::TOTAL {
            errors.push(format!("Embed text totals {total} characters (max {}).", limits::TOTAL));
        }
        if let Some(c) = e.get("color").filter(|c| !c.is_null())
            && int24(c).is_none()
        {
            errors.push("Colour must be a hex value like #5865F2.".into());
        }
        let checks: [(&str, Option<&Value>, bool); 6] = [
            ("Embed URL", e.get("url"), false),
            ("Author URL", e.get("author").and_then(|a| a.get("url")), false),
            ("Author icon", e.get("author").and_then(|a| a.get("iconUrl")), true),
            ("Footer icon", e.get("footer").and_then(|f| f.get("iconUrl")), true),
            ("Thumbnail", e.get("thumbnail"), true),
            ("Image", e.get("image"), true),
        ];
        for (label, v, image_like) in checks {
            let Some(v) = v.filter(|v| !v.is_null()) else { continue };
            if let Some(s) = v.as_str()
                && len(s) > limits::URL
            {
                errors.push(format!("{label} is longer than {} characters.", limits::URL));
                continue;
            }
            let s = js_string(v);
            let ok = v.as_str().is_some_and(|s| is_http_url(s) || is_placeholder_url(s)) || (image_like && is_attachment_url(&s));
            if !ok {
                errors.push(format!("{label} must be an http(s) URL."));
            }
        }
    }
    let buttons = t["buttons"].as_array().cloned().unwrap_or_default();
    if buttons.len() > limits::BUTTONS {
        errors.push(format!("Too many link buttons ({}, max {}).", buttons.len(), limits::BUTTONS));
    }
    for (idx, b) in buttons.iter().enumerate() {
        let i = idx + 1;
        let label = b["label"].as_str().unwrap_or("");
        if label.is_empty() {
            errors.push(format!("Button {i} needs a label."));
        } else if len(label) > limits::BUTTON_LABEL {
            errors.push(format!("Button {i} label is {} characters (max {}).", len(label), limits::BUTTON_LABEL));
        }
        let url = b["url"].as_str().unwrap_or("");
        if len(url) > limits::BUTTON_URL {
            errors.push(format!("Button {i} URL is {} characters (max {}).", len(url), limits::BUTTON_URL));
        } else if !(is_http_url(url) || is_placeholder_url(url)) {
            errors.push(format!("Button {i} URL must be an http(s) URL."));
        }
        if let Some(em) = b["emoji"].as_str().filter(|s| !s.is_empty())
            && parse_button_emoji(em).is_none()
        {
            errors.push(format!("Button {i} emoji is not a valid emoji."));
        }
    }
    let has_content = t["content"].as_str().is_some_and(|c| !is_blank(c));
    if require_something && !has_content && !is_visible(e) && buttons.is_empty() {
        errors.push(EMPTY_MESSAGE_ERROR.into());
    }
    Validation { ok: errors.is_empty(), errors }
}

// ── Rendering ──────────────────────────────────────────────────────────────────

/// `renderEmbed(spec, ctx, { defaultColor })`: placeholders expanded, every part clamped after expansion, URLs that
/// do not resolve to http(s) dropped, and the 6000-character total enforced (the description shrinks first, then
/// field values from the last field, then fields are dropped). `default_color` is used when the spec has none
/// (`None` omits the colour). `None` when nothing visible remains. The result is the embed JSON (`toJSON()`).
pub fn render_embed(spec: &Value, ctx: &RenderContext, default_color: Option<u32>) -> Option<Value> {
    let s = normalize_legacy_embed(spec);
    let r = |t: &str, max: usize| clamp_text(&render_placeholders(t, ctx, None), max);
    let u = |t: &str| js_trim(&render_placeholders(t, ctx, None)).to_string();
    let get = |k: &str| s.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());

    let mut data = Map::new();
    let title = get("title").map(|t| r(t, limits::TITLE)).unwrap_or_default();
    if !title.is_empty() {
        data.insert("title".into(), json!(title));
    }
    let mut description = get("description").map(|t| r(t, limits::DESCRIPTION)).unwrap_or_default();
    if let Some(url) = get("url") {
        let v = u(url);
        if is_http_url(&v) {
            data.insert("url".into(), json!(v));
        }
    }
    let mut author: Option<Map<String, Value>> = None;
    if let Some(name) = s.get("author").and_then(|a| a.get("name")).and_then(Value::as_str).filter(|n| !n.is_empty()) {
        let name = r(name, limits::AUTHOR);
        if !name.is_empty() {
            let mut a = Map::new();
            a.insert("name".into(), json!(name));
            if let Some(icon) = s["author"].get("iconUrl").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                let v = u(icon);
                if image_url_ok(&v) {
                    a.insert("icon_url".into(), json!(v));
                }
            }
            if let Some(link) = s["author"].get("url").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                let v = u(link);
                if is_http_url(&v) {
                    a.insert("url".into(), json!(v));
                }
            }
            author = Some(a);
        }
    }
    let mut footer: Option<Map<String, Value>> = None;
    if let Some(t) = s.get("footer").and_then(|f| f.get("text")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
        let t = r(t, limits::FOOTER);
        if !t.is_empty() {
            let mut f = Map::new();
            f.insert("text".into(), json!(t));
            if let Some(icon) = s["footer"].get("iconUrl").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                let v = u(icon);
                if image_url_ok(&v) {
                    f.insert("icon_url".into(), json!(v));
                }
            }
            footer = Some(f);
        }
    }
    if let Some(t) = get("thumbnail") {
        let v = u(t);
        if image_url_ok(&v) {
            data.insert("thumbnail".into(), json!({ "url": v }));
        }
    }
    if let Some(t) = get("image") {
        let v = u(t);
        if image_url_ok(&v) {
            data.insert("image".into(), json!({ "url": v }));
        }
    }
    let mut fields: Vec<(String, String, bool)> = Vec::new();
    for f in s.get("fields").and_then(Value::as_array).into_iter().flatten().take(limits::FIELDS) {
        let mut name = r(f["name"].as_str().unwrap_or(""), limits::FIELD_NAME);
        let mut value = r(f["value"].as_str().unwrap_or(""), limits::FIELD_VALUE);
        if name.is_empty() && value.is_empty() {
            continue;
        }
        if name.is_empty() {
            name = "\u{200B}".into();
        }
        if value.is_empty() {
            value = "\u{200B}".into();
        }
        fields.push((name, value, f["inline"].as_bool().unwrap_or(false)));
    }

    let total = |description: &str, fields: &[(String, String, bool)]| {
        len(&title)
            + len(description)
            + author.as_ref().map(|a| s_len(a.get("name"))).unwrap_or(0)
            + footer.as_ref().map(|f| s_len(f.get("text"))).unwrap_or(0)
            + fields.iter().map(|(n, v, _)| len(n) + len(v)).sum::<usize>()
    };
    let mut excess = total(&description, &fields) as i64 - limits::TOTAL as i64;
    if excess > 0 && !description.is_empty() {
        let new_len = len(&description) as i64 - excess;
        description = if new_len >= 1 { clamp_text(&description, new_len as usize) } else { String::new() };
        excess = total(&description, &fields) as i64 - limits::TOTAL as i64;
    }
    let mut i = fields.len();
    while i > 0 && excess > 0 {
        i -= 1;
        let target = (len(&fields[i].1) as i64 - excess).max(1) as usize;
        fields[i].1 = clamp_text(&fields[i].1, target);
        excess = total(&description, &fields) as i64 - limits::TOTAL as i64;
    }
    while excess > 0 && !fields.is_empty() {
        fields.pop();
        excess = total(&description, &fields) as i64 - limits::TOTAL as i64;
    }

    if !description.is_empty() {
        data.insert("description".into(), json!(description));
    }
    if let Some(a) = author {
        data.insert("author".into(), Value::Object(a));
    }
    if let Some(f) = footer {
        data.insert("footer".into(), Value::Object(f));
    }
    if !fields.is_empty() {
        data.insert("fields".into(), Value::Array(fields.into_iter().map(|(n, v, i)| json!({ "name": n, "value": v, "inline": i })).collect()));
    }
    let visible = ["title", "description", "fields", "author", "footer", "image", "thumbnail"].iter().any(|k| data.contains_key(*k));
    if !visible {
        return None;
    }
    let color = if is_integer(s.get("color")) { s["color"].as_f64().map(|c| c as u32) } else { default_color };
    if let Some(c) = color {
        data.insert("color".into(), json!(if c == 0 { BLACK } else { c }));
    }
    if s.get("timestamp").is_some_and(|t| truthy(Some(t))) {
        data.insert("timestamp".into(), json!(bot_util::js::to_iso_string(bot_util::js::now_ms()).unwrap_or_default()));
    }
    Some(Value::Object(data))
}

/// An embed JSON (Discord's shape) as a serenity builder.
pub fn embed_from_json(v: &Value) -> CreateEmbed {
    match serde_json::from_value::<serenity::all::Embed>(v.clone()) {
        Ok(e) => CreateEmbed::from(e),
        Err(_) => CreateEmbed::new(),
    }
}

/// A rendered template (`{ content?, embeds, components, allowedMentions }`).
#[derive(Debug, Clone)]
pub struct Rendered {
    pub content: Option<String>,
    /// Embed JSON (zero or one).
    pub embeds: Vec<Value>,
    /// The link-button rows as JSON (`{ type: 1, components: [...] }`).
    pub link_rows: Vec<Value>,
    /// `extraComponents`, after the link rows.
    pub extra: Vec<CreateActionRow>,
    pub allowed_mentions: CreateAllowedMentions,
}

impl Default for Rendered {
    /// An empty render: no content, no embeds, no rows, default mentions (a fallback value).
    fn default() -> Self {
        Self { content: None, embeds: Vec::new(), link_rows: Vec::new(), extra: Vec::new(), allowed_mentions: CreateAllowedMentions::new() }
    }
}

impl Rendered {
    /// An empty render (see [`Default`]).
    pub fn empty() -> Self {
        Self::default()
    }

    /// The rows as serenity builders: link rows, then the extras.
    pub fn components(&self) -> Vec<CreateActionRow> {
        let mut rows: Vec<CreateActionRow> = self
            .link_rows
            .iter()
            .map(|row| {
                let buttons = row["components"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|b| {
                        let mut btn = CreateButton::new_link(b["url"].as_str().unwrap_or_default()).label(b["label"].as_str().unwrap_or_default());
                        if let Some(e) = b.get("emoji").and_then(reaction_of) {
                            btn = btn.emoji(e);
                        }
                        btn
                    })
                    .collect();
                CreateActionRow::Buttons(buttons)
            })
            .collect();
        rows.extend(self.extra.iter().cloned());
        rows
    }

    /// The message payload: `embeds` and `components` are always set, so sending it as an edit removes stale ones.
    /// To also clear removed text when editing, set `content` to `""` first (`message.edit({ content: '', ... })`).
    pub fn to_payload(&self) -> Payload {
        let mut p = Payload::new()
            .embeds(self.embeds.iter().map(embed_from_json).collect())
            .components(self.components())
            .allowed_mentions(self.allowed_mentions.clone());
        if let Some(c) = &self.content {
            p = p.content(c.clone());
        }
        p
    }

    /// The JSON the JS returns (embeds and components serialised), for comparisons.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(c) = &self.content {
            m.insert("content".into(), json!(c));
        }
        m.insert("embeds".into(), Value::Array(self.embeds.clone()));
        let mut comps = self.link_rows.clone();
        comps.extend(self.extra.iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)));
        m.insert("components".into(), Value::Array(comps));
        Value::Object(m)
    }
}

/// `renderTemplate` options.
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    /// `defaultColor` (`null`/absent: `COLORS.INFO`).
    pub default_color: Option<u32>,
    /// `allowedMentions` (default `{ parse: ['users'] }`), used verbatim.
    pub allowed_mentions: Option<CreateAllowedMentions>,
    /// `extraComponents` (rows of select menus ...): after the link rows; link rows + extras never exceed 5 rows.
    pub extra_components: Vec<CreateActionRow>,
}

/// `renderTemplate(tpl, ctx, opts)`: the message payload, or `None` when there is no content, no visible embed and no
/// components. Link buttons whose rendered URL is not http(s) (or over 512 characters) are dropped; a button emoji is
/// used only when it parses.
pub fn render_template(tpl: &Value, ctx: &RenderContext, opts: RenderOptions) -> Option<Rendered> {
    let t = normalize_template(tpl);
    let content = t["content"].as_str().map(|c| clamp_text(&render_placeholders(c, ctx, None), limits::CONTENT)).filter(|c| !is_blank(c));
    let embed = if t["embed"].is_object() { render_embed(&t["embed"], ctx, Some(opts.default_color.unwrap_or(colors::INFO))) } else { None };
    let mut buttons = Vec::new();
    for b in t["buttons"].as_array().into_iter().flatten() {
        let label = clamp_text(js_trim(&render_placeholders(b["label"].as_str().unwrap_or(""), ctx, None)), limits::BUTTON_LABEL);
        let url = js_trim(&render_placeholders(b["url"].as_str().unwrap_or(""), ctx, None)).to_string();
        if label.is_empty() || !is_http_url(&url) || len(&url) > limits::BUTTON_URL {
            continue;
        }
        let mut c = json!({ "type": 2, "style": 5, "label": label, "url": url });
        if let Some(e) = b["emoji"].as_str().filter(|s| !s.is_empty()).and_then(parse_button_emoji) {
            c["emoji"] = e;
        }
        buttons.push(c);
    }
    let allowed_rows = MAX_ROWS.saturating_sub(opts.extra_components.len());
    let mut link_rows: Vec<Value> = buttons.chunks(BUTTONS_PER_ROW).map(|chunk| json!({ "type": 1, "components": chunk })).collect();
    if link_rows.len() > allowed_rows {
        bot_util::log_warn!("Template has more link buttons than fit; extra buttons dropped");
        link_rows.truncate(allowed_rows);
    }
    if content.is_none() && embed.is_none() && link_rows.is_empty() && opts.extra_components.is_empty() {
        return None;
    }
    Some(Rendered {
        content,
        embeds: embed.into_iter().collect(),
        link_rows,
        extra: opts.extra_components,
        allowed_mentions: opts.allowed_mentions.unwrap_or_else(|| CreateAllowedMentions::new().all_users(true)),
    })
}

// ── Modal ──────────────────────────────────────────────────────────────────────

/// `MODAL_INPUTS`: key, label, paragraph style, placeholder.
pub const MODAL_INPUTS: [(&str, &str, bool, &str); 5] = [
    ("content", "Message text (outside the embed)", true, "Hey {user}, welcome to {server}!"),
    ("title", "Embed title", false, "Welcome to {server}!"),
    ("description", "Embed description", true, "Use {user}, {server}, {server.member_count} …"),
    ("color", "Embed colour (hex or name)", false, "#5865F2"),
    ("footer", "Embed footer", true, "{server.member_count} members"),
];

/// `buildTemplateModal`'s `overflow: 'throw'` error: the over-long keys.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Too long for the form: {}.", fields.join(", "))]
pub struct TooLongForForm {
    pub fields: Vec<String>,
}

fn prefill(t: &Value) -> [(&'static str, Option<String>); 5] {
    let e = &t["embed"];
    let color = e.get("color").filter(|c| is_integer(Some(c)) && int24(c).is_some()).map(format_color);
    [
        ("content", t["content"].as_str().map(String::from)),
        ("title", e.get("title").and_then(Value::as_str).map(String::from)),
        ("description", e.get("description").and_then(Value::as_str).map(String::from)),
        ("color", color),
        ("footer", e.get("footer").and_then(|f| f.get("text")).and_then(Value::as_str).map(String::from)),
    ]
}

/// `buildTemplateModal({ customId, title, template, overflow })` as JSON (the modal Discord receives): five optional
/// inputs prefilled from the template. With `throw_on_overflow` an over-long prefill is an error.
pub fn template_modal_json(custom_id: &str, title: Option<&str>, template: &Value, throw_on_overflow: bool) -> Result<Value, TooLongForForm> {
    let t = normalize_template(template);
    let pre = prefill(&t);
    if throw_on_overflow {
        let over: Vec<String> = pre.iter().filter(|(k, v)| v.as_ref().is_some_and(|s| len(s) > input_max(k))).map(|(k, _)| k.to_string()).collect();
        if !over.is_empty() {
            return Err(TooLongForForm { fields: over });
        }
    }
    let rows: Vec<Value> = MODAL_INPUTS
        .iter()
        .map(|(key, label, paragraph, placeholder)| {
            let mut input = json!({
                "type": 4, "custom_id": field_id(key), "label": label, "style": if *paragraph { 2 } else { 1 },
                "max_length": input_max(key), "required": false, "placeholder": placeholder,
            });
            if let Some(v) = pre.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.clone()).filter(|v| !v.is_empty()) {
                input["value"] = json!(utf16_prefix(&v, input_max(key)));
            }
            json!({ "type": 1, "components": [input] })
        })
        .collect();
    let title = clamp_text(title.filter(|t| !t.is_empty()).unwrap_or("Edit message"), 45);
    Ok(json!({ "custom_id": custom_id, "title": title, "components": rows }))
}

/// `buildTemplateModal(...)` as a serenity modal (see [`template_modal_json`]).
pub fn build_template_modal(custom_id: &str, title: Option<&str>, template: &Value, throw_on_overflow: bool) -> Result<CreateModal, TooLongForForm> {
    let json = template_modal_json(custom_id, title, template, throw_on_overflow)?;
    let rows = json["components"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let i = &row["components"][0];
            let style = if i["style"] == 2 { InputTextStyle::Paragraph } else { InputTextStyle::Short };
            let mut input = CreateInputText::new(style, i["label"].as_str().unwrap_or_default(), i["custom_id"].as_str().unwrap_or_default())
                .max_length(i["max_length"].as_u64().unwrap_or(0) as u16)
                .required(false)
                .placeholder(i["placeholder"].as_str().unwrap_or_default());
            if let Some(v) = i["value"].as_str() {
                input = input.value(v);
            }
            CreateActionRow::InputText(input)
        })
        .collect();
    Ok(CreateModal::new(json["custom_id"].as_str().unwrap_or_default(), json["title"].as_str().unwrap_or_default()).components(rows))
}

const OUT_OF_DATE: &str = "This form is out of date. Open it again.";

/// `parseTemplateModal(interaction, base, { requireSomething })`: merges a submitted template modal into a copy of
/// `base`. `get(field_id)` is `fields.getTextInputValue` (`None`: the input is missing, a stale form). An empty input
/// clears that part; a missing one is an error and leaves it unchanged. The template is returned even with errors.
pub fn parse_template_modal(get: impl Fn(&str) -> Option<String>, base: &Value, require_something: bool) -> (Value, Vec<String>) {
    let mut errors = Vec::new();
    let base_tpl = normalize_template(base);
    let mut tpl = base_tpl.clone();
    let mut embed: Map<String, Value> = tpl["embed"].as_object().cloned().unwrap_or_default();
    let mut stale = false;
    let mut read = |key: &str| -> Option<String> {
        let v = get(field_id(key));
        if v.is_none() {
            stale = true;
        }
        v
    };

    let content = read("content");
    let title = read("title");
    let description = read("description");
    let color = read("color");
    let footer = read("footer");

    if let Some(c) = content {
        tpl["content"] = if is_blank(&c) { Value::Null } else { Value::String(c) };
    }
    if let Some(t) = title {
        if is_blank(&t) {
            embed.shift_remove("title");
        } else {
            embed.insert("title".into(), Value::String(t));
        }
    }
    if let Some(d) = description {
        let original = base_tpl["embed"].get("description").and_then(Value::as_str).filter(|o| !o.is_empty());
        let max = input_max("description");
        if let Some(o) = original.filter(|o| len(o) > max && d == utf16_prefix(o, max)) {
            // A long description that was only prefilled (cut to 4000) is not silently truncated on save.
            embed.insert("description".into(), Value::String(o.to_string()));
        } else if !is_blank(&d) {
            embed.insert("description".into(), Value::String(d));
        } else {
            embed.shift_remove("description");
        }
    }
    if let Some(c) = color {
        if js_trim(&c).is_empty() {
            embed.shift_remove("color");
        } else {
            match parse_color_str(&c) {
                Some(v) => {
                    embed.insert("color".into(), json!(v));
                }
                None => errors.push("Colour must be a hex value like #5865F2 or a colour name.".into()),
            }
        }
    }
    if let Some(f) = footer {
        if is_blank(&f) {
            embed.shift_remove("footer");
        } else {
            let icon = embed.get("footer").and_then(|x| x.get("iconUrl")).cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null);
            embed.insert("footer".into(), json!({ "text": f, "iconUrl": icon }));
        }
    }
    if stale {
        errors.insert(0, OUT_OF_DATE.into());
    }
    let embed = Value::Object(embed);
    tpl["embed"] = if is_visible(&embed) || is_integer(embed.get("color")) { embed } else { Value::Null };
    errors.extend(validate_template(&tpl, require_something).errors);
    (tpl, errors)
}

// ── JSON ───────────────────────────────────────────────────────────────────────

/// `templateToJson(tpl)`: pretty JSON (2 spaces) of the normalised template.
pub fn template_to_json(tpl: &Value) -> String {
    serde_json::to_string_pretty(&normalize_template(tpl)).unwrap_or_default()
}

/// `templateFromJson(str)`: imports a template from JSON (our export, a bare EmbedSpec, the legacy welcome shape, or
/// Discord / Discohook message JSON). Over 20 KB is refused. `(template, errors)`: no template when there are errors.
pub fn template_from_json(s: &str) -> (Option<Value>, Vec<String>) {
    if s.len() > MAX_JSON_BYTES {
        return (None, vec!["The JSON is larger than 20 KB.".into()]);
    }
    let parsed: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return (None, vec![format!("That is not valid JSON: {}", clamp_text(&e.to_string(), 200))]),
    };
    let template = normalize_template(&parsed);
    let errors = validate_template(&template, true).errors;
    if errors.is_empty() { (Some(template), errors) } else { (None, errors) }
}
