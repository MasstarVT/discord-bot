//! The dashboard's settings registry (src/dashboard/fields.js): every GuildSettings value an admin may edit, as field
//! descriptors grouped per module the way the dashboard shows them, plus the reading, validation, cross-field and
//! storage rules the slash commands apply to the same values.
//!
//! The descriptors, groups, column map and restated constants are generated from the JS
//! (`node rust/fixtures/gen/dash-schema-golden.mjs` writes `fields_data.json`, embedded here). The server-only hooks
//! the JS attaches to some descriptors are restated in Rust ([`hook_read`], [`hook_check`], [`hook_normalize`],
//! [`hook_write`]); the tests check that exactly the JS's hooks exist:
//!   read(row)                 the stored value of this field in a GuildSettings row (default: row[field])
//!   check(value)              None, a message or { path, message } (path relative to the field), after validate_value
//!   normalize(value)          the value to store, after check
//!   write(value, draft, row)  puts the value into `draft` (the columns to write); default: draft[field] = value
//! A field whose name is not a column (the automod rules, the captcha verify button) is "virtual": its write merges it
//! into the JSON column named in [`Registry::settings_columns`].
//!
//! A GuildSettings row is passed as JSON: the object Prisma returns (camelCase column names, dates as ISO strings),
//! which is what `serde_json::to_value(&bot_db::models::GuildSettings)` gives ([`row_json`]).

use std::collections::HashSet;
use std::sync::LazyLock;

use bot_core::custom_embed::normalize_template;
use bot_util::js::{trim as js_trim, utf16_len, utf16_prefix};
use bot_util::jsre::JsRegex;
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders_detailed};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::js::{canon, is_integer, js_keys, js_num, non_blank, round2, strict_eq, to_js_string_or_empty, to_number};
use crate::schema::{
    Field, FieldError, Refs, StoredRefs, collect_stored_refs, is_snowflake, read_value, stored_ref_keys, validate_value,
};

// ── The generated data ──────────────────────────────────────────────────────────────────────────────────────

/// A group of fields on a dashboard page (`{ id, title, description?, fields }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub fields: Vec<Field>,
}

impl Group {
    /// The group as the catalog serves it (`publicGroup` in catalog.js): id, title, description when set, public
    /// field descriptors.
    pub fn public_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), Value::String(self.id.clone()));
        out.insert("title".into(), Value::String(self.title.clone()));
        if let Some(d) = self.description.as_ref().filter(|d| !d.is_empty()) {
            out.insert("description".into(), Value::String(d.clone()));
        }
        out.insert("fields".into(), Value::Array(self.fields.iter().map(Field::public_json).collect()));
        Value::Object(out)
    }
}

/// One choice of an enum-like constant (`{ value, label }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

/// The threshold of an automod rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Threshold {
    pub min: i64,
    pub max: i64,
    pub default: i64,
    pub unit: String,
}

/// One automod rule (`AUTOMOD_RULES[key]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomodRule {
    pub label: String,
    pub threshold: Option<Threshold>,
    pub description: String,
}

/// `NUMBER_LIMITS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberLimits {
    pub xp_range_max: i64,
    pub economy_range_max: i64,
    pub work_cooldown_max_minutes: i64,
    pub autorole_delay_max_seconds: i64,
    pub captcha_timeout_min_minutes: i64,
    pub captcha_timeout_max_minutes: i64,
    /// 28 days, Discord's timeout maximum.
    pub timeout_max_seconds: i64,
    /// 365 days.
    pub timed_action_max_seconds: i64,
}

/// `AUTOMOD_DEFAULTS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomodDefaults {
    pub allowed_domains: Vec<String>,
    pub timeout_seconds: i64,
    pub min_timeout_seconds: i64,
    pub max_timeout_seconds: i64,
}

/// `AUTOMOD_HONEYPOT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomodHoneypot {
    pub actions: Vec<Choice>,
    pub timeout_seconds: i64,
    pub max_channels: i64,
}

/// The constants fields.js restates from the bot's services (each names its original there).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct Constants {
    /// Most fields one PATCH may change.
    pub max_settings_changes: usize,
    /// Default texts used while a message column is null (field -> text).
    pub default_texts: IndexMap<String, String>,
    /// DEFAULT_WELCOME_EMBED / DEFAULT_GOODBYE_EMBED (field -> EmbedSpec).
    pub default_embeds: IndexMap<String, Value>,
    /// defaultPanelTemplate() of captchaService.
    pub default_captcha_panel: Value,
    /// DEFAULT_VERIFY_BUTTON of captchaService.
    pub default_verify_button: Value,
    /// DEFAULT_RANK_CARD_STYLE of profileCard.
    pub default_rank_card_style: Value,
    pub number_limits: NumberLimits,
    /// The automod rule registry, in RULE_KEYS order.
    pub automod_rules: IndexMap<String, AutomodRule>,
    pub automod_actions: Vec<Choice>,
    pub automod_defaults: AutomodDefaults,
    pub automod_honeypot: AutomodHoneypot,
    /// LOG_EVENTS: key -> label, in display order.
    pub log_events: IndexMap<String, String>,
    pub tier_actions: Vec<Choice>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawData {
    settings_field_order: Vec<String>,
    flag_fields: Vec<Value>,
    general_groups: Vec<Value>,
    module_groups: IndexMap<String, Vec<Value>>,
    hooks: IndexMap<String, Vec<String>>,
    settings_columns: IndexMap<String, String>,
    json_columns: Vec<String>,
    not_exposed: IndexMap<String, String>,
    constants: Constants,
}

/// The whole registry: groups, fields, columns and constants.
#[derive(Debug)]
pub struct Registry {
    /// The groups of the General page.
    pub general_groups: Vec<Group>,
    /// MODULES key -> groups (every module, in MODULE_NAMES order; a module with nothing but its flag has none).
    pub module_groups: IndexMap<String, Vec<Group>>,
    /// field -> descriptor for every field the dashboard reads and writes (module flags, general, module groups), in
    /// the JS order of `SETTINGS_FIELDS`.
    pub settings_fields: IndexMap<String, Field>,
    /// field -> the GuildSettings column it is stored in (a virtual field names its JSON column, logEvents its
    /// inverse list).
    pub settings_columns: IndexMap<String, String>,
    /// Columns stored as Json (null clears them with SQL NULL).
    pub json_columns: Vec<String>,
    /// GuildSettings columns the dashboard never writes, with the reason.
    pub not_exposed: IndexMap<String, String>,
    /// The hooks the JS descriptors carry (field -> names), as generated; [`hook_names`] must agree.
    pub js_hooks: IndexMap<String, Vec<String>>,
    pub constants: Constants,
}

fn group_from(v: &Value) -> Group {
    Group {
        id: v["id"].as_str().unwrap_or_default().to_string(),
        title: v["title"].as_str().unwrap_or_default().to_string(),
        description: v.get("description").and_then(Value::as_str).map(String::from),
        fields: v["fields"].as_array().into_iter().flatten().filter_map(|f| Field::from_json(f.clone()).ok()).collect(),
    }
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    let raw: RawData = serde_json::from_str(include_str!("fields_data.json")).expect("fields_data.json is generated and valid");
    let general_groups: Vec<Group> = raw.general_groups.iter().map(group_from).collect();
    let module_groups: IndexMap<String, Vec<Group>> =
        raw.module_groups.iter().map(|(k, gs)| (k.clone(), gs.iter().map(group_from).collect())).collect();
    let mut all: IndexMap<String, Field> = IndexMap::new();
    let flags = raw.flag_fields.iter().filter_map(|f| Field::from_json(f.clone()).ok());
    let grouped = general_groups.iter().chain(module_groups.values().flatten()).flat_map(|g| g.fields.iter().cloned());
    for f in flags.chain(grouped) {
        all.insert(f.field().to_string(), f);
    }
    let settings_fields = raw.settings_field_order.iter().filter_map(|k| all.get(k).map(|f| (k.clone(), f.clone()))).collect();
    Registry {
        general_groups,
        module_groups,
        settings_fields,
        settings_columns: raw.settings_columns,
        json_columns: raw.json_columns,
        not_exposed: raw.not_exposed,
        js_hooks: raw.hooks,
        constants: raw.constants,
    }
});

/// The registry (built once from the embedded data).
pub fn registry() -> &'static Registry {
    &REGISTRY
}

/// The descriptor of a settings field (`SETTINGS_FIELDS[field]`).
pub fn settings_field(field: &str) -> Option<&'static Field> {
    REGISTRY.settings_fields.get(field)
}

/// The groups of a module page (`MODULE_GROUPS[key]`); `None` for an unknown module.
pub fn module_groups(key: &str) -> Option<&'static [Group]> {
    REGISTRY.module_groups.get(key).map(Vec::as_slice)
}

/// The groups of the General page (`GENERAL_GROUPS`).
pub fn general_groups() -> &'static [Group] {
    &REGISTRY.general_groups
}

/// The GuildSettings column a field is stored in (`SETTINGS_COLUMNS[field]`).
pub fn settings_column(field: &str) -> Option<&'static str> {
    REGISTRY.settings_columns.get(field).map(String::as_str)
}

/// True for a column stored as Json (`JSON_COLUMNS`).
pub fn is_json_column(column: &str) -> bool {
    REGISTRY.json_columns.iter().any(|c| c == column)
}

/// The restated constants.
pub fn constants() -> &'static Constants {
    &REGISTRY.constants
}

/// `MAX_SETTINGS_CHANGES`.
pub fn max_settings_changes() -> usize {
    REGISTRY.constants.max_settings_changes
}

/// `AUTOMOD_RULE_KEYS`.
pub fn automod_rule_keys() -> Vec<&'static str> {
    REGISTRY.constants.automod_rules.keys().map(String::as_str).collect()
}

/// `DEFAULT_TEXTS[field]`.
pub fn default_text(field: &str) -> Option<&'static str> {
    REGISTRY.constants.default_texts.get(field).map(String::as_str)
}

/// A GuildSettings row as the JSON these functions read (what Prisma returns).
pub fn row_json(row: &bot_db::models::GuildSettings) -> Value {
    serde_json::to_value(row).unwrap_or(Value::Null)
}

fn limits() -> &'static NumberLimits {
    &REGISTRY.constants.number_limits
}

fn log_event_keys() -> impl Iterator<Item = &'static String> {
    REGISTRY.constants.log_events.keys()
}

// ── Small pure helpers ──────────────────────────────────────────────────────────────────────────────────────

/// `row?.[key]` (`None` is undefined).
fn get<'a>(row: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    row.and_then(Value::as_object).and_then(|o| o.get(key))
}

fn obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// `embedHasContent(spec)` (hasEmbedContent in welcomeService): true when an EmbedSpec would show something.
pub fn embed_has_content(spec: &Value) -> bool {
    let Some(s) = spec.as_object() else { return false };
    non_blank(s.get("title"))
        || non_blank(s.get("description"))
        || s.get("fields").and_then(Value::as_array).is_some_and(|f| !f.is_empty())
        || non_blank(obj(s.get("author")).and_then(|a| a.get("name")))
        || non_blank(s.get("image"))
}

/// The EmbedSpec stored in a column (any stored shape), or null.
fn stored_embed(raw: Option<&Value>) -> Value {
    match raw {
        Some(v @ Value::Object(_)) => {
            normalize_template(&json!({ "content": null, "embed": v, "buttons": [] })).get("embed").cloned().unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

/// Only the keys that carry a value, nested objects too (serializeEmbed in welcomeService).
fn serialize_embed(spec: &Value) -> Value {
    let present = |v: &Value| !(v.is_null() || v.as_str() == Some(""));
    let copy = |o: &Value| -> Map<String, Value> {
        o.as_object().map(|m| m.iter().filter(|(_, v)| present(v)).map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()
    };
    let mut out = Map::new();
    for key in ["title", "description", "url", "color", "author", "footer", "thumbnail", "image", "fields", "timestamp"] {
        let Some(v) = spec.get(key) else { continue };
        if !present(v) || v == &Value::Bool(false) {
            continue;
        }
        match key {
            "author" | "footer" => {
                let nested = copy(v);
                if !nested.is_empty() {
                    out.insert(key.into(), Value::Object(nested));
                }
            }
            "fields" => {
                if let Some(list) = v.as_array().filter(|l| !l.is_empty()) {
                    out.insert(key.into(), Value::Array(list.iter().map(|f| Value::Object(copy(f))).collect()));
                }
            }
            _ => {
                out.insert(key.into(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// An embed to store: null when it shows nothing (the bot then uses its default, or no embed).
fn embed_to_stored(spec: &Value) -> Value {
    if embed_has_content(spec) { serialize_embed(spec) } else { Value::Null }
}

fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0xAD | 0x34F | 0x61C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
}

static LETTER_OR_DIGIT: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"[\p{L}\p{N}]", "u"));

/// Replaces every run of JS whitespace with one space (`.replace(/\s+/g, ' ')`).
fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `normalizeAutomodWord` (normalizeWord in automodService): NFKC, invisible characters removed, lower-cased, 2-64
/// characters with a letter or digit. `input` `None` is undefined.
pub fn normalize_automod_word(input: Option<&Value>) -> Option<String> {
    normalize_automod_word_str(&to_js_string_or_empty(input))
}

/// [`normalize_automod_word`] for a string.
pub fn normalize_automod_word_str(input: &str) -> Option<String> {
    let nfkc: String = input.nfkc().filter(|c| !is_invisible(*c)).collect();
    let s = collapse_spaces(&bot_util::js::lower(js_trim(&nfkc)));
    let n = utf16_len(&s);
    if !(2..=64).contains(&n) || !LETTER_OR_DIGIT.test(&s) {
        return None;
    }
    Some(s)
}

static DOMAIN_RE: LazyLock<JsRegex> =
    LazyLock::new(|| JsRegex::new(r"^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:[a-z]{2,63}|xn--[a-z0-9-]{1,59})$", ""));
static PORT_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r":\d+$", ""));

/// Node's `domainToASCII`: the WHATWG host parser's ASCII form, '' when the host is invalid.
fn domain_to_ascii(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    url::Host::parse(s).map(|h| h.to_string()).unwrap_or_default()
}

/// `normalizeAutomodDomain` (normalizeDomain in automodService): the punycode host without www., or null.
pub fn normalize_automod_domain(input: Option<&Value>) -> Option<String> {
    normalize_automod_domain_str(&to_js_string_or_empty(input))
}

/// [`normalize_automod_domain`] for a string.
pub fn normalize_automod_domain_str(input: &str) -> Option<String> {
    let mut s = bot_util::js::lower(js_trim(input));
    if s.is_empty() {
        return None;
    }
    if s.contains("://") {
        let url = url::Url::parse(&s).ok()?;
        s = url.host_str().unwrap_or_default().to_string();
    }
    let head = s.split(['/', '?', '#']).next().unwrap_or_default().to_string();
    let mut s = PORT_RE.replace_all(&head, "");
    if let Some(rest) = s.strip_prefix("*.") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("www.") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_suffix('.') {
        s = rest.to_string();
    }
    let s = domain_to_ascii(&s);
    (DOMAIN_RE.test(&s) && utf16_len(&s) <= 253).then_some(s)
}

/// The /automod patterns rule: a safe regex that matches neither an empty nor a blank message.
fn pattern_problem(p: &str) -> Option<String> {
    if let Err(reason) = bot_util::safe_regex::validate_regex(p, "i") {
        return Some(if reason.is_empty() { "is not a valid pattern.".into() } else { reason });
    }
    let Some(re) = bot_util::safe_regex::compile_safe_regex(p, "i") else {
        return Some("is not a safe pattern.".into());
    };
    if bot_util::safe_regex::safe_regex_test(&re, "", None) || bot_util::safe_regex::safe_regex_test(&re, " ", None) {
        return Some("matches every message.".into());
    }
    None
}

// validateCurrencyEmoji / validateCurrencyName (src/services/economyService.js).
static CURRENCY_CUSTOM_EMOJI_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^<a?:[A-Za-z0-9_]{2,32}:(\d{17,20})>$", ""));
static CURRENCY_UNICODE_EMOJI_RE: LazyLock<JsRegex> = LazyLock::new(|| {
    JsRegex::new(
        r"^(?:\p{Extended_Pictographic}|\p{Regional_Indicator})(?:\p{Extended_Pictographic}|\p{Regional_Indicator}|\p{Emoji_Modifier}|\u{FE0F}|\u{200D}|\u{20E3})*$",
        "u",
    )
});

fn currency_emoji_ok(v: &str) -> bool {
    let n = v.chars().count();
    CURRENCY_CUSTOM_EMOJI_RE.test(v) || ((1..=10).contains(&n) && CURRENCY_UNICODE_EMOJI_RE.test(v))
}

// UTC aliases of normalizeTimeZone (src/commands/admin/settings.js), stored as 'UTC'.
const UTC_ALIASES: [&str; 18] = [
    "utc", "gmt", "uct", "zulu", "universal", "greenwich", "etc/utc", "etc/gmt", "etc/uct", "etc/zulu", "etc/universal",
    "etc/greenwich", "gmt0", "gmt+0", "gmt-0", "etc/gmt0", "etc/gmt+0", "etc/gmt-0",
];

// LEGACY_TZ_NAMES (src/commands/admin/settings.js): ICU's legacy names -> the current tzdata names.
const LEGACY_TZ_NAMES: [(&str, &str); 19] = [
    ("Africa/Asmera", "Africa/Asmara"),
    ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
    ("America/Catamarca", "America/Argentina/Catamarca"),
    ("America/Cordoba", "America/Argentina/Cordoba"),
    ("America/Jujuy", "America/Argentina/Jujuy"),
    ("America/Mendoza", "America/Argentina/Mendoza"),
    ("America/Coral_Harbour", "America/Atikokan"),
    ("America/Godthab", "America/Nuuk"),
    ("America/Indianapolis", "America/Indiana/Indianapolis"),
    ("America/Louisville", "America/Kentucky/Louisville"),
    ("Asia/Calcutta", "Asia/Kolkata"),
    ("Asia/Katmandu", "Asia/Kathmandu"),
    ("Asia/Rangoon", "Asia/Yangon"),
    ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
    ("Atlantic/Faeroe", "Atlantic/Faroe"),
    ("Europe/Kiev", "Europe/Kyiv"),
    ("Pacific/Enderbury", "Pacific/Kanton"),
    ("Pacific/Ponape", "Pacific/Pohnpei"),
    ("Pacific/Truk", "Pacific/Chuuk"),
];

/// `new Intl.DateTimeFormat('en-US', { timeZone }).resolvedOptions().timeZone`, or None when the zone is invalid.
fn resolved_zone(zone: &str) -> Option<String> {
    bot_util::time::canonical_zone(zone)
}

/// The name /settings timezone would store for a valid zone (`canonicalZone`, normalizeTimeZone): canonical,
/// current tzdata spelling.
pub fn canonical_zone(zone: &str) -> String {
    let is_utc = |z: &str| UTC_ALIASES.contains(&bot_util::js::lower(z).as_str());
    if is_utc(zone) {
        return "UTC".into();
    }
    let Some(resolved) = resolved_zone(zone) else { return zone.to_string() };
    if is_utc(&resolved) {
        return "UTC".into();
    }
    match LEGACY_TZ_NAMES.iter().find(|(old, _)| *old == resolved) {
        Some((_, modern)) if resolved_zone(modern).is_some() => (*modern).to_string(),
        _ => resolved,
    }
}

const SAMPLE_ACHIEVEMENT_VARS: [(&str, &str); 10] = [
    ("player", "<@1>"),
    ("player.name", "Member"),
    ("achievement", "Chatterbox"),
    ("achievement.name", "Chatterbox"),
    ("achievement.description", "Send messages"),
    ("tier", "Bronze"),
    ("tier.emoji", "🥉"),
    ("goal", "100"),
    ("progress", "100"),
    ("rewards", ""),
];

// ── Automod (virtual fields over the automodConfig column) ──────────────────────────────────────────────────

/// The automod list fields: field -> key inside automodConfig (`AUTOMOD_LIST_KEYS`).
pub const AUTOMOD_LIST_KEYS: [(&str, &str); 6] = [
    ("automodBadWords", "badWords"),
    ("automodBadWordPatterns", "badWordPatterns"),
    ("automodBlockedDomains", "blockedDomains"),
    ("automodAllowedDomains", "allowedDomains"),
    ("automodExemptRoleIds", "exemptRoleIds"),
    ("automodExemptChannelIds", "exemptChannelIds"),
];

/// The field name of an automod rule (`automodRule<Key>`).
pub fn rule_field_name(key: &str) -> String {
    let mut c = key.chars();
    match c.next() {
        Some(f) => format!("automodRule{}{}", f.to_uppercase(), c.as_str()),
        None => "automodRule".into(),
    }
}

fn rule_key_of(field: &str) -> Option<&'static str> {
    REGISTRY.constants.automod_rules.keys().find(|k| rule_field_name(k) == field).map(String::as_str)
}

fn list_key_of(field: &str) -> Option<&'static str> {
    AUTOMOD_LIST_KEYS.iter().find(|(f, _)| *f == field).map(|(_, k)| *k)
}

const HONEYPOT_FIELD: &str = "automodHoneypot";

/// The entries of a stored list that are valid on their own (one bad entry never hides the rest).
fn read_list(desc: &Field, raw: Option<&Value>) -> Value {
    let Some(list) = raw.and_then(Value::as_array) else { return desc.default_or_null() };
    let one = desc.with("maxItems", json!(1));
    let max = desc.number("maxItems").unwrap_or(f64::INFINITY);
    let mut out: Vec<String> = Vec::new();
    for item in list {
        if let Ok(v) = validate_value(&one, &Value::Array(vec![item.clone()]), Some(desc.field()), &mut Refs::default()) {
            let first = v.as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            let n = match hook_normalize(desc.field(), Value::Array(vec![first.clone()])) {
                Some(norm) => norm.as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null),
                None => first,
            };
            if let Some(s) = n.as_str()
                && !out.iter().any(|o| o == s)
            {
                out.push(s.to_string());
            }
        }
        if out.len() as f64 >= max {
            break;
        }
    }
    Value::Array(out.into_iter().map(Value::String).collect())
}

/// A full v2 automodConfig (`automodConfigOf`; normalizeConfig in automodService upgrades a v1 row first).
pub fn automod_config_of(row: Option<&Value>) -> Value {
    let raw = get(row, "automodConfig");
    let empty = Map::new();
    let src = match raw.and_then(Value::as_object) {
        Some(o) if o.get("version").and_then(Value::as_f64) == Some(2.0) => o,
        _ => &empty,
    };
    let mut rules = Map::new();
    for key in REGISTRY.constants.automod_rules.keys() {
        let Some(desc) = settings_field(&rule_field_name(key)) else { continue };
        let stored = src.get("rules").and_then(Value::as_object).and_then(|r| r.get(key));
        rules.insert(key.clone(), read_value(desc, stored));
    }
    let mut cfg = Map::new();
    cfg.insert("version".into(), json!(2));
    cfg.insert("rules".into(), Value::Object(rules));
    for (field, key) in AUTOMOD_LIST_KEYS {
        let Some(desc) = settings_field(field) else { continue };
        let v = if desc.ty() == "stringList" { read_list(desc, src.get(key)) } else { read_value(desc, src.get(key)) };
        cfg.insert(key.into(), v);
    }
    // A missing allowedDomains key means the defaults; an explicit [] stays empty.
    if !src.get("allowedDomains").is_some_and(Value::is_array) {
        cfg.insert("allowedDomains".into(), json!(REGISTRY.constants.automod_defaults.allowed_domains));
    }
    if let Some(desc) = settings_field(HONEYPOT_FIELD) {
        cfg.insert("honeypot".into(), read_value(desc, src.get("honeypot")));
    }
    Value::Object(cfg)
}

// ── Welcome, captcha ────────────────────────────────────────────────────────────────────────────────────────

/// welcomeCardEnabled + parseCardStyle (welcomeService): the style's own flag, then a legacy welcomeEmbed.card, else
/// on.
fn welcome_card_style_of(row: Option<&Value>) -> Value {
    let empty = Map::new();
    let raw = obj(get(row, "welcomeCardStyle")).unwrap_or(&empty);
    static HEX6: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^#[0-9a-f]{6}$", "i"));
    let color = |v: Option<&Value>| match v.and_then(Value::as_str) {
        Some(s) if HEX6.test(s) => Value::String(s.to_uppercase()),
        _ => Value::Null,
    };
    let enabled = match raw.get("enabled") {
        Some(Value::Bool(b)) => *b,
        _ => match obj(get(row, "welcomeEmbed")) {
            Some(e) => e.get("card") != Some(&Value::Bool(false)),
            None => true,
        },
    };
    let overlay = match raw.get("overlayOpacity").and_then(Value::as_f64) {
        Some(n) if n.is_finite() => js_num(round2(n.clamp(0.0, 1.0))),
        _ => Value::Null,
    };
    let url = match raw.get("backgroundUrl") {
        Some(Value::String(s)) if utf16_len(s) <= 2048 => Value::String(s.clone()),
        _ => Value::Null,
    };
    json!({
        "enabled": enabled,
        "background": color(raw.get("background")),
        "backgroundUrl": url,
        "textColor": color(raw.get("textColor")),
        "accentColor": color(raw.get("accentColor")),
        "overlayOpacity": overlay,
    })
}

/// panelTemplate (captchaService): the stored panel, or the default when it shows nothing.
fn captcha_panel_of(row: Option<&Value>) -> Value {
    if let Some(raw @ Value::Object(_)) = get(row, "captchaTemplate") {
        let t = normalize_template(raw);
        let content = t.get("content").is_some_and(|c| crate::js::truthy(Some(c)));
        let embed = t.get("embed").is_some_and(|e| crate::js::truthy(Some(e)));
        if content || embed {
            return t;
        }
    }
    normalize_template(&REGISTRY.constants.default_captcha_panel)
}

const BUTTON_STYLES: [&str; 4] = ["SUCCESS", "PRIMARY", "SECONDARY", "DANGER"];

/// parseVerifyButton (captchaService).
fn verify_button_of(row: Option<&Value>) -> Value {
    let raw = obj(get(row, "captchaTemplate")).and_then(|t| t.get("verifyButton")).and_then(Value::as_object);
    let def = &REGISTRY.constants.default_verify_button;
    let label = match raw.and_then(|r| r.get("label")).and_then(Value::as_str) {
        Some(l) if !js_trim(l).is_empty() => utf16_prefix(js_trim(l), 80).to_string(),
        _ => def["label"].as_str().unwrap_or("Verify").to_string(),
    };
    let style = match raw.and_then(|r| r.get("style")).and_then(Value::as_str) {
        Some(s) if BUTTON_STYLES.contains(&s) => s.to_string(),
        _ => def["style"].as_str().unwrap_or("SUCCESS").to_string(),
    };
    json!({ "label": label, "style": style })
}

// ── Escalation tiers (normalizeEscalationTiers in src/services/moderationService.js) ───────────────────────

const LEGACY_ESCALATION_SINCE: &str = "2026-10-10T12:00:00.000Z";

fn to_pos_int(v: Option<&Value>) -> Option<f64> {
    v.filter(|v| is_integer(v)).and_then(Value::as_f64).filter(|n| *n > 0.0)
}

fn tier_key(t: &Value) -> String {
    let w = match t.get("withinSeconds") {
        None | Some(Value::Null) => "null".to_string(),
        Some(v) => crate::js::to_js_string(Some(v)),
    };
    format!("{}|{w}", crate::js::to_js_string(t.get("warnCount")))
}

fn sort_tiers(list: &mut [Value]) {
    let f = |t: &Value, k: &str, missing: f64| t.get(k).and_then(Value::as_f64).unwrap_or(missing);
    list.sort_by(|a, b| {
        f(a, "warnCount", f64::NAN)
            .partial_cmp(&f(b, "warnCount", f64::NAN))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                f(a, "withinSeconds", f64::INFINITY).partial_cmp(&f(b, "withinSeconds", f64::INFINITY)).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| f(a, "id", f64::NAN).partial_cmp(&f(b, "id", f64::NAN)).unwrap_or(std::cmp::Ordering::Equal))
    });
}

/// The stored tiers in the bot's canonical form (`normalizeEscalation`: legacy rows read, sorted, deduplicated,
/// capped). Never fails.
pub fn normalize_escalation(raw: Option<&Value>) -> Value {
    let Some(list) = raw.and_then(Value::as_array) else { return json!([]) };
    let l = limits();
    let tier_names: Vec<&str> = REGISTRY.constants.tier_actions.iter().map(|a| a.value.as_str()).collect();
    let mut entries: Vec<(Option<f64>, Map<String, Value>)> = Vec::new();
    for e in list {
        let Some(e) = e.as_object() else { continue };
        let legacy = !e.contains_key("withinSeconds") && !e.contains_key("durationSeconds");
        let Some(warn) = e.get("warnCount").filter(|v| is_integer(v)).and_then(Value::as_f64).filter(|n| (1.0..=100.0).contains(n)) else {
            continue;
        };
        let mut action = to_js_string_or_empty(e.get("action")).to_uppercase();
        if !tier_names.contains(&action.as_str()) {
            continue;
        }
        let mut dur = to_pos_int(if legacy { e.get("duration") } else { e.get("durationSeconds") });
        let mut within: Option<f64> = None;
        if !legacy {
            let w = to_number(e.get("withinSeconds"));
            if w.is_finite() && w.fract() == 0.0 && w >= 60.0 && w <= l.timed_action_max_seconds as f64 {
                within = Some(w);
            }
        }
        if legacy && action == "BAN" && dur.is_some() {
            action = "TEMPBAN".into();
        }
        if action == "TIMEOUT" || action == "TEMPBAN" {
            let Some(d) = dur else { continue };
            let max = if action == "TIMEOUT" { l.timeout_max_seconds } else { l.timed_action_max_seconds } as f64;
            dur = Some(d.max(60.0).min(max));
        } else if action == "MUTE" {
            dur = dur.map(|d| d.max(60.0).min(l.timed_action_max_seconds as f64));
        } else {
            dur = None;
        }
        let mut count_from = match e.get("countFrom") {
            Some(Value::String(s)) if bot_util::js::date_parse(s).is_finite() => Value::String(s.clone()),
            _ => Value::Null,
        };
        if legacy && count_from.is_null() {
            count_from = Value::String(LEGACY_ESCALATION_SINCE.into());
        }
        let id = e.get("id").filter(|v| is_integer(v)).and_then(Value::as_f64).filter(|n| *n >= 1.0);
        let mut t = Map::new();
        t.insert("id".into(), id.map_or(Value::Null, js_num));
        t.insert("warnCount".into(), js_num(warn));
        t.insert("withinSeconds".into(), within.map_or(Value::Null, js_num));
        t.insert("action".into(), Value::String(action));
        t.insert("durationSeconds".into(), dur.map_or(Value::Null, js_num));
        t.insert("countFrom".into(), count_from);
        entries.push((id, t));
    }
    let mut taken: HashSet<i64> = HashSet::new();
    let mut next = entries.iter().filter_map(|(id, _)| *id).fold(0.0_f64, f64::max) as i64;
    for (id, t) in &mut entries {
        if let Some(i) = id.map(|i| i as i64)
            && !taken.contains(&i)
        {
            taken.insert(i);
            continue;
        }
        loop {
            next += 1;
            if !taken.contains(&next) {
                break;
            }
        }
        t.insert("id".into(), json!(next));
        taken.insert(next);
    }
    let mut by_key: IndexMap<String, Value> = IndexMap::new();
    for (_, t) in entries {
        let t = Value::Object(t);
        by_key.insert(tier_key(&t), t);
    }
    let mut out: Vec<Value> = by_key.into_values().collect();
    sort_tiers(&mut out);
    out.truncate(bot_config::constants::limits::ESCALATION_TIERS);
    Value::Array(out)
}

/// The /modconfig escalation add rules for each tier, and no two tiers for the same warnings and window.
fn escalation_problem(tiers: &Value) -> Option<Problem> {
    let l = limits();
    let mut seen: HashSet<String> = HashSet::new();
    for (i, t) in tiers.as_array().into_iter().flatten().enumerate() {
        if !seen.insert(tier_key(t)) {
            return Some(Problem::at(format!("{i}.warnCount"), "Another step already uses this number of warnings and window."));
        }
        let d = t.get("durationSeconds").and_then(Value::as_f64);
        let action = t.get("action").and_then(Value::as_str).unwrap_or_default();
        let path = format!("{i}.durationSeconds");
        if action == "TIMEOUT" && d.is_none_or(|d| d > l.timeout_max_seconds as f64) {
            return Some(Problem::at(path, "A timeout needs a duration of 1 minute to 28 days (2419200 seconds)."));
        }
        if action == "TEMPBAN" && d.is_none() {
            return Some(Problem::at(path, "A temporary ban needs a duration."));
        }
        if (action == "KICK" || action == "BAN") && d.is_some() {
            return Some(Problem::at(path, "Kick and permanent ban have no duration: leave it empty."));
        }
    }
    None
}

/// Tiers to store: a tier with the same warnings and window keeps its id and countFrom; a new one gets the next id.
fn escalation_to_stored(tiers: &Value, row: Option<&Value>) -> Value {
    let current = normalize_escalation(get(row, "infractionEscalation"));
    let current = current.as_array().cloned().unwrap_or_default();
    let mut next = current.iter().filter_map(|t| t.get("id").and_then(Value::as_f64)).fold(0.0_f64, f64::max) as i64;
    let mut out: Vec<Value> = tiers
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let key = tier_key(t);
            let old = current.iter().rev().find(|c| tier_key(c) == key);
            let id = match old {
                Some(o) => o["id"].clone(),
                None => {
                    next += 1;
                    json!(next)
                }
            };
            let action = t.get("action").cloned().unwrap_or(Value::Null);
            let kick_or_ban = matches!(action.as_str(), Some("KICK" | "BAN"));
            json!({
                "id": id,
                "warnCount": t.get("warnCount").cloned().unwrap_or(Value::Null),
                "withinSeconds": t.get("withinSeconds").cloned().unwrap_or(Value::Null),
                "action": action,
                "durationSeconds": if kick_or_ban { Value::Null } else { t.get("durationSeconds").cloned().unwrap_or(Value::Null) },
                "countFrom": old.map_or(Value::Null, |o| o["countFrom"].clone()),
            })
        })
        .collect();
    sort_tiers(&mut out);
    Value::Array(out)
}

// ── Hooks ───────────────────────────────────────────────────────────────────────────────────────────────────

/// A problem a `check` hook found: `path` is relative to the field (None: the field itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: Option<String>,
    pub message: String,
}

impl Problem {
    fn msg(message: impl Into<String>) -> Self {
        Self { path: None, message: message.into() }
    }
    fn at(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { path: Some(path.into()), message: message.into() }
    }
}

/// The 12 message fields (`messageField`): null means the default text. `trim` is false for the welcome, welcome DM
/// and goodbye texts, which are stored as typed.
fn message_field(field: &str) -> Option<bool> {
    REGISTRY.constants.default_texts.contains_key(field).then_some(!matches!(field, "welcomeMessage" | "welcomeDmMessage" | "goodbyeMessage"))
}

/// The `read(row)` hook of a field: its stored value in a GuildSettings row. `None` when the field has no read hook
/// (its value is then `row[field]`).
pub fn hook_read(field: &str, row: Option<&Value>) -> Option<Value> {
    if let Some(key) = rule_key_of(field) {
        return Some(automod_config_of(row)["rules"][key].clone());
    }
    if let Some(key) = list_key_of(field) {
        return Some(automod_config_of(row)[key].clone());
    }
    let list_or_empty = |k: &str| get(row, k).filter(|v| v.is_array()).cloned().unwrap_or_else(|| json!([]));
    Some(match field {
        HONEYPOT_FIELD => automod_config_of(row)["honeypot"].clone(),
        "timezoneRoles" | "xpBoostRoles" | "autoThreadChannels" | "voiceRoleLinks" => list_or_empty(field),
        "infractionEscalation" => normalize_escalation(get(row, field)),
        "logEvents" => {
            let off: Vec<Value> = get(row, "disabledLogEvents").and_then(Value::as_array).cloned().unwrap_or_default();
            Value::Array(
                log_event_keys().filter(|k| !off.iter().any(|o| o.as_str() == Some(k.as_str()))).map(|k| Value::String(k.clone())).collect(),
            )
        }
        "welcomeMessage" => match get(row, field) {
            Some(Value::String(s)) => Value::String(s.clone()),
            // Null: the bot's default text, except next to a custom embed (legacy rows keep sending no text line there).
            _ => {
                let custom = get(row, "welcomeUseEmbed") != Some(&Value::Bool(false))
                    && embed_has_content(&stored_embed(get(row, "welcomeEmbed")));
                Value::String(if custom { String::new() } else { default_text(field).unwrap_or_default().to_string() })
            }
        },
        "goodbyeMessage" => match get(row, field) {
            Some(Value::String(s)) => Value::String(s.clone()),
            _ => Value::String(if get(row, "goodbyeUseEmbed") == Some(&Value::Bool(true)) {
                String::new()
            } else {
                default_text(field).unwrap_or_default().to_string()
            }),
        },
        "welcomeEmbed" | "goodbyeEmbed" => {
            let spec = stored_embed(get(row, field));
            if embed_has_content(&spec) { spec } else { REGISTRY.constants.default_embeds.get(field).cloned().unwrap_or(Value::Null) }
        }
        "welcomeDmEmbed" => {
            let spec = stored_embed(get(row, field));
            if embed_has_content(&spec) { spec } else { Value::Null }
        }
        "welcomeCardStyle" => welcome_card_style_of(row),
        "captchaTemplate" => captcha_panel_of(row),
        "captchaVerifyButton" => verify_button_of(row),
        _ => {
            let trim = message_field(field)?;
            match get(row, field) {
                Some(Value::String(s)) if !(trim && js_trim(s).is_empty()) => Value::String(s.clone()),
                _ => Value::String(default_text(field).unwrap_or_default().to_string()),
            }
        }
    })
}

fn duplicate(list: &Value, key: impl Fn(&Value) -> String, sub: &str, message: &str) -> Option<Problem> {
    let mut seen: HashSet<String> = HashSet::new();
    for (i, item) in list.as_array().into_iter().flatten().enumerate() {
        if !seen.insert(key(item)) {
            return Some(Problem::at(format!("{i}.{sub}"), message));
        }
    }
    None
}

fn str_of(v: &Value, key: &str) -> String {
    crate::js::to_js_string(v.get(key))
}

/// The `check(value)` hook of a field, run after [`validate_value`] on a non-null value. `None` when the field has
/// no check hook; `Some(None)` when the value passes.
pub fn hook_check(field: &str, value: &Value) -> Option<Option<Problem>> {
    let items = || value.as_array().into_iter().flatten();
    Some(match field {
        "timezoneRoles" | "xpBoostRoles" => duplicate(value, |l| str_of(l, "roleId"), "roleId", "That role is already in the list."),
        "autoThreadChannels" => duplicate(value, |l| str_of(l, "channelId"), "channelId", "That channel is already in the list."),
        "voiceRoleLinks" => duplicate(
            value,
            |l| format!("{}:{}", str_of(l, "channelId"), str_of(l, "roleId")),
            "roleId",
            "That channel already has this role.",
        ),
        "infractionEscalation" => escalation_problem(value),
        "automodBadWords" => items().enumerate().find(|(_, w)| normalize_automod_word(Some(w)).is_none()).map(|(i, w)| {
            Problem::at(
                i.to_string(),
                format!("\"{}\" needs 2 to 64 characters with at least one letter or digit.", utf16_prefix(w.as_str().unwrap_or_default(), 40)),
            )
        }),
        "automodBadWordPatterns" => items().enumerate().find_map(|(i, p)| {
            let p = p.as_str().unwrap_or_default();
            pattern_problem(p).map(|problem| Problem::at(i.to_string(), format!("\"{}\" {problem}", utf16_prefix(p, 40))))
        }),
        "automodBlockedDomains" | "automodAllowedDomains" => {
            items().enumerate().find(|(_, d)| normalize_automod_domain(Some(d)).is_none()).map(|(i, d)| {
                Problem::at(i.to_string(), format!("\"{}\" is not a domain such as example.com.", utf16_prefix(d.as_str().unwrap_or_default(), 40)))
            })
        }
        "captchaTimeoutMinutes" => {
            let n = value.as_f64().unwrap_or(f64::NAN);
            (!(n == 0.0 || n >= limits().captcha_timeout_min_minutes as f64))
                .then(|| Problem::msg("Use 0 (never) or between 5 and 10080 minutes (7 days)."))
        }
        "achievementMessage" => {
            let Some(text) = value.as_str() else { return Some(None) };
            let ctx = RenderContext {
                vars: Some(SAMPLE_ACHIEVEMENT_VARS.iter().map(|(k, v)| ((*k).to_string(), VarValue::from(*v))).collect()),
                ..RenderContext::default()
            };
            js_trim(&render_placeholders_detailed(text, &ctx, None).text)
                .is_empty()
                .then(|| Problem::msg("That text renders as an empty message."))
        }
        "currencyEmoji" => (!currency_emoji_ok(value.as_str().unwrap_or_default()))
            .then(|| Problem::msg("That is not an emoji. Use a standard emoji like 🪙 or a custom emoji.")),
        _ => return None,
    })
}

fn dedupe(list: Vec<Value>) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for v in list {
        if !out.iter().any(|o| strict_eq(o, &v)) {
            out.push(v);
        }
    }
    Value::Array(out)
}

/// The `normalize(value)` hook of a field: the value to store, after the check. `None` when the field has none.
pub fn hook_normalize(field: &str, value: Value) -> Option<Value> {
    let opt = |s: Option<String>| s.map_or(Value::Null, Value::String);
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    Some(match field {
        "timezone" => Value::String(canonical_zone(value.as_str().unwrap_or_default())),
        "customPrefix" => {
            let s = value.as_str().unwrap_or_default();
            Value::String(if bot_util::js::lower(s) == "off" { String::new() } else { s.to_string() })
        }
        "timezoneRoles" => Value::Array(
            list(&value)
                .iter()
                .map(|l| json!({ "roleId": l.get("roleId").cloned().unwrap_or(Value::Null), "zone": canonical_zone(l["zone"].as_str().unwrap_or_default()) }))
                .collect(),
        ),
        "xpBoostRoles" => Value::Array(
            list(&value)
                .iter()
                .map(|b| {
                    json!({ "roleId": b.get("roleId").cloned().unwrap_or(Value::Null), "multiplier": js_num(round2(b["multiplier"].as_f64().unwrap_or(f64::NAN))) })
                })
                .collect(),
        ),
        "automodBadWords" => dedupe(list(&value).iter().map(|w| opt(normalize_automod_word(Some(w)))).collect()),
        "automodBlockedDomains" | "automodAllowedDomains" => {
            dedupe(list(&value).iter().map(|d| opt(normalize_automod_domain(Some(d)))).collect())
        }
        "logEvents" => {
            let chosen = list(&value);
            Value::Array(
                log_event_keys().filter(|k| chosen.iter().any(|c| c.as_str() == Some(k.as_str()))).map(|k| Value::String(k.clone())).collect(),
            )
        }
        "welcomeEmbed" | "welcomeDmEmbed" | "goodbyeEmbed" => embed_to_stored(&value),
        "xpMultiplier" => js_num(round2(value.as_f64().unwrap_or(f64::NAN))),
        "rankCardStyle" => {
            let mut m = value.as_object().cloned().unwrap_or_default();
            let o = m.get("overlayOpacity").and_then(Value::as_f64).unwrap_or(f64::NAN);
            m.insert("overlayOpacity".into(), js_num(round2(o)));
            Value::Object(m)
        }
        "currencyName" => Value::String(collapse_spaces(value.as_str().unwrap_or_default())),
        _ => {
            if message_field(field)? {
                match value {
                    Value::String(s) => Value::String(js_trim(&s).to_string()),
                    other => other,
                }
            } else {
                return None;
            }
        }
    })
}

/// The columns to write, being built (`draft`), with the automod lists written in this request.
#[derive(Debug, Default)]
pub struct Draft {
    pub columns: Map<String, Value>,
    touched: HashSet<String>,
}

fn automod_draft<'a>(draft: &'a mut Draft, row: Option<&Value>) -> &'a mut Map<String, Value> {
    if draft.columns.get("automodConfig").is_none_or(Value::is_null) {
        draft.columns.insert("automodConfig".into(), automod_config_of(row));
    }
    let cfg = draft.columns.get_mut("automodConfig").expect("just inserted");
    if !cfg.is_object() {
        *cfg = json!({});
    }
    cfg.as_object_mut().expect("an object")
}

fn captcha_draft<'a>(draft: &'a mut Draft, row: Option<&Value>) -> &'a mut Map<String, Value> {
    if !draft.columns.get("captchaTemplate").is_some_and(|v| crate::js::truthy(Some(v))) {
        let mut raw = match get(row, "captchaTemplate") {
            Some(t @ Value::Object(_)) => normalize_template(t).as_object().cloned().unwrap_or_default(),
            _ => json!({ "content": null, "embed": null, "buttons": [] }).as_object().cloned().unwrap_or_default(),
        };
        raw.insert("verifyButton".into(), verify_button_of(row));
        draft.columns.insert("captchaTemplate".into(), Value::Object(raw));
    }
    draft.columns.get_mut("captchaTemplate").and_then(Value::as_object_mut).expect("an object")
}

/// The `write(value, draft, row)` hook of a field: puts the value into the draft. False when the field has no write
/// hook (the value is then written to the column of the same name).
pub fn hook_write(field: &str, value: Value, draft: &mut Draft, row: Option<&Value>) -> bool {
    if let Some(key) = rule_key_of(field) {
        let cfg = automod_draft(draft, row);
        if !cfg.get("rules").is_some_and(Value::is_object) {
            cfg.insert("rules".into(), json!({}));
        }
        cfg["rules"].as_object_mut().expect("an object").insert(key.into(), value);
        return true;
    }
    if let Some(key) = list_key_of(field) {
        let other = match key {
            "blockedDomains" => Some("allowedDomains"),
            "allowedDomains" => Some("blockedDomains"),
            _ => None,
        };
        let skip_other = other.is_none_or(|o| draft.touched.contains(o));
        let cfg = automod_draft(draft, row);
        let written: Vec<Value> = value.as_array().cloned().unwrap_or_default();
        cfg.insert(key.into(), value);
        // Adding a domain to one list takes it off the other (as /automod domains does), unless both lists are saved
        // together.
        if let Some(o) = other.filter(|_| !skip_other) {
            let kept: Vec<Value> =
                cfg.get(o).and_then(Value::as_array).into_iter().flatten().filter(|d| !written.iter().any(|w| strict_eq(w, d))).cloned().collect();
            cfg.insert(o.into(), Value::Array(kept));
        }
        draft.touched.insert(key.to_string());
        return true;
    }
    match field {
        HONEYPOT_FIELD => {
            automod_draft(draft, row).insert("honeypot".into(), value);
        }
        "infractionEscalation" => {
            let stored = escalation_to_stored(&value, row);
            draft.columns.insert(field.into(), stored);
        }
        "logEvents" => {
            let on = value.as_array().cloned().unwrap_or_default();
            let off: Vec<Value> =
                log_event_keys().filter(|k| !on.iter().any(|v| v.as_str() == Some(k.as_str()))).map(|k| Value::String(k.clone())).collect();
            draft.columns.insert("disabledLogEvents".into(), Value::Array(off));
        }
        "welcomeCardStyle" => {
            let lower = |c: &Value| c.as_str().map_or(Value::Null, |s| Value::String(s.to_lowercase()));
            let overlay = match value.get("overlayOpacity") {
                Some(Value::Null) => Value::Null,
                o => js_num(round2(o.and_then(Value::as_f64).unwrap_or(f64::NAN))),
            };
            let style = json!({
                "v": 1,
                "enabled": value.get("enabled").cloned().unwrap_or(Value::Null),
                "background": lower(&value["background"]),
                "backgroundUrl": value.get("backgroundUrl").cloned().unwrap_or(Value::Null),
                "textColor": lower(&value["textColor"]),
                "accentColor": lower(&value["accentColor"]),
                "overlayOpacity": overlay,
            });
            draft.columns.insert(field.into(), style);
        }
        "captchaTemplate" => {
            let t = normalize_template(&value);
            let target = captcha_draft(draft, row);
            for (k, v) in t.as_object().into_iter().flatten() {
                target.insert(k.clone(), v.clone());
            }
        }
        "captchaVerifyButton" => {
            captcha_draft(draft, row).insert("verifyButton".into(), value);
        }
        _ => return false,
    }
    true
}

/// The hooks the Rust registry has for a field (names, in read, check, normalize, write order).
pub fn hook_names(field: &str) -> Vec<&'static str> {
    let sample = settings_field(field).map(Field::default_or_null).unwrap_or(Value::Null);
    let mut out = Vec::new();
    if hook_read(field, None).is_some() {
        out.push("read");
    }
    if hook_check(field, &sample).is_some() {
        out.push("check");
    }
    if hook_normalize(field, sample.clone()).is_some() {
        out.push("normalize");
    }
    let mut d = Draft::default();
    if hook_write(field, sample, &mut d, None) {
        out.push("write");
    }
    out
}

// ── Reading, validating, storing ────────────────────────────────────────────────────────────────────────────

/// `storedOf(desc, row)`: the read hook's value, else `row[field]` (`None` is undefined).
pub fn stored_of(field: &str, row: Option<&Value>) -> Option<Value> {
    let empty = json!({});
    let r = match row {
        Some(v) if !v.is_null() => Some(v),
        _ => Some(&empty),
    };
    match hook_read(field, r) {
        Some(v) => Some(v),
        None => get(row, field).cloned(),
    }
}

/// Every field's current value, as the browser edits it (`readSettingsValues`), in `SETTINGS_FIELDS` order.
/// `row` is a GuildSettings row (settingsBridge upgrades legacy JSON first), or None / null.
pub fn read_settings_values(row: Option<&Value>) -> Map<String, Value> {
    REGISTRY.settings_fields.iter().map(|(field, desc)| (field.clone(), read_value(desc, stored_of(field, row).as_ref()))).collect()
}

/// Every channel and role id the stored settings hold (`collectSettingsRefs`; checkRefs' keep sets).
pub fn collect_settings_refs(row: Option<&Value>) -> StoredRefs {
    let mut out = StoredRefs::default();
    for (field, desc) in &REGISTRY.settings_fields {
        collect_stored_refs(desc, stored_of(field, row).as_ref(), &mut out);
    }
    out
}

/// Which stored id sits in which setting (`collectSettingsRefKeys`: refKey per field path).
pub fn collect_settings_ref_keys(row: Option<&Value>) -> IndexSet<String> {
    let mut out = IndexSet::new();
    for (field, desc) in &REGISTRY.settings_fields {
        stored_ref_keys(desc, stored_of(field, row).as_ref(), Some(field), &mut out);
    }
    out
}

/// Why a PATCH body was refused: the message and, when known, the path of the first problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsError {
    pub message: String,
    pub path: Option<String>,
}

/// Validated changes: field -> value to store, and every channel and role id met.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedChanges {
    pub changes: Map<String, Value>,
    pub refs: Refs,
}

const FORBIDDEN_KEYS: [&str; 3] = ["__proto__", "constructor", "prototype"];

fn sub_join(path: &str, sub: Option<&str>) -> String {
    match sub {
        None | Some("") => path.to_string(),
        Some(s) => format!("{path}.{s}"),
    }
}

/// Validates one field's value with the commands' rules (validate_value, then its check and normalize hooks).
pub fn validate_setting(field: &str, value: &Value, refs: &mut Refs) -> Result<Value, FieldError> {
    let Some(desc) = settings_field(field) else {
        return Err(FieldError::new(utf16_prefix(field, 64), format!("\"{}\" is not a setting the dashboard can change.", utf16_prefix(field, 64))));
    };
    let mut v = validate_value(desc, value, Some(field), refs)?;
    if !v.is_null() || !desc.nullable() {
        if let Some(Some(p)) = hook_check(field, &v) {
            return Err(FieldError::new(sub_join(field, p.path.as_deref()), p.message));
        }
        if let Some(n) = hook_normalize(field, v.clone()) {
            v = n;
        }
    }
    Ok(canon(v))
}

/// Validates a PATCH body `{ changes: { [field]: value } }` with the commands' rules for each value
/// (`validateSettingsChanges`; cross-field rules need the stored row: [`check_cross_fields`]).
pub fn validate_settings_changes(body: &Value) -> Result<ValidatedChanges, SettingsError> {
    let fail = |m: String, path: Option<String>| Err(SettingsError { message: m, path });
    let Some(body) = body.as_object() else { return fail("The body must be a JSON object.".into(), None) };
    if body.keys().any(|k| k != "changes") {
        return fail("The body may only contain \"changes\".".into(), None);
    }
    let Some(input) = body.get("changes").and_then(Value::as_object) else {
        return fail("\"changes\" must be an object of field: value.".into(), None);
    };
    let keys = js_keys(input);
    if keys.is_empty() {
        return fail("\"changes\" is empty.".into(), None);
    }
    let max = max_settings_changes();
    if keys.len() > max {
        return fail(format!("At most {max} changes per request."), None);
    }
    let mut refs = Refs::default();
    let mut changes = Map::new();
    for key in keys {
        if FORBIDDEN_KEYS.contains(&key.as_str()) || settings_field(key).is_none() {
            let short = utf16_prefix(key, 64).to_string();
            return fail(format!("\"{short}\" is not a setting the dashboard can change."), Some(short));
        }
        match validate_setting(key, &input[key.as_str()], &mut refs) {
            Ok(v) => {
                changes.insert(key.clone(), v);
            }
            Err(e) => {
                let path = if e.path.is_empty() { key.clone() } else { e.path };
                return fail(e.message, Some(path));
            }
        }
    }
    Ok(ValidatedChanges { changes, refs })
}

/// The columns to write for validated changes (`toStoredChanges`), merged into the stored row where a field is part
/// of a JSON column (automod rules and lists, the captcha panel) or keeps bot-managed parts (escalation tier ids). JSON
/// nulls stay plain null: the caller writes SQL NULL for them ([`is_json_column`]).
pub fn to_stored_changes(changes: &Map<String, Value>, row: Option<&Value>) -> Map<String, Value> {
    let mut draft = Draft::default();
    for (field, value) in changes {
        if settings_field(field).is_none() {
            continue;
        }
        if !hook_write(field, value.clone(), &mut draft, row) {
            draft.columns.insert(field.clone(), value.clone());
        }
    }
    draft.columns
}

/// A broken cross-field rule: the field to blame and the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossProblem {
    pub path: String,
    pub message: String,
}

/// `(x ?? []).includes(needle)`.
fn includes(list: Option<&Value>, needle: Option<&Value>) -> bool {
    let needle = needle.unwrap_or(&Value::Null);
    match list {
        Some(Value::Array(a)) => a.iter().any(|v| strict_eq(v, needle)),
        Some(Value::String(s)) => s.contains(&crate::js::to_js_string(Some(needle))),
        _ => false,
    }
}

/// `a === b` where `None` is undefined (undefined equals only undefined).
fn js_eq_opt(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => strict_eq(x, y),
        _ => false,
    }
}

type CrossTest = fn(&Map<String, Value>) -> bool;

fn min_max_ok(r: &Map<String, Value>, min: &str, max: &str) -> bool {
    let (a, b) = (r.get(min), r.get(max));
    !(a.is_some_and(is_integer) && b.is_some_and(is_integer) && a.and_then(Value::as_f64) > b.and_then(Value::as_f64))
}

fn channel_mode_ok(r: &Map<String, Value>, mode: &str, channel: &str) -> bool {
    r.get(mode).and_then(Value::as_str) != Some("CHANNEL") || r.get(channel).is_some_and(is_snowflake)
}

const CROSS_RULES: [(&[&str], CrossTest, &str); 9] = [
    (&["xpMax", "xpMin"], |r| min_max_ok(r, "xpMin", "xpMax"), "The minimum XP per message cannot be higher than the maximum."),
    (&["dailyMax", "dailyMin"], |r| min_max_ok(r, "dailyMin", "dailyMax"), "The minimum daily reward cannot be higher than the maximum."),
    (&["workMax", "workMin"], |r| min_max_ok(r, "workMin", "workMax"), "The minimum work pay cannot be higher than the maximum."),
    (
        &["economyStartingBalance", "economyMaxBalance"],
        |r| min_max_ok(r, "economyStartingBalance", "economyMaxBalance"),
        "The starting balance cannot be higher than the maximum balance.",
    ),
    (&["levelUpChannelId", "levelUpMode"], |r| channel_mode_ok(r, "levelUpMode", "levelUpChannelId"), "Pick the announcement channel for \"Custom channel\"."),
    (
        &["achievementChannelId", "achievementAnnounceMode"],
        |r| channel_mode_ok(r, "achievementAnnounceMode", "achievementChannelId"),
        "Pick the announcement channel for \"Custom channel\".",
    ),
    // checkVerifiedRole (captchaService): checked when the verified role is picked.
    (
        &["captchaRoleId"],
        |r| {
            let role = r.get("captchaRoleId");
            !crate::js::truthy(role)
                || !(includes(r.get("modRoleIds"), role) || includes(r.get("immuneRoleIds"), role) || js_eq_opt(r.get("muteRoleId"), role))
        },
        "The verified role cannot also be a moderator, immune or mute role.",
    ),
    // missingRequirement (captchaService) ignores the verified role: nobody holds it before verifying.
    (
        &["captchaRequiredRoleIds", "captchaRoleId"],
        |r| !crate::js::truthy(r.get("captchaRoleId")) || !includes(r.get("captchaRequiredRoleIds"), r.get("captchaRoleId")),
        "The verified role cannot also be a required role.",
    ),
    (
        &["liveRoleRequiredRoleId", "liveRoleId"],
        |r| {
            let live = r.get("liveRoleId");
            !crate::js::truthy(live) || !js_eq_opt(live, r.get("liveRoleRequiredRoleId"))
        },
        "The live role and the required role must be different roles.",
    ),
];

/// The first cross-field rule the merged row breaks (`checkCrossFields`), or None. Only rules that involve a changed
/// field are checked (`changes` None: every rule), so an old inconsistency never blocks an unrelated save. `row` is
/// the stored row with the changes applied ([`to_stored_changes`]).
pub fn check_cross_fields(row: Option<&Value>, changes: Option<&Map<String, Value>>) -> Option<CrossProblem> {
    let empty = Map::new();
    let r = row.and_then(Value::as_object).unwrap_or(&empty);
    let changed = |f: &str| changes.is_none_or(|c| c.contains_key(f));
    for (fields, test, message) in CROSS_RULES {
        if let Some(blamed) = fields.iter().find(|f| changed(f))
            && !test(r)
        {
            return Some(CrossProblem { path: (*blamed).to_string(), message: message.to_string() });
        }
    }

    // /modconfig escalation add: a mute step without a duration, or longer than a timeout can last, needs the mute role.
    if changed("infractionEscalation") && !r.get("muteRoleId").is_some_and(is_snowflake) {
        let sent = match changes.and_then(|c| c.get("infractionEscalation")) {
            Some(v @ Value::Array(_)) => v.clone(),
            _ => normalize_escalation(r.get("infractionEscalation")),
        };
        let max = limits().timeout_max_seconds as f64;
        let hit = sent.as_array().into_iter().flatten().position(|t| {
            t.get("action").and_then(Value::as_str) == Some("MUTE")
                && match t.get("durationSeconds") {
                    Some(Value::Null) => true,
                    Some(v) => v.as_f64().is_some_and(|d| d > max),
                    None => false,
                }
        });
        if let Some(i) = hit {
            return Some(CrossProblem {
                path: format!("infractionEscalation.{i}.action"),
                message: "A mute without a duration or longer than 28 days needs a mute role. Set one first.".into(),
            });
        }
    }

    // A domain cannot be both blocked and allowed (only possible when both lists are saved together).
    if changed("automodBlockedDomains") || changed("automodAllowedDomains") {
        let cfg = automod_config_of(Some(&Value::Object(r.clone())));
        let allowed = cfg["allowedDomains"].as_array().cloned().unwrap_or_default();
        if let Some(both) = cfg["blockedDomains"].as_array().into_iter().flatten().find(|d| allowed.iter().any(|a| strict_eq(a, d))) {
            return Some(CrossProblem {
                path: "automodAllowedDomains".into(),
                message: format!("{} is in both the blocked and the allowed domains.", crate::js::to_js_string(Some(both))),
            });
        }
    }
    None
}
