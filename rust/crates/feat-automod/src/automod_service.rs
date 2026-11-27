//! Port of `src/services/automodService.js`: auto-moderation (pkg-automod.md).
//!
//! The MEE6 rule set (bad words, repeated text, invites, links, personal info, caps, emojis, spoilers, mentions, zalgo, message-rate
//! anti-spam), each with its own action and role/channel scope, plus honeypot channels (whoever posts in one is
//! punished at once).
//!
//! This module holds the pure config model (v2 shape, v1 upgrade, normalisation), the detectors, the compiled-matcher
//! memo, outcome resolution and the action pipeline [`run_automod`]. It has no database access of its own: settings
//! arrive through the hook context, and every punishment goes through
//! `feat_moderation::moderation_service::execute_mod_action` so the Discord action really happens.
//!
//! Failure policy (as the JS):
//! - Redis down -> `spam` and `repeatedText` are skipped (fail OPEN: never punish on missing data).
//! - Malformed config -> normalised, never an error; a bad list entry is dropped.
//! - Unknown member -> if the lookup failed for any reason other than "not in the server", do nothing.
//! - A failed delete -> never punishes when the action needed the delete, never tells the member their message was
//!   removed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use bot_core::hooks::{AutomodAction, HookCtx, HookEvent};
use bot_core::interaction_helpers::{PermTarget, check_channel_usable, missing_bot_permissions};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::safe_regex::{MatchOptions, SafeRegex, compile_safe_regex, contains_word, escape_regex, safe_regex_first_match};
use bot_util::text::clamp_text;
use feat_moderation::moderation_service::{ModActionInput, ModError, ModSuccess};
use indexmap::IndexMap;
use regress::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Member, Message, MessageId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateMessage};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use crate::js_re;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const AUTOMOD_CONFIG_VERSION: i64 = 2;
pub const SCOPE_MODES: [&str; 2] = ["except", "only"];
pub const DEFAULT_TIMEOUT_SECONDS: i64 = 600;
pub const MIN_TIMEOUT_SECONDS: i64 = 60;
/// 28 days (Discord timeout maximum).
pub const MAX_TIMEOUT_SECONDS: i64 = 2_419_200;
pub const CAPS_MIN_LETTERS: usize = 10;
pub const REPEAT_WINDOW_MS: i64 = 30_000;
pub const SPAM_WINDOW_MS: i64 = 5_000;
pub const PUNISH_WINDOW_MS: i64 = 30_000;
pub const NOTICE_TTL_MS: u64 = 6_000;
pub const BAN_DELETE_MESSAGE_SECONDS: i64 = 3_600;
/// Nitro messages exceed safeRegex's 2000 default.
pub const PATTERN_INPUT_CAP: usize = 4_000;
pub const WORD_MIN_LENGTH: usize = 2;
pub const WORD_MAX_LENGTH: usize = 64;
/// `LIMITS.AUTOMOD_EXEMPTIONS`.
pub const EXEMPT_CAP: usize = bot_config::limits::AUTOMOD_EXEMPTIONS;

/// Seeded into `allowedDomains` only when that key is missing (new config or v1 upgrade).
pub const DEFAULT_ALLOWED_DOMAINS: [&str; 5] = ["tenor.com", "giphy.com", "discord.com", "discordapp.com", "discordapp.net"];

/// Rule keys in display order (= tie-break order).
pub const RULE_KEYS: [&str; 11] = ["badWords", "repeatedText", "invites", "links", "personalInfo", "caps", "emojis", "spoilers", "mentions", "zalgo", "spam"];

/// A rule's threshold: `{ min, max, default, unit }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold {
    pub min: i64,
    pub max: i64,
    pub default: i64,
    pub unit: &'static str,
}

/// One entry of `RULES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleMeta {
    pub key: &'static str,
    pub label: &'static str,
    pub emoji: &'static str,
    pub threshold: Option<Threshold>,
    pub on_edit: bool,
    pub description: &'static str,
}

const fn t(min: i64, max: i64, default: i64, unit: &'static str) -> Option<Threshold> {
    Some(Threshold { min, max, default, unit })
}

/// The rule registry, in [`RULE_KEYS`] order.
pub const RULES: [RuleMeta; 11] = [
    RuleMeta { key: "badWords", label: "Bad Words", emoji: "🤬", threshold: None, on_edit: true, description: "Removes messages that contain a listed word, phrase or pattern." },
    RuleMeta { key: "repeatedText", label: "Repeated Text", emoji: "🔁", threshold: t(2, 20, 3, "identical messages within 30 s"), on_edit: false, description: "Catches the same message sent over and over." },
    RuleMeta { key: "invites", label: "Server Invites", emoji: "📨", threshold: None, on_edit: true, description: "Catches invite links to other Discord servers." },
    RuleMeta { key: "links", label: "External Links", emoji: "🔗", threshold: None, on_edit: true, description: "Catches links that are blocked, or not allowed when no domain is blocked." },
    RuleMeta { key: "personalInfo", label: "Personal Info", emoji: "🪪", threshold: None, on_edit: true, description: "Catches phone numbers, street addresses, emails and other personal details (anti-doxxing)." },
    RuleMeta { key: "caps", label: "Excessive Caps", emoji: "🔠", threshold: t(50, 100, 70, "% capitals, messages with at least 10 letters"), on_edit: true, description: "Catches messages that are mostly capital letters." },
    RuleMeta { key: "emojis", label: "Excessive Emojis", emoji: "😀", threshold: t(1, 50, 10, "emojis"), on_edit: true, description: "Catches messages with too many emojis." },
    RuleMeta { key: "spoilers", label: "Excessive Spoilers", emoji: "🙈", threshold: t(1, 50, 5, "spoilers"), on_edit: true, description: "Catches messages with too many spoiler tags or spoiler attachments." },
    RuleMeta { key: "mentions", label: "Excessive Mentions", emoji: "📢", threshold: t(2, 50, 5, "mentions"), on_edit: true, description: "Catches messages that ping too many members or roles." },
    RuleMeta { key: "zalgo", label: "Zalgo", emoji: "👹", threshold: t(3, 20, 4, "stacked combining marks"), on_edit: true, description: "Catches glitch text with piles of combining marks." },
    RuleMeta { key: "spam", label: "Anti-Spam", emoji: "⏩", threshold: t(3, 30, 6, "messages within 5 s"), on_edit: false, description: "Catches members who send messages too fast." },
];

/// `RULES[key]`.
pub fn rule_meta(key: &str) -> Option<&'static RuleMeta> {
    RULES.iter().find(|m| m.key == key)
}

fn rule_index(key: &str) -> i64 {
    RULE_KEYS.iter().position(|k| *k == key).map(|i| i as i64).unwrap_or(-1)
}

pub const ACTIONS: [&str; 7] = ["off", "delete", "warn", "delete_warn", "timeout", "kick", "ban"];

/// `ACTION_LABELS[action]`.
pub fn action_label(action: &str) -> Option<&'static str> {
    Some(match action {
        "off" => "Off",
        "delete" => "Delete message",
        "warn" => "Warn member",
        "delete_warn" => "Delete & warn",
        "timeout" => "Delete & timeout",
        "kick" => "Delete & kick",
        "ban" => "Delete & ban",
        _ => return None,
    })
}

/// `DELETING_ACTIONS.has(action)`.
pub fn is_deleting_action(action: &str) -> bool {
    matches!(action, "delete" | "delete_warn" | "timeout" | "kick" | "ban")
}

/// `PUNISHMENTS[action]`.
pub fn punishment_of(action: &str) -> Option<&'static str> {
    Some(match action {
        "warn" | "delete_warn" => "WARN",
        "timeout" => "TIMEOUT",
        "kick" => "KICK",
        "ban" => "BAN",
        _ => return None,
    })
}

/// `PUNISHMENT_RANK[punishment]`.
pub fn punishment_rank(punishment: &str) -> Option<i32> {
    Some(match punishment {
        "WARN" => 1,
        "TIMEOUT" => 2,
        "KICK" => 3,
        "BAN" | "SOFTBAN" => 4,
        _ => return None,
    })
}

fn punishment_word(p: Option<&str>) -> &'static str {
    match p {
        Some("WARN") => "Warn",
        Some("TIMEOUT") => "Timeout",
        Some("KICK") => "Kick",
        Some("BAN") => "Ban",
        Some("SOFTBAN") => "Softban",
        _ => "Action",
    }
}

/// Honeypot: channels nobody should type in. Not a rule: a rule's "every channel except" default would catch the
/// whole server.
pub const HONEYPOT_LABEL: &str = "Honeypot";
pub const HONEYPOT_EMOJI: &str = "🍯";
pub const HONEYPOT_ACTIONS: [&str; 4] = ["softban", "ban", "kick", "timeout"];

/// `HONEYPOT_ACTION_LABELS[action]`.
pub fn honeypot_action_label(action: &str) -> Option<&'static str> {
    Some(match action {
        "softban" => "Softban (kick and clear their messages)",
        "ban" => "Ban and clear their messages",
        "kick" => "Delete & kick",
        "timeout" => "Delete & timeout",
        _ => return None,
    })
}

/// `HONEYPOT_PUNISHMENTS[action]`.
pub fn honeypot_punishment(action: &str) -> Option<&'static str> {
    Some(match action {
        "softban" => "SOFTBAN",
        "ban" => "BAN",
        "kick" => "KICK",
        "timeout" => "TIMEOUT",
        _ => return None,
    })
}

pub const HONEYPOT_DEFAULT_TIMEOUT_SECONDS: i64 = 86_400;
/// A ban or softban clears the last day of the member's messages in every channel.
pub const HONEYPOT_DELETE_MESSAGE_SECONDS: i64 = 86_400;
/// `LIMITS.AUTOMOD_HONEYPOTS`.
pub const HONEYPOT_CAP: usize = bot_config::limits::AUTOMOD_HONEYPOTS;

// ── JS value helpers ──────────────────────────────────────────────────────────

fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `Number.isInteger(v)` for a JSON value.
fn js_integer(v: Option<&Value>) -> Option<i64> {
    let f = v?.as_f64()?;
    if f.is_finite() && f.fract() == 0.0 { Some(f.clamp(i64::MIN as f64, i64::MAX as f64) as i64) } else { None }
}

/// `String(v ?? '')`.
fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(a)) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

fn utf16_len(s: &str) -> usize {
    bot_util::js::utf16_len(s)
}

/// `s.replace(/\s+/g, ' ')` (JS whitespace).
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

fn dedupe(list: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    list.into_iter().filter(|x| seen.insert(x.clone())).collect()
}

// ── Config model ──────────────────────────────────────────────────────────────

/// One rule's settings (the JS key order: `action`, `threshold`, `timeoutSeconds`, `roleMode`, `roleIds`,
/// `channelMode`, `channelIds`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleConfig {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<i64>,
    pub timeout_seconds: i64,
    pub role_mode: String,
    pub role_ids: Vec<String>,
    pub channel_mode: String,
    pub channel_ids: Vec<String>,
}

/// The honeypot settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoneypotConfig {
    pub channel_ids: Vec<String>,
    pub action: String,
    pub timeout_seconds: i64,
}

/// A normalised v2 config (`GuildSettings.automodConfig`), serialised with the JS key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomodConfig {
    pub version: i64,
    /// Every rule key, in [`RULE_KEYS`] order.
    pub rules: IndexMap<String, RuleConfig>,
    pub bad_words: Vec<String>,
    pub bad_word_patterns: Vec<String>,
    pub blocked_domains: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub exempt_role_ids: Vec<String>,
    pub exempt_channel_ids: Vec<String>,
    pub honeypot: HoneypotConfig,
}

impl AutomodConfig {
    /// `config.rules[key]` (every key exists in a normalised config).
    pub fn rule(&self, key: &str) -> RuleConfig {
        self.rules.get(key).cloned().unwrap_or_else(|| default_rule(key))
    }

    /// A mutable `config.rules[key]`.
    pub fn rule_mut(&mut self, key: &str) -> &mut RuleConfig {
        self.rules.entry(key.to_string()).or_insert_with(|| default_rule(key))
    }

    /// The JSON the JS stores.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

fn default_rule(key: &str) -> RuleConfig {
    RuleConfig {
        action: "off".into(),
        threshold: rule_meta(key).and_then(|m| m.threshold).map(|t| t.default),
        timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        role_mode: "except".into(),
        role_ids: Vec::new(),
        channel_mode: "except".into(),
        channel_ids: Vec::new(),
    }
}

fn default_honeypot() -> HoneypotConfig {
    HoneypotConfig { channel_ids: Vec::new(), action: "softban".into(), timeout_seconds: HONEYPOT_DEFAULT_TIMEOUT_SECONDS }
}

/// A fresh, fully populated v2 config with every rule off.
pub fn default_config() -> AutomodConfig {
    AutomodConfig {
        version: AUTOMOD_CONFIG_VERSION,
        rules: RULE_KEYS.iter().map(|k| (k.to_string(), default_rule(k))).collect(),
        bad_words: Vec::new(),
        bad_word_patterns: Vec::new(),
        blocked_domains: Vec::new(),
        allowed_domains: DEFAULT_ALLOWED_DOMAINS.iter().map(|s| s.to_string()).collect(),
        exempt_role_ids: Vec::new(),
        exempt_channel_ids: Vec::new(),
        honeypot: default_honeypot(),
    }
}

/// True for a plain object whose `version` is undefined (the pre-v2 shape; `{}` counts).
pub fn is_legacy_config(raw: Option<&Value>) -> bool {
    matches!(raw, Some(Value::Object(m)) if !m.contains_key("version"))
}

/// Invisible and zero-width characters: soft hyphen, combining grapheme joiner, Arabic letter mark, Mongolian vowel
/// separator, zero-width and bidi marks, word joiner and invisible operators, the bidi isolates, the byte-order mark.
fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0xAD | 0x34F | 0x61C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
}

/// Removes invisible characters.
pub fn strip_invisible(s: &str) -> String {
    s.chars().filter(|c| !is_invisible(*c)).collect()
}

fn nfkc(s: &str) -> String {
    s.nfkc().collect()
}

/// A literal underscore (typed `\_`) is held as this private-use character while markdown is stripped.
const LITERAL_UNDERSCORE: char = '\u{E000}';

static EDGE_UNDERSCORES_RE: LazyLock<Regex> =
    LazyLock::new(|| js_re::re(r"(?<![\p{L}\p{N}_\u{E000}])_+|_+(?![\p{L}\p{N}_\u{E000}])", "gu"));

/// Bad words and patterns: NFKC, markdown emphasis, spoiler bars and escapes removed; edge underscores (italics or
/// underline) become spaces, an underscore between two word parts and an escaped `\_` stay.
pub fn text_for_words(content: &str) -> String {
    let s: String = strip_invisible(&nfkc(content)).chars().filter(|c| *c != LITERAL_UNDERSCORE).collect();
    // `.replace(/\\([\s\S]?)/g, …)`: a backslash and the next character become that character (`\_` is held).
    let mut unescaped = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('_') => unescaped.push(LITERAL_UNDERSCORE),
                Some(n) => unescaped.push(n),
                None => {}
            }
        } else {
            unescaped.push(c);
        }
    }
    let stripped: String = unescaped.chars().filter(|c| !matches!(c, '\\' | '*' | '~' | '|' | '`')).collect();
    let spaced = js_re::replace_all(&EDGE_UNDERSCORES_RE, &stripped, |_, _| " ".to_string());
    spaced.replace(LITERAL_UNDERSCORE, "_")
}

/// Invites and links: NFKC, invisible characters removed, and unescaped (`discord\.gg`).
pub fn text_for_links(content: &str) -> String {
    strip_invisible(&nfkc(content)).replace('\\', "")
}

static LETTER_OR_DIGIT_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"[\p{L}\p{N}]", "u"));

/// Words: NFKC, invisible characters removed, trimmed, lower-cased, inner whitespace collapsed. Length 2-64 and at
/// least one letter or digit.
pub fn normalize_word(input: &str) -> Option<String> {
    let s = collapse_spaces(&bot_util::js::lower(bot_util::js::trim(&strip_invisible(&nfkc(input)))));
    let len = utf16_len(&s);
    if !(WORD_MIN_LENGTH..=WORD_MAX_LENGTH).contains(&len) || !js_re::test(&LETTER_OR_DIGIT_RE, &s) {
        return None;
    }
    Some(s)
}

static PORT_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r":\d+$", ""));
static DOMAIN_RE: LazyLock<Regex> =
    LazyLock::new(|| js_re::re(r"^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:[a-z]{2,63}|xn--[a-z0-9-]{1,59})$", ""));

/// Node's `url.domainToASCII(s)`: the WHATWG host parser; '' when invalid.
fn domain_to_ascii(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    match url::Host::parse(s) {
        Ok(url::Host::Domain(d)) => d,
        Ok(h) => h.to_string(),
        Err(_) => String::new(),
    }
}

/// Domains. Accepts "Example.COM", "https://www.example.com/path", "*.example.com" and IDN. The ASCII (punycode)
/// host without "www.", or `None` when invalid.
pub fn normalize_domain(input: &str) -> Option<String> {
    let mut s = bot_util::js::lower(bot_util::js::trim(input));
    if s.is_empty() {
        return None;
    }
    if s.contains("://") {
        s = url::Url::parse(&s).ok()?.host_str().unwrap_or_default().to_string();
    }
    let first = s.split(['/', '?', '#']).next().unwrap_or_default().to_string();
    let mut s = js_re::replace_all(&PORT_RE, &first, |_, _| String::new());
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
    (js_re::test(&DOMAIN_RE, &s) && utf16_len(&s) <= 253).then_some(s)
}

fn clean_snowflakes(v: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(a)) = v else { return Vec::new() };
    let ids: Vec<String> = a.iter().filter_map(Value::as_str).filter(|s| is_snowflake(s)).map(str::to_string).collect();
    dedupe(ids).into_iter().take(EXEMPT_CAP).collect()
}

fn clean_words(v: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(a)) = v else { return Vec::new() };
    let mut out = Vec::new();
    for item in a {
        let text = match item {
            Value::String(s) => s.clone(),
            Value::Number(n) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
            _ => continue,
        };
        if let Some(w) = normalize_word(&text) {
            out.push(w);
        }
    }
    dedupe(out).into_iter().take(bot_config::limits::AUTOMOD_WORDS).collect()
}

fn clean_patterns(v: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(a)) = v else { return Vec::new() };
    let mut out = Vec::new();
    for item in a {
        let Some(s) = item.as_str() else { continue };
        let p = bot_util::js::trim(s);
        if !p.is_empty() && utf16_len(p) <= 200 {
            out.push(p.to_string());
        }
    }
    dedupe(out).into_iter().take(bot_config::limits::AUTOMOD_PATTERNS).collect()
}

fn clean_domains(v: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(a)) = v else { return Vec::new() };
    let out: Vec<String> = a.iter().filter_map(|item| normalize_domain(&js_string(Some(item)))).collect();
    dedupe(out).into_iter().take(bot_config::limits::AUTOMOD_DOMAINS).collect()
}

fn clean_rule(key: &str, r: Option<&Value>) -> RuleConfig {
    let mut out = default_rule(key);
    let Some(Value::Object(r)) = r else { return out };
    if let Some(a) = r.get("action").and_then(Value::as_str).filter(|a| ACTIONS.contains(a)) {
        out.action = a.to_string();
    }
    if let Some(t) = rule_meta(key).and_then(|m| m.threshold)
        && let Some(n) = js_integer(r.get("threshold"))
    {
        out.threshold = Some(n.clamp(t.min, t.max));
    }
    if let Some(n) = js_integer(r.get("timeoutSeconds")) {
        out.timeout_seconds = n.clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS);
    }
    if let Some(m) = r.get("roleMode").and_then(Value::as_str).filter(|m| SCOPE_MODES.contains(m)) {
        out.role_mode = m.to_string();
    }
    if let Some(m) = r.get("channelMode").and_then(Value::as_str).filter(|m| SCOPE_MODES.contains(m)) {
        out.channel_mode = m.to_string();
    }
    out.role_ids = clean_snowflakes(r.get("roleIds"));
    out.channel_ids = clean_snowflakes(r.get("channelIds"));
    out
}

fn clean_honeypot(h: Option<&Value>) -> HoneypotConfig {
    let mut out = default_honeypot();
    let Some(Value::Object(h)) = h else { return out };
    out.channel_ids = clean_snowflakes(h.get("channelIds")).into_iter().take(HONEYPOT_CAP).collect();
    if let Some(a) = h.get("action").and_then(Value::as_str).filter(|a| HONEYPOT_ACTIONS.contains(a)) {
        out.action = a.to_string();
    }
    if let Some(n) = js_integer(h.get("timeoutSeconds")) {
        out.timeout_seconds = n.clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS);
    }
    out
}

fn map_legacy_action(value: Option<&Value>) -> &'static str {
    match js_string(value).to_uppercase().as_str() {
        "NONE" => "delete",
        "MUTE" | "TIMEOUT" => "timeout",
        "KICK" => "kick",
        "BAN" => "ban",
        _ => "delete_warn", // 'WARN', missing or unknown: the legacy default
    }
}

/// Upgrades a v1 config (flat `filter*` keys) to v2. Only keys that are present enable a rule. Pure.
pub fn upgrade_v1(raw: Option<&Value>) -> AutomodConfig {
    let empty = Map::new();
    let src = match raw {
        Some(Value::Object(m)) => m,
        _ => &empty,
    };
    let legacy = map_legacy_action(src.get("automodAction"));
    let mut rules = Map::new();
    let mut enable = |key: &str, extra: Option<i64>| {
        let mut r = json!({ "action": legacy });
        if let Some(n) = extra {
            r["threshold"] = json!(n);
        }
        rules.insert(key.to_string(), r);
    };
    if src.get("filterInvites") == Some(&Value::Bool(true)) {
        enable("invites", None);
    }
    if src.get("filterLinks") == Some(&Value::Bool(true)) {
        enable("links", None);
    }
    if src.get("filterBadWords") == Some(&Value::Bool(true)) {
        enable("badWords", None);
    }
    if let Some(n) = js_integer(src.get("massMentionThreshold")).filter(|n| *n >= 1) {
        enable("mentions", Some(n.clamp(2, 50)));
    }
    if let Some(n) = js_integer(src.get("duplicateThreshold")).filter(|n| *n >= 2) {
        enable("repeatedText", Some(n.clamp(2, 20)));
    }
    if let Some(n) = js_integer(src.get("spamThreshold")).filter(|n| *n >= 1) {
        enable("spam", Some(n.clamp(3, 30)));
    }
    let mut obj = Map::new();
    obj.insert("version".into(), json!(AUTOMOD_CONFIG_VERSION));
    obj.insert("rules".into(), Value::Object(rules));
    for key in ["badWords", "badWordPatterns", "blockedDomains"] {
        if let Some(v) = src.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    obj.insert("allowedDomains".into(), json!(DEFAULT_ALLOWED_DOMAINS));
    for key in ["exemptRoleIds", "exemptChannelIds"] {
        if let Some(v) = src.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    normalize_config(Some(&Value::Object(obj)))
}

/// A fresh, fully populated v2 config. Never fails and is idempotent. Null, non-objects and arrays give the all-off
/// default; a v1 object is upgraded.
pub fn normalize_config(raw: Option<&Value>) -> AutomodConfig {
    let Some(Value::Object(src)) = raw else { return default_config() };
    if is_legacy_config(raw) {
        return upgrade_v1(raw);
    }
    let mut cfg = default_config();
    let src_rules = src.get("rules").filter(|r| r.is_object());
    for key in RULE_KEYS {
        cfg.rules.insert(key.to_string(), clean_rule(key, src_rules.and_then(|r| r.get(key))));
    }
    cfg.bad_words = clean_words(src.get("badWords"));
    cfg.bad_word_patterns = clean_patterns(src.get("badWordPatterns"));
    cfg.blocked_domains = clean_domains(src.get("blockedDomains"));
    // An explicit [] stays empty (allowlist mode with nothing allowed); a missing or invalid key gets the defaults.
    if let Some(v @ Value::Array(_)) = src.get("allowedDomains") {
        cfg.allowed_domains = clean_domains(Some(v));
    }
    cfg.exempt_role_ids = clean_snowflakes(src.get("exemptRoleIds"));
    cfg.exempt_channel_ids = clean_snowflakes(src.get("exemptChannelIds"));
    cfg.honeypot = clean_honeypot(src.get("honeypot"));
    cfg
}

/// Rule keys whose action is not 'off', in display order.
pub fn active_rule_keys(config: &AutomodConfig) -> Vec<&'static str> {
    RULE_KEYS.iter().copied().filter(|k| config.rules.get(*k).is_some_and(|r| r.action != "off")).collect()
}

// ── Compiled matchers ─────────────────────────────────────────────────────────

static SPACELESS_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    js_re::re(r"^[\p{scx=Han}\p{scx=Hiragana}\p{scx=Katakana}\p{scx=Thai}\p{scx=Lao}\p{scx=Khmer}\p{scx=Myanmar}]+$", "u")
});

/// True for a listed word written only in a script without word spaces (matched as a substring).
pub fn is_spaceless_word(word: &str) -> bool {
    js_re::test(&SPACELESS_WORD_RE, word)
}

/// The compiled bad-word alternation and user patterns of a config.
#[derive(Debug, Clone, Default)]
pub struct Matchers {
    /// One escaped-literal alternation (no user regex syntax, so it cannot backtrack catastrophically).
    pub bad_words_re: Option<Arc<Regex>>,
    /// Set when the alternation could not be built: words are matched one by one.
    pub fallback_words: Option<Vec<String>>,
    /// The patterns that compiled safely.
    pub patterns: Vec<Arc<SafeRegex>>,
    /// The index in `badWordPatterns` of each compiled pattern.
    pub pattern_index: Vec<usize>,
}

/// `buildMatchers(config)`.
pub fn build_matchers(config: &AutomodConfig) -> Matchers {
    let mut m = Matchers::default();
    let words = &config.bad_words;
    if !words.is_empty() {
        let alt = |list: &[&String]| {
            let mut sorted: Vec<&String> = list.to_vec();
            sorted.sort_by_key(|w| std::cmp::Reverse(utf16_len(w)));
            sorted.iter().map(|w| escape_regex(w).replace(' ', "\\s+")).collect::<Vec<_>>().join("|")
        };
        let spaced: Vec<&String> = words.iter().filter(|w| !is_spaceless_word(w)).collect();
        let spaceless: Vec<&String> = words.iter().filter(|w| is_spaceless_word(w)).collect();
        let mut parts = Vec::new();
        if !spaced.is_empty() {
            parts.push(format!(r"(?<![\p{{L}}\p{{N}}_])(?:{})(?![\p{{L}}\p{{N}}_])", alt(&spaced)));
        }
        if !spaceless.is_empty() {
            parts.push(format!("(?:{})", alt(&spaceless)));
        }
        match Regex::with_flags(&parts.join("|"), "iu") {
            Ok(r) => m.bad_words_re = Some(Arc::new(r)),
            Err(e) => {
                bot_util::log_warn!(format!("Automod bad-word regex could not be built, matching word by word: {e}"));
                m.fallback_words = Some(words.clone());
            }
        }
    }
    for (i, p) in config.bad_word_patterns.iter().enumerate() {
        if let Some(re) = compile_safe_regex(p, "i") {
            m.patterns.push(re);
            m.pattern_index.push(i);
        }
    }
    m
}

/// A normalised config and its compiled matchers.
#[derive(Debug, Clone)]
pub struct CompiledConfig {
    pub config: AutomodConfig,
    pub matchers: Matchers,
}

/// The per-app memo of compiled configs (the JS `WeakMap` keyed by the frozen `automodConfig` object): per guild, the
/// raw value it was compiled from.
#[derive(Default)]
struct CompiledMemo {
    by_raw: Mutex<HashMap<String, (Value, Arc<CompiledConfig>)>>,
    default: Mutex<Option<Arc<CompiledConfig>>>,
}

/// Compiles a raw `automodConfig` value (no memo).
pub fn compile_config(raw: Option<&Value>) -> Arc<CompiledConfig> {
    let config = normalize_config(raw);
    let matchers = build_matchers(&config);
    Arc::new(CompiledConfig { config, matchers })
}

/// `getAutomodConfig(settings)`: the normalised config and compiled matchers of a settings row, memoised per guild
/// and raw value (a write makes a new value, which recompiles).
pub fn get_automod_config(app: &App, settings: Option<&GuildSettings>) -> Arc<CompiledConfig> {
    let memo = app.ext::<CompiledMemo>();
    match settings.and_then(|s| s.automod_config.as_ref()).filter(|v| v.is_object() || v.is_array()) {
        Some(raw) => {
            let guild = settings.map(|s| s.guild_id.clone()).unwrap_or_default();
            if let Some((v, hit)) = memo.by_raw.lock().unwrap_or_else(|e| e.into_inner()).get(&guild)
                && v == raw
            {
                return hit.clone();
            }
            let compiled = compile_config(Some(raw));
            let mut map = memo.by_raw.lock().unwrap_or_else(|e| e.into_inner());
            if map.len() > 10_000 {
                map.clear();
            }
            map.insert(guild, (raw.clone(), compiled.clone()));
            compiled
        }
        None => {
            let mut d = memo.default.lock().unwrap_or_else(|e| e.into_inner());
            d.get_or_insert_with(|| compile_config(None)).clone()
        }
    }
}

// ── In-process state (per app) ────────────────────────────────────────────────

/// One automod punishment per member per 30 s, unless the new one is more severe (`claimPunishment`).
#[derive(Default)]
pub struct PunishmentSlots(Mutex<IndexMap<String, (i32, i64)>>);

impl PunishmentSlots {
    /// True when the punishment may be applied. A failed punishment still occupies the slot.
    pub fn claim(&self, guild_id: &str, user_id: &str, rank: i32, now: i64) -> bool {
        let key = format!("{guild_id}:{user_id}");
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((prev_rank, until)) = map.get(&key)
            && *until > now
            && *prev_rank >= rank
        {
            return false;
        }
        map.shift_remove(&key);
        map.insert(key, (rank, now + PUNISH_WINDOW_MS));
        if map.len() > 10_000 {
            map.shift_remove_index(0);
        }
        true
    }
}

struct ThrottleSet {
    notice: bot_util::async_util::Throttle,
    delete_log: bot_util::async_util::Throttle,
    not_deleted: bot_util::async_util::Throttle,
    warn: bot_util::async_util::Throttle,
}

impl Default for ThrottleSet {
    fn default() -> Self {
        use bot_util::async_util::create_throttle;
        Self { notice: create_throttle(10_000), delete_log: create_throttle(10_000), not_deleted: create_throttle(60_000), warn: create_throttle(600_000) }
    }
}

/// The notice, log and warning throttles (replaced as a whole by a reset).
#[derive(Default)]
struct Throttles(std::sync::RwLock<Arc<ThrottleSet>>);

impl Throttles {
    fn get(&self) -> Arc<ThrottleSet> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn reset(&self) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ThrottleSet::default());
    }
}

/// `claimPunishment(guildId, userId, rank, now)` on the app's slots.
pub fn claim_punishment(app: &App, guild_id: &str, user_id: &str, rank: i32, now: i64) -> bool {
    app.ext::<PunishmentSlots>().claim(guild_id, user_id, rank, now)
}

/// `resetAutomodState()`: clears the compiled-config memo, the punishment slots and the throttles of this app (tests).
pub fn reset_automod_state(app: &App) {
    let memo = app.ext::<CompiledMemo>();
    memo.by_raw.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *memo.default.lock().unwrap_or_else(|e| e.into_inner()) = None;
    app.ext::<PunishmentSlots>().0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    app.ext::<Throttles>().reset();
}

fn allow(app: &App, pick: impl Fn(&ThrottleSet) -> &bot_util::async_util::Throttle, key: &str) -> bool {
    let th = app.ext::<Throttles>().get();
    pick(&th).allow_at(key, app.clock().now_ms())
}

fn warn_once(app: &App, key: &str, text: String) {
    if allow(app, |t| &t.warn, key) {
        bot_util::log_warn!(text);
    }
}

// ── Message views ─────────────────────────────────────────────────────────────

/// An attachment as the rules see it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentView {
    /// `attachment.spoiler` (the file name starts with `SPOILER_`).
    pub spoiler: bool,
}

/// A forwarded snapshot: its text and attachments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotView {
    pub content: String,
    pub attachments: Vec<AttachmentView>,
}

/// The fields of a message the rules read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutomodMessage {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author_bot: bool,
    pub author_username: String,
    pub author_tag: String,
    pub content: String,
    pub attachments: Vec<AttachmentView>,
    pub snapshots: Vec<SnapshotView>,
    /// `mentions.users` keys (unique, in order).
    pub mention_user_ids: Vec<String>,
    /// `mentions.roles.size`.
    pub mention_role_count: usize,
    /// `mentions.everyone`.
    pub mention_everyone: bool,
    /// `mentions.repliedUser?.id`.
    pub replied_user_id: Option<String>,
    pub webhook: bool,
    pub system: bool,
}

fn attachment_view(a: &serenity::all::Attachment) -> AttachmentView {
    AttachmentView { spoiler: a.filename.starts_with("SPOILER_") }
}

impl AutomodMessage {
    /// The view of a serenity message.
    pub fn of(message: &Message, guild_id: GuildId) -> Self {
        let mut users = Vec::new();
        for u in &message.mentions {
            let id = u.id.to_string();
            if !users.contains(&id) {
                users.push(id);
            }
        }
        let mut roles = HashSet::new();
        for r in &message.mention_roles {
            roles.insert(*r);
        }
        use serenity::all::MessageType;
        let system = !matches!(message.kind, MessageType::Regular | MessageType::InlineReply | MessageType::ChatInputCommand | MessageType::ContextMenuCommand);
        Self {
            id: message.id.to_string(),
            guild_id: guild_id.to_string(),
            channel_id: message.channel_id.to_string(),
            author_id: message.author.id.to_string(),
            author_bot: message.author.bot,
            author_username: message.author.name.clone(),
            author_tag: bot_core::adapters::user_tag(&message.author),
            content: message.content.clone(),
            attachments: message.attachments.iter().map(attachment_view).collect(),
            snapshots: message
                .message_snapshots
                .iter()
                .map(|s| SnapshotView { content: s.content.clone(), attachments: s.attachments.iter().map(attachment_view).collect() })
                .collect(),
            mention_user_ids: users,
            mention_role_count: roles.len(),
            mention_everyone: message.mention_everyone,
            replied_user_id: message.referenced_message.as_ref().map(|m| m.author.id.to_string()),
            webhook: message.webhook_id.is_some(),
            system,
        }
    }

    /// `effectiveText(message)`: the own content plus every forwarded snapshot's content.
    pub fn effective_text(&self) -> String {
        bot_util::text::effective_text(Some(&self.content), self.snapshots.iter().map(|s| Some(s.content.as_str())))
    }

    /// `message.url`.
    pub fn url(&self) -> String {
        format!("https://discord.com/channels/{}/{}/{}", self.guild_id, self.channel_id, self.id)
    }
}

/// The channel ids a rule scope sees, and whether the channel is a thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelScope {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub is_thread: bool,
    /// A thread's parent's category (`channel.parent.parentId`).
    pub thread_category_id: Option<String>,
}

impl ChannelScope {
    /// The scope of a channel; a thread's category is read from the cached parent.
    pub fn of(app: &App, guild_id: GuildId, channel: &GuildChannel) -> Self {
        let is_thread = bot_core::permissions::is_thread(channel);
        let parent_id = channel.parent_id.map(|p| p.to_string());
        let thread_category_id = if is_thread {
            channel
                .parent_id
                .and_then(|p| bot_core::interaction_helpers::cached_channel(app, guild_id, p))
                .and_then(|parent| parent.parent_id.map(|c| c.to_string()))
        } else {
            None
        };
        Self { id: channel.id.to_string(), name: channel.name.clone(), parent_id, is_thread, thread_category_id }
    }
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Invites. No nested quantifiers; an underscore before the link is allowed (markdown italics).
static INVITE_RE: LazyLock<Regex> = LazyLock::new(|| {
    js_re::re(r"(?<![A-Za-z0-9.-])(?:https?:\/\/)?(?:(?:www|ptb|canary)\.)?(?:discord(?:app)?\.com\/invite|discord\.gg)\/([a-z0-9-]{2,32})", "gi")
});

/// Every invite code in `text` (original case).
pub fn find_invites(text: &str) -> Vec<String> {
    INVITE_RE.find_iter(text).filter_map(|m| js_re::group(&m, text, 1).map(str::to_string)).collect()
}

static URL_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r#"(?<![A-Za-z0-9])https?:\/\/[^\s<>"'`|]+"#, "gi"));
static WWW_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"(?<![A-Za-z0-9./-])www\.[a-z0-9-]+(?:\.[a-z0-9-]+)+", "gi"));
static TRAILING_MARKS_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r#"[)\].,!?:;'"*_~]+$"#, ""));

/// An exact or dot-suffix match on a parsed hostname: netflix.com is not x.com.
pub fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The distinct hostnames of the links in `text`. Discord invite URLs belong to the invites rule.
pub fn extract_link_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let Ok(u) = url::Url::parse(raw) else { return };
        let host = bot_util::js::lower(u.host_str().unwrap_or_default());
        let host = host.strip_suffix('.').unwrap_or(&host).to_string();
        if host.is_empty() {
            return;
        }
        if host == "discord.gg" || ((host_matches(&host, "discord.com") || host_matches(&host, "discordapp.com")) && u.path().starts_with("/invite/")) {
            return;
        }
        if !out.contains(&host) {
            out.push(host);
        }
    };
    for m in URL_RE.find_iter(text) {
        let raw = &text[m.range()];
        push(&js_re::replace_all(&TRAILING_MARKS_RE, raw, |_, _| String::new()));
    }
    for m in WWW_RE.find_iter(text) {
        push(&format!("https://{}", &text[m.range()]));
    }
    out
}

static CUSTOM_EMOJI_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"<a?:\w{2,32}:\d{17,20}>", "g"));
static MENTION_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"<(?:@[!&]?|#)\d{17,20}>", "g"));
static URL_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"\bhttps?:\/\/\S+", "gi"));
static CASED_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"[\p{Lu}\p{Ll}\p{Lt}]", "gu"));
static UPPER_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"[\p{Lu}\p{Lt}]", "gu"));

/// `capsStats(content)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsStats {
    pub letters: usize,
    pub upper: usize,
    pub percent: i64,
}

/// Capitals share: ignores custom emoji, mentions and URLs; counts only cased letters.
pub fn caps_stats(content: &str) -> CapsStats {
    let t = js_re::replace_all(&CUSTOM_EMOJI_RE, content, |_, _| String::new());
    let t = js_re::replace_all(&MENTION_TOKEN_RE, &t, |_, _| String::new());
    let t = js_re::replace_all(&URL_TOKEN_RE, &t, |_, _| String::new());
    let letters = js_re::count(&CASED_RE, &t);
    let upper = js_re::count(&UPPER_RE, &t);
    let percent = if letters > 0 { bot_util::text::js_round((upper as f64 * 100.0) / letters as f64) as i64 } else { 0 };
    CapsStats { letters, upper, percent }
}

static EMOJI_SEGMENT_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"\p{Extended_Pictographic}|\p{Regional_Indicator}|\u{20E3}", "u"));

/// Custom emoji tokens plus grapheme clusters that hold a pictographic, a regional indicator or a keycap.
pub fn count_emojis(content: &str) -> usize {
    let custom = js_re::count(&CUSTOM_EMOJI_RE, content);
    let rest = js_re::replace_all(&CUSTOM_EMOJI_RE, content, |_, _| " ".to_string());
    custom + rest.graphemes(true).filter(|g| js_re::test(&EMOJI_SEGMENT_RE, g)).count()
}

/// Pairs of "||", plus attachments flagged as spoilers.
pub fn count_spoilers(content: &str, attachments: &[AttachmentView]) -> usize {
    content.matches("||").count() / 2 + attachments.iter().filter(|a| a.spoiler).count()
}

/// Parsed mentions only: unique users other than the author, plus roles, plus 1 for a real @everyone/@here ping. A
/// reply ping does not count unless the content also mentions that user.
pub fn count_mentions(message: &AutomodMessage) -> usize {
    let author = message.author_id.as_str();
    let mut users = message.mention_user_ids.iter().filter(|id| id.as_str() != author).count() as i64;
    if let Some(rid) = message.replied_user_id.as_deref()
        && rid != author
        && message.mention_user_ids.iter().any(|u| u == rid)
        && !message.content.contains(&format!("<@{rid}>"))
        && !message.content.contains(&format!("<@!{rid}>"))
    {
        users -= 1;
    }
    (users + message.mention_role_count as i64 + i64::from(message.mention_everyone)).max(0) as usize
}

static VARIATION_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"[\u{FE00}-\u{FE0F}\u{20E3}]|[\u{E0100}-\u{E01EF}]", "gu"));
static COMBINING_RUN_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"[\p{Mn}\p{Me}]+", "gu"));

/// The longest run of combining marks after NFC, ignoring variation selectors and the keycap mark.
pub fn max_combining_run(content: &str) -> usize {
    let nfc: String = content.nfc().collect();
    let t = js_re::replace_all(&VARIATION_RE, &nfc, |_, _| String::new());
    COMBINING_RUN_RE.find_iter(&t).map(|m| t[m.range()].chars().count()).max().unwrap_or(0)
}

/// Fingerprint for repeated text; `None` for empty content, so images and stickers never count.
pub fn duplicate_fingerprint(content: &str) -> Option<String> {
    let fp = collapse_spaces(&bot_util::js::lower(&strip_invisible(&nfkc(content))));
    let fp = bot_util::js::trim(&fp).to_string();
    (!fp.is_empty()).then_some(fp)
}

// ── Scope ─────────────────────────────────────────────────────────────────────

/// Channel ids a rule scope is compared against: thread -> [thread, parent, category]; channel -> [channel, category].
pub fn scope_channel_ids(channel: Option<&ChannelScope>) -> Vec<String> {
    let Some(c) = channel else { return Vec::new() };
    let mut ids = vec![c.id.clone()];
    if c.is_thread
        && let Some(p) = &c.parent_id
    {
        ids.push(p.clone());
    }
    let category = if c.is_thread { c.thread_category_id.clone() } else { c.parent_id.clone() };
    if let Some(cat) = category {
        ids.push(cat);
    }
    ids
}

/// Whether a channel is a honeypot or a thread in one. Categories never count.
pub fn is_honeypot_channel(channel: Option<&ChannelScope>, honeypot: &HoneypotConfig) -> bool {
    let Some(c) = channel else { return false };
    if honeypot.channel_ids.is_empty() {
        return false;
    }
    if honeypot.channel_ids.contains(&c.id) {
        return true;
    }
    c.is_thread && c.parent_id.as_ref().is_some_and(|p| honeypot.channel_ids.contains(p))
}

/// Whether a rule applies to a member in a channel, by the rule's role and channel modes.
pub fn rule_applies(rule: &RuleConfig, role_ids: &[String], channel_ids: &[String]) -> bool {
    let role_hit = rule.role_ids.iter().any(|id| role_ids.contains(id));
    let chan_hit = rule.channel_ids.iter().any(|id| channel_ids.contains(id));
    (if rule.role_mode == "only" { role_hit } else { !role_hit }) && (if rule.channel_mode == "only" { chan_hit } else { !chan_hit })
}

/// Staff bypass: the owner, admins and immune roles (`isImmune`), MODERATOR level (mod roles, Kick/Ban/Moderate
/// Members, the bot owner) and members with Manage Messages.
pub fn is_staff(app: &App, guild_id: GuildId, member: Option<&Member>, settings: Option<&GuildSettings>) -> bool {
    let Some(member) = member else { return false };
    if feat_moderation::moderation_service::is_immune_member(app, guild_id, member, settings) {
        return true;
    }
    let cache = app.discord().map(|d| d.cache.clone());
    let access = MemberAccess::from_cache(cache.as_deref(), guild_id, member);
    if resolve_permission_level(Some(&access), settings, app.config().bot_owner_id.as_deref()) >= PermissionLevel::Moderator {
        return true;
    }
    access.permissions.is_some_and(|p| bot_core::permissions::js_has(p, serenity::all::Permissions::MANAGE_MESSAGES))
}

// ── Rule names and descriptions ───────────────────────────────────────────────

fn rule_alias(s: &str) -> Option<&'static str> {
    Some(match s {
        "badwords" | "bad words" => "badWords",
        "repeated" | "repeated text" | "duplicates" | "duplicate" => "repeatedText",
        "invite" | "server invites" => "invites",
        "link" | "external links" => "links",
        "personal info" | "personal" | "pii" | "dox" | "doxx" | "doxxing" | "doxing" | "phone" | "phone numbers" | "address" => "personalInfo",
        "capitals" | "excessive caps" => "caps",
        "emoji" | "excessive emojis" => "emojis",
        "spoiler" | "excessive spoilers" => "spoilers",
        "mention" | "excessive mentions" => "mentions",
        "anti-spam" | "antispam" | "rate" => "spam",
        _ => return None,
    })
}

static PAREN_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"\s*\([^)]*\)\s*$", ""));
static LEADING_SYMBOLS_RE: LazyLock<Regex> = LazyLock::new(|| js_re::re(r"^[^\p{L}\p{N}]+", "u"));

/// Resolves a rule key, label or alias (also "Label (key)" as pasted from autocomplete) to a rule key.
pub fn resolve_rule_key(input: &str) -> Option<&'static str> {
    let find = |s: &str| -> Option<&'static str> {
        for m in &RULES {
            if bot_util::js::lower(m.key) == s || bot_util::js::lower(m.label) == s {
                return Some(m.key);
            }
        }
        rule_alias(s)
    };
    let raw = bot_util::js::lower(bot_util::js::trim(input));
    if raw.is_empty() {
        return None;
    }
    let stripped = js_re::replace_all(&PAREN_SUFFIX_RE, &raw, |_, _| String::new());
    let stripped = js_re::replace_all(&LEADING_SYMBOLS_RE, &stripped, |_, _| String::new());
    find(&raw).or_else(|| find(bot_util::js::trim(&stripped)))
}

/// The threshold of a rule in words, e.g. "≥ 70% capitals".
pub fn format_threshold(key: &str, value: i64) -> String {
    match key {
        "caps" => format!("≥ {value}% capitals"),
        "emojis" => format!("≥ {value} emojis"),
        "spoilers" => format!("≥ {value} spoilers"),
        "mentions" => format!("≥ {value} mentions"),
        "zalgo" => format!("≥ {value} stacked marks"),
        "repeatedText" => format!("{value} identical messages in 30 s"),
        "spam" => format!("{value} messages in 5 s"),
        _ => value.to_string(),
    }
}

fn duration_long(seconds: i64) -> String {
    bot_util::parse_duration::format_duration_long(seconds as f64 * 1000.0)
}

/// One status line for a rule: emoji, label, action, then the optional threshold, timeout and scope parts.
pub fn describe_rule_line(key: &str, rule: &RuleConfig, config: &AutomodConfig) -> String {
    let Some(meta) = rule_meta(key) else { return key.to_string() };
    let mut line = format!("{} **{}** — {}", meta.emoji, meta.label, action_label(&rule.action).unwrap_or(&rule.action));
    if meta.threshold.is_some() && rule.action != "off" {
        line.push_str(&format!(" · {}", format_threshold(key, rule.threshold.unwrap_or_default())));
    }
    if rule.action == "timeout" {
        line.push_str(&format!(" · {}", duration_long(rule.timeout_seconds)));
    }
    if !rule.role_ids.is_empty() || rule.role_mode == "only" {
        line.push_str(&format!(" · roles: {} {}", rule.role_mode, rule.role_ids.len()));
    }
    if !rule.channel_ids.is_empty() || rule.channel_mode == "only" {
        line.push_str(&format!(" · channels: {} {}", rule.channel_mode, rule.channel_ids.len()));
    }
    if key == "badWords" {
        line.push_str(&format!(" · {} words, {} patterns", config.bad_words.len(), config.bad_word_patterns.len()));
    }
    line
}

/// The honeypot channels and action ("#a, #b → action"), or how to set one up.
pub fn describe_honeypot_value(config: &AutomodConfig) -> String {
    let hp = &config.honeypot;
    if hp.channel_ids.is_empty() {
        return "Off. Set one up with `/automod honeypot add`.".into();
    }
    let timeout = if hp.action == "timeout" { format!(" · {}", duration_long(hp.timeout_seconds)) } else { String::new() };
    let channels = hp.channel_ids.iter().map(|id| format!("<#{id}>")).collect::<Vec<_>>().join(", ");
    format!("{channels} → {}{timeout}", honeypot_action_label(&hp.action).unwrap_or(&hp.action))
}

/// The honeypot line for replies: "🍯 **Honeypot** — …".
pub fn describe_honeypot(config: &AutomodConfig) -> String {
    format!("{HONEYPOT_EMOJI} **{HONEYPOT_LABEL}** — {}", describe_honeypot_value(config))
}

/// The sentence that explains what the External Links rule does with the current domain lists.
pub fn describe_links_mode(config: &AutomodConfig) -> &'static str {
    if config.blocked_domains.is_empty() {
        "Links: every link is blocked except allowed domains."
    } else {
        "Links: only blocked domains are removed."
    }
}

// ── Detectors ─────────────────────────────────────────────────────────────────

/// A rule that matched: `{ rule, action, detail, timeoutSeconds }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    pub rule: String,
    pub action: String,
    pub detail: Option<String>,
    pub timeout_seconds: Option<i64>,
}

impl RuleMatch {
    /// A bare `{ rule, action }` (tests and the dashboard).
    pub fn new(rule: &str, action: &str) -> Self {
        Self { rule: rule.into(), action: action.into(), detail: None, timeout_seconds: None }
    }
}

fn make_match(config: &AutomodConfig, key: &str, detail: &str) -> RuleMatch {
    let r = config.rule(key);
    RuleMatch { rule: key.into(), action: r.action, detail: Some(clamp_text(detail, 200)), timeout_seconds: Some(r.timeout_seconds) }
}

/// Bad words (whole-word; entries in scripts without word spaces match as a substring) then regex patterns
/// (sandboxed).
pub fn check_bad_words(content: &str, config: &AutomodConfig, matchers: &Matchers) -> Option<RuleMatch> {
    let t = text_for_words(content);
    if t.is_empty() {
        return None;
    }
    if let Some(re) = &matchers.bad_words_re {
        if let Some(m) = re.find(&t) {
            return Some(make_match(config, "badWords", &format!("word \"{}\"", clamp_text(&t[m.range()], 40))));
        }
    } else if let Some(words) = &matchers.fallback_words {
        let lower = bot_util::js::lower(&t);
        if let Some(w) = words.iter().find(|w| if is_spaceless_word(w) { lower.contains(&bot_util::js::lower(w)) } else { contains_word(&t, w, false) }) {
            return Some(make_match(config, "badWords", &format!("word \"{}\"", clamp_text(w, 40))));
        }
    }
    if !matchers.patterns.is_empty() {
        let list: Vec<Option<Arc<SafeRegex>>> = matchers.patterns.iter().cloned().map(Some).collect();
        let k = safe_regex_first_match(&list, &t, Some(MatchOptions { timeout_ms: 50, input_cap: PATTERN_INPUT_CAP }));
        if k >= 0 {
            let idx = matchers.pattern_index.get(k as usize).copied().unwrap_or(0);
            return Some(make_match(config, "badWords", &format!("pattern #{}", idx + 1)));
        }
    }
    None
}

/// What the invites rule knows about the guild: its vanity code and its own invite codes (`guild.invites.cache`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuildInvites {
    pub vanity_code: Option<String>,
    pub codes: HashSet<String>,
}

fn check_invites(content: &str, config: &AutomodConfig, guild: &GuildInvites) -> Option<RuleMatch> {
    let vanity = guild.vanity_code.as_deref().map(bot_util::js::lower);
    for code in find_invites(&text_for_links(content)) {
        if vanity.as_deref().is_some_and(|v| bot_util::js::lower(&code) == v) {
            continue;
        }
        if guild.codes.contains(&code) {
            continue; // this guild's own invite; no API call
        }
        return Some(make_match(config, "invites", &format!("invite discord.gg/{code}")));
    }
    None
}

fn check_links(content: &str, config: &AutomodConfig) -> Option<RuleMatch> {
    for host in extract_link_hosts(&text_for_links(content)) {
        if config.allowed_domains.iter().any(|d| host_matches(&host, d)) {
            continue;
        }
        // No blocked domain at all: allowlist mode, every non-allowed link is a violation.
        if config.blocked_domains.is_empty() || config.blocked_domains.iter().any(|d| host_matches(&host, d)) {
            return Some(make_match(config, "links", &format!("link to {host}")));
        }
    }
    None
}

fn run_detector(key: &str, message: &AutomodMessage, config: &AutomodConfig, matchers: &Matchers, guild: &GuildInvites) -> Option<RuleMatch> {
    let text = || message.effective_text();
    let threshold = || config.rule(key).threshold.unwrap_or_default();
    match key {
        "badWords" => check_bad_words(&text(), config, matchers),
        "invites" => check_invites(&text(), config, guild),
        "links" => check_links(&text(), config),
        "personalInfo" => {
            let kinds = bot_util::personal_info::find_personal_info(&text());
            (!kinds.is_empty()).then(|| make_match(config, "personalInfo", &bot_util::personal_info::describe_personal_info(&kinds)))
        }
        "caps" => {
            let s = caps_stats(&text());
            (s.letters >= CAPS_MIN_LETTERS && s.percent >= threshold()).then(|| make_match(config, "caps", &format!("{}% capitals", s.percent)))
        }
        "emojis" => {
            let n = count_emojis(&text()) as i64;
            let limit = threshold();
            (n >= limit).then(|| make_match(config, "emojis", &format!("{n} emojis (limit {limit})")))
        }
        "spoilers" => {
            // Each snapshot is counted on its own, so a "||" opened in one part never pairs with one in another.
            let mut n = count_spoilers(&message.content, &message.attachments);
            for s in &message.snapshots {
                n += count_spoilers(&s.content, &s.attachments);
            }
            let n = n as i64;
            let limit = threshold();
            (n >= limit).then(|| make_match(config, "spoilers", &format!("{n} spoilers (limit {limit})")))
        }
        "mentions" => {
            let n = count_mentions(message) as i64;
            let limit = threshold();
            (n >= limit).then(|| make_match(config, "mentions", &format!("{n} mentions (limit {limit})")))
        }
        "zalgo" => {
            let n = max_combining_run(&text()) as i64;
            (n >= threshold()).then(|| make_match(config, "zalgo", &format!("zalgo text ({n} stacked marks)")))
        }
        _ => None,
    }
}

const CONTENT_RULES: [&str; 9] = ["badWords", "invites", "links", "personalInfo", "caps", "emojis", "spoilers", "mentions", "zalgo"];

/// Runs the content rules in `active_keys`, in [`RULE_KEYS`] order. No I/O (patterns run in a bounded worker). No
/// text skips every rule except spoilers (spoiler attachments). The text is the content plus every forwarded
/// snapshot's content.
pub fn evaluate_content_rules(message: &AutomodMessage, active_keys: &[&str], config: &AutomodConfig, matchers: &Matchers, guild: &GuildInvites) -> Vec<RuleMatch> {
    let has_text = !message.effective_text().is_empty();
    let mut out = Vec::new();
    for key in RULE_KEYS {
        if !active_keys.contains(&key) || !CONTENT_RULES.contains(&key) {
            continue;
        }
        if !has_text && key != "spoilers" {
            continue;
        }
        if let Some(m) = run_detector(key, message, config, matchers, guild) {
            out.push(m);
        }
    }
    out
}

/// `sha1(text)` as hex, first 16 characters.
pub fn sha16(text: &str) -> String {
    use sha1::Digest;
    let digest = sha1::Sha1::digest(text.as_bytes());
    hex::encode(digest)[..16].to_string()
}

/// `automod:dup:{guild}:{user}:{sha16(fingerprint)}`.
pub fn dup_key(guild_id: &str, user_id: &str, fingerprint: &str) -> String {
    format!("automod:dup:{guild_id}:{user_id}:{}", sha16(fingerprint))
}

/// `automod:rate:{guild}:{user}`.
pub fn rate_key(guild_id: &str, user_id: &str) -> String {
    format!("automod:rate:{guild_id}:{user_id}")
}

/// Repeated text over a Redis sliding window. A `None` count (Redis down) skips the rule: fail open.
pub async fn check_repeated_text(app: &App, message: &AutomodMessage, config: &AutomodConfig) -> Option<RuleMatch> {
    let fp = duplicate_fingerprint(&message.effective_text())?;
    if !app.cache().available() {
        return None;
    }
    let key = dup_key(&message.guild_id, &message.author_id, &fp);
    let count = app.cache().sliding_window(&key, REPEAT_WINDOW_MS, Some(&message.id)).await?;
    (count >= config.rule("repeatedText").threshold.unwrap_or(i64::MAX))
        .then(|| make_match(config, "repeatedText", &format!("same message {count} times in 30 s")))
}

/// Message rate over a Redis sliding window. Every message counts, whatever it holds. Fails open.
pub async fn check_spam(app: &App, message: &AutomodMessage, config: &AutomodConfig) -> Option<RuleMatch> {
    if !app.cache().available() {
        return None;
    }
    let key = rate_key(&message.guild_id, &message.author_id);
    let count = app.cache().sliding_window(&key, SPAM_WINDOW_MS, Some(&message.id)).await?;
    (count >= config.rule("spam").threshold.unwrap_or(i64::MAX)).then(|| make_match(config, "spam", &format!("{count} messages in 5 s")))
}

// ── Outcome resolution ────────────────────────────────────────────────────────

/// What automod does about one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub primary: RuleMatch,
    pub others: Vec<RuleMatch>,
    pub deletes: bool,
    /// `WARN | TIMEOUT | KICK | BAN | SOFTBAN`.
    pub punishment: Option<String>,
    pub punishment_needs_delete: bool,
    pub timeout_seconds: Option<i64>,
    /// Honeypot bans: the message history to clear.
    pub delete_message_seconds: Option<i64>,
    /// False for the honeypot: no in-channel notice.
    pub notice: bool,
}

fn rank_of(m: &RuleMatch) -> i32 {
    punishment_of(&m.action).and_then(punishment_rank).unwrap_or(0)
}

/// Several matches still mean one action: delete when any rule asks for it, apply the strongest punishment (ties go
/// to the first rule in [`RULE_KEYS`] order).
pub fn resolve_outcome(matches: &[RuleMatch]) -> Option<Outcome> {
    if matches.is_empty() {
        return None;
    }
    let mut ordered = matches.to_vec();
    ordered.sort_by_key(|m| rule_index(&m.rule));
    let deletes = ordered.iter().any(|m| is_deleting_action(&m.action));
    let mut best = 0;
    for (i, m) in ordered.iter().enumerate() {
        if rank_of(m) > rank_of(&ordered[best]) {
            best = i;
        }
    }
    let primary_idx =
        if rank_of(&ordered[best]) > 0 { best } else { ordered.iter().position(|m| is_deleting_action(&m.action)).unwrap_or(0) };
    let primary = ordered[primary_idx].clone();
    let punishment = (rank_of(&primary) > 0).then(|| punishment_of(&primary.action).unwrap_or_default().to_string());
    let timeout_seconds = (punishment.as_deref() == Some("TIMEOUT")).then(|| {
        ordered.iter().filter(|m| m.action == "timeout").map(|m| m.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS)).max().unwrap_or(DEFAULT_TIMEOUT_SECONDS)
    });
    let others = ordered.iter().enumerate().filter(|(i, _)| *i != primary_idx).map(|(_, m)| m.clone()).collect();
    Some(Outcome {
        punishment_needs_delete: is_deleting_action(&primary.action),
        primary,
        others,
        deletes,
        punishment,
        timeout_seconds,
        delete_message_seconds: None,
        notice: true,
    })
}

/// The outcome for a post in a honeypot channel. The punishment never waits on the delete, and no in-channel notice
/// is sent.
pub fn honeypot_outcome(honeypot: &HoneypotConfig) -> Outcome {
    let action = if HONEYPOT_ACTIONS.contains(&honeypot.action.as_str()) { honeypot.action.clone() } else { "softban".into() };
    let punishment = honeypot_punishment(&action).unwrap_or("SOFTBAN").to_string();
    let is_ban = punishment == "BAN" || punishment == "SOFTBAN";
    Outcome {
        timeout_seconds: (punishment == "TIMEOUT").then_some(honeypot.timeout_seconds),
        primary: RuleMatch { rule: "honeypot".into(), action, detail: Some("Posted in a honeypot channel".into()), timeout_seconds: None },
        others: Vec::new(),
        deletes: true,
        punishment: Some(punishment),
        punishment_needs_delete: false,
        delete_message_seconds: is_ban.then_some(HONEYPOT_DELETE_MESSAGE_SECONDS),
        notice: false,
    }
}

fn meta_emoji(key: &str) -> Option<&'static str> {
    rule_meta(key).map(|m| m.emoji).or((key == "honeypot").then_some(HONEYPOT_EMOJI))
}

/// `outcomeHasPersonalInfo(outcome)`: whether the Personal Info rule matched (its logs must not repeat the details).
pub fn outcome_has_personal_info(outcome: &Outcome) -> bool {
    outcome.primary.rule == "personalInfo" || outcome.others.iter().any(|m| m.rule == "personalInfo")
}

/// `logContent(outcome, text)`: the message text as automod logs show it, personal details blanked out when the
/// Personal Info rule matched.
pub fn log_content(outcome: &Outcome, text: &str) -> String {
    if outcome_has_personal_info(outcome) { bot_util::personal_info::redact_personal_info(text) } else { text.to_string() }
}

/// `ruleLabel(key)`.
pub fn rule_label(key: &str) -> String {
    match rule_meta(key) {
        Some(m) => m.label.to_string(),
        None if key == "honeypot" => HONEYPOT_LABEL.to_string(),
        None => key.to_string(),
    }
}

fn also_labels(outcome: &Outcome) -> String {
    outcome.others.iter().map(|m| rule_label(&m.rule)).collect::<Vec<_>>().join(", ")
}

/// The audit-log reason: "[Automod] Label: detail (also: …) in #channel", at most 500 characters.
pub fn build_reason(outcome: &Outcome, channel_name: &str) -> String {
    let also = if outcome.others.is_empty() { String::new() } else { format!(" (also: {})", also_labels(outcome)) };
    clamp_text(
        &format!("[Automod] {}: {}{also} in #{channel_name}", rule_label(&outcome.primary.rule), outcome.primary.detail.as_deref().unwrap_or_default()),
        500,
    )
}

/// The `logFields` handed to `executeModAction`: always exactly three entries, every value at most 1024 characters.
pub fn build_punishment_log_fields(rule_or_label: &str, detail: Option<&str>, content: &str, channel_id: Option<&str>) -> Vec<(String, String)> {
    let label = match rule_meta(rule_or_label) {
        Some(m) => format!("{} {}", m.emoji, m.label),
        None => rule_or_label.to_string(),
    };
    let wh = channel_id.filter(|c| !c.is_empty()).map(|c| format!(" in <#{c}>")).unwrap_or_default();
    let detail = clamp_text(&format!("{}{wh}", detail.unwrap_or_default()), 1024);
    vec![
        ("Rule".into(), clamp_text(&label, 1024)),
        ("Detail".into(), if detail.is_empty() { "—".into() } else { detail }),
        ("Content".into(), clamp_text(if content.is_empty() { "*[no text]*" } else { content }, 1024)),
    ]
}

// ── Staff-facing output ───────────────────────────────────────────────────────

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// Posts an embed to the configured mod-log channel. Never fails.
pub async fn post_mod_log(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, embed: CreateEmbed) {
    let Some(id) = settings.and_then(|s| s.mod_log_channel_id.as_deref()).and_then(parse_id).map(ChannelId::new) else { return };
    let usable = check_channel_usable(app, guild_id, Some(id), &["ViewChannel", "SendMessages", "EmbedLinks"]);
    if !usable.ok {
        warn_once(
            app,
            &format!("modlog:{guild_id}"),
            format!("Automod cannot post to the mod-log channel in {guild_id}: {}", usable.reason.as_deref().unwrap_or("unusable")),
        );
        return;
    }
    let Some(http) = app.http() else { return };
    let msg = CreateMessage::new().embed(embed).allowed_mentions(CreateAllowedMentions::new());
    if let Err(e) = id.send_message(http.as_ref(), msg).await {
        match bot_core::discord::error_code(&e) {
            Some(code @ (10003 | 50001 | 50013)) => {
                warn_once(app, &format!("modlog:{guild_id}"), format!("Automod cannot post to the mod-log channel in {guild_id} (code {code})"))
            }
            _ => bot_util::log_error!("Automod mod-log post failed".to_string(), bot_core::discord::error_message(&e)),
        }
    }
}

/// What the log embed says (`buildLogEmbed(kind, …)`).
pub struct LogEmbedInput<'a> {
    pub title: &'a str,
    pub message: &'a AutomodMessage,
    pub is_edit: bool,
    pub outcome: &'a Outcome,
    pub problem: Option<String>,
    pub footer_extra: &'a str,
}

/// The mod-log embed: `warn` (yellow) or error (red).
pub fn build_log_embed(warn: bool, input: &LogEmbedInput<'_>) -> CreateEmbed {
    let m = input.message;
    let content = log_content(input.outcome, &m.effective_text());
    let attachments = m.attachments.len() + m.snapshots.iter().map(|s| s.attachments.len()).sum::<usize>();
    let footer = clamp_text(&format!("User ID: {} • Message ID: {}{}", m.author_id, m.id, input.footer_extra), 2048);
    let embed = if warn { warn_embed(input.title, None, Some(&footer)) } else { error_embed(input.title, None, Some(&footer)) };
    let also = if input.outcome.others.is_empty() { String::new() } else { format!("\nAlso matched: {}", also_labels(input.outcome)) };
    let rule_text = format!("{} {}{also}", meta_emoji(&input.outcome.primary.rule).unwrap_or(""), rule_label(&input.outcome.primary.rule));
    let tag = if m.author_tag.is_empty() { if m.author_username.is_empty() { m.author_id.clone() } else { m.author_username.clone() } } else { m.author_tag.clone() };
    let content_text = format!("{}{}", if content.is_empty() { "*[no text]*" } else { &content }, if attachments > 0 { format!("\n📎 {attachments} attachment(s)") } else { String::new() });
    let detail = input.outcome.primary.detail.clone().filter(|d| !d.is_empty()).unwrap_or_else(|| "—".into());
    let mut embed = embed
        .field("Member", clamp_text(&format!("<@{}> `{tag}`", m.author_id), 1024), true)
        .field("Channel", clamp_text(&format!("<#{}>{}", m.channel_id, if input.is_edit { " (edited message)" } else { "" }), 1024), true)
        .field("Rule", clamp_text(bot_util::js::trim(&rule_text), 1024), false)
        .field("Detail", clamp_text(&detail, 1024), false)
        .field("Content", clamp_text(&content_text, 1024), false);
    if let Some(p) = input.problem.as_deref().filter(|p| !p.is_empty()) {
        embed = embed.field("Problem", clamp_text(p, 1024), false);
    }
    embed
}

/// The short in-channel notice: `(text, mention)`. The matched word is never echoed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeInput<'a> {
    pub deleted: bool,
    pub status: Option<&'a str>,
    pub punishment: Option<&'a str>,
    pub author_id: &'a str,
    pub author_username: Option<&'a str>,
    pub label: &'a str,
    pub timeout_seconds: Option<i64>,
}

/// `buildNoticeText(…)` -> `(text, mention)`.
pub fn build_notice_text(p: &NoticeInput<'_>) -> (String, bool) {
    let mention = format!("<@{}>", p.author_id);
    if p.status == Some("applied") {
        if matches!(p.punishment, Some("KICK") | Some("BAN")) {
            let name = clamp_text(p.author_username.unwrap_or(p.author_id), 32);
            let verb = if p.punishment == Some("KICK") { "kicked" } else { "banned" };
            return (format!("🛡\u{FE0F} **{name}** was {verb} by automod: **{}**.", p.label), false);
        }
        if !p.deleted {
            return (format!("⚠\u{FE0F} {mention}, you have been warned: **{}**.", p.label), true);
        }
        if p.punishment == Some("TIMEOUT") {
            return (
                format!(
                    "🛡\u{FE0F} {mention}, your message was removed and you have been timed out for {}: **{}**.",
                    bot_util::parse_duration::format_duration_long(p.timeout_seconds.map(|s| s as f64 * 1000.0).unwrap_or(f64::NAN)),
                    p.label
                ),
                true,
            );
        }
        return (format!("🛡\u{FE0F} {mention}, your message was removed and you have been warned: **{}**.", p.label), true);
    }
    (format!("🛡\u{FE0F} {mention}, your message was removed: **{}**.", p.label), true)
}

async fn send_notice(app: &App, guild_id: GuildId, message: &AutomodMessage, text: String, mention: bool) {
    let Some(channel) = parse_id(&message.channel_id).map(ChannelId::new) else { return };
    if !check_channel_usable(app, guild_id, Some(channel), &["ViewChannel", "SendMessages"]).ok {
        return;
    }
    let Some(http) = app.http() else { return };
    let users: Vec<UserId> = if mention { parse_id(&message.author_id).map(UserId::new).into_iter().collect() } else { Vec::new() };
    let msg = CreateMessage::new().content(clamp_text(&text, 2000)).allowed_mentions(CreateAllowedMentions::new().users(users));
    let Ok(sent) = channel.send_message(http.as_ref(), msg).await else { return };
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(NOTICE_TTL_MS)).await;
        bot_core::deletion_tracker::mark_bot_deletion(&app, &[sent.id.to_string()], Some("automod"), None);
        if let Some(http) = app.http() {
            let _ = http.delete_message(sent.channel_id, sent.id, None).await;
        }
    });
}

// ── Applying an outcome ───────────────────────────────────────────────────────

/// What deleting the offending message did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteState {
    pub deleted: bool,
    pub gone: bool,
    /// `missing` (no permission) or `error`.
    pub problem: Option<&'static str>,
}

async fn delete_offender(app: &App, guild_id: GuildId, message: &AutomodMessage, channel_name: &str, outcome: &Outcome) -> DeleteState {
    let mut state = DeleteState::default();
    let (Some(channel), Some(msg_id)) = (parse_id(&message.channel_id).map(ChannelId::new), parse_id(&message.id).map(MessageId::new)) else {
        state.problem = Some("error");
        return state;
    };
    if !missing_bot_permissions(app, PermTarget::Channel(guild_id, channel), &["ManageMessages"]).is_empty() {
        state.problem = Some("missing");
        warn_once(app, &format!("perm:{guild_id}:{channel}"), format!("Automod cannot delete in #{channel_name} ({guild_id}): missing Manage Messages"));
        return state;
    }
    // Always before the delete call, so log hooks can label it (and blank personal details out of the delete log).
    let reason = if outcome_has_personal_info(outcome) { bot_util::personal_info::PERSONAL_INFO_DELETE_REASON } else { "automod" };
    bot_core::deletion_tracker::mark_bot_deletion(app, std::slice::from_ref(&message.id), Some(reason), None);
    let Some(http) = app.http() else {
        state.problem = Some("error");
        return state;
    };
    match http.delete_message(channel, msg_id, None).await {
        Ok(()) => state.deleted = true,
        Err(e) => match bot_core::discord::error_code(&e) {
            Some(10008 | 10003) => {
                state.deleted = true;
                state.gone = true; // already gone
            }
            Some(50013 | 50001) => state.problem = Some("missing"),
            _ => {
                state.problem = Some("error");
                bot_util::log_warn!(format!("Automod could not delete message {} in {guild_id}: {}", message.id, bot_core::discord::error_message(&e)));
            }
        },
    }
    state
}

/// The punishment seam: `executeModAction`, replaceable in tests (`set_mod_executor`).
pub type ModExecutor = Arc<dyn Fn(App, ModActionInput) -> bot_core::error::BoxFuture<Result<ModSuccess, ModError>> + Send + Sync>;

#[derive(Default)]
struct ExecutorOverride(std::sync::RwLock<Option<ModExecutor>>);

/// Replaces `executeModAction` for this app (tests, tools). Shared by automod and anti-raid.
pub fn set_mod_executor(app: &App, f: ModExecutor) {
    *app.ext::<ExecutorOverride>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// `executeModAction(input)` through the override when one is set.
pub async fn execute_mod_action(app: &App, input: ModActionInput) -> Result<ModSuccess, ModError> {
    let over = app.ext::<ExecutorOverride>().0.read().unwrap_or_else(|e| e.into_inner()).clone();
    match over {
        Some(f) => f(app.clone(), input).await,
        None => feat_moderation::moderation_service::execute_mod_action(app, input).await,
    }
}

/// The hook-context side of the pipeline: `ctx.stop` and `ctx.state.automodAction`.
pub trait AutomodSink: Send + Sync {
    fn stop(&self, reason: &str);
    fn set_action(&self, action: &AutomodAction);
}

impl<E: HookEvent> AutomodSink for HookCtx<E> {
    fn stop(&self, reason: &str) {
        HookCtx::stop(self, reason);
    }

    fn set_action(&self, action: &AutomodAction) {
        self.state.set_automod_action(action);
    }
}

/// What [`apply_outcome`] did (tests and the dashboard).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub delete: DeleteState,
    /// `applied | failed | suppressed | no-member | skipped-delete-failed`.
    pub status: Option<&'static str>,
    pub stopped: bool,
    pub notice: bool,
    pub logged: Option<String>,
}

/// The pieces of a handled message.
pub struct ApplyInput<'a> {
    pub guild_id: GuildId,
    pub message: &'a AutomodMessage,
    pub channel: Option<&'a ChannelScope>,
    pub member: Option<&'a Member>,
    pub settings: Option<&'a GuildSettings>,
    pub outcome: &'a Outcome,
    pub is_edit: bool,
}

/// `applyOutcome(ctx, …)`: delete, stop the hook chain, punish, notice, and at most one mod-log embed.
pub async fn apply_outcome(app: &App, sink: &dyn AutomodSink, input: ApplyInput<'_>) -> Applied {
    let ApplyInput { guild_id, message, channel, member, settings, outcome, is_edit } = input;
    let g = guild_id.to_string();
    // `channel?.name ?? 'unknown'`: a channel known only by its id (not cached) has no name.
    let channel_name = channel.map(|c| c.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "unknown".into());
    let del = if outcome.deletes { delete_offender(app, guild_id, message, &channel_name, outcome).await } else { DeleteState::default() };
    let mut applied = Applied { delete: del.clone(), ..Default::default() };

    // A message that is gone must not reach XP, triggers or translation. Never stop when it survived.
    if del.deleted {
        sink.stop("automod");
        applied.stopped = true;
    }
    sink.set_action(&AutomodAction { rule: outcome.primary.rule.clone(), deleted: del.deleted });

    // Punishment.
    let mut status: Option<&'static str> = None;
    let mut failure: Option<(String, String)> = None;
    if let Some(punishment) = outcome.punishment.as_deref() {
        if outcome.punishment_needs_delete && !del.deleted {
            status = Some("skipped-delete-failed"); // a failed delete never punishes
        } else if let Some(member) = member {
            let rank = punishment_rank(punishment).unwrap_or(0);
            if !claim_punishment(app, &g, &member.user.id.to_string(), rank, app.clock().now_ms()) {
                status = Some("suppressed");
            } else {
                let moderator = feat_moderation::moderation_service::bot_user(app).unwrap_or_else(|| feat_moderation::guild_ops::UserRef::id(""));
                let mut mod_input = ModActionInput::new(g.clone(), member.user.id.to_string(), moderator, punishment);
                mod_input.target_member = Some(member.clone());
                mod_input.target_user = Some(member.user.clone());
                mod_input.reason = Some(build_reason(outcome, &channel_name));
                if punishment == "TIMEOUT" {
                    mod_input.duration_ms = outcome.timeout_seconds.map(|s| s as f64 * 1000.0);
                }
                if punishment == "BAN" || punishment == "SOFTBAN" {
                    mod_input.delete_message_seconds = Some(outcome.delete_message_seconds.unwrap_or(BAN_DELETE_MESSAGE_SECONDS));
                }
                mod_input.source = "automod".into();
                let label = {
                    let also = if outcome.others.is_empty() { String::new() } else { format!(" (also: {})", also_labels(outcome)) };
                    bot_util::js::trim(&format!("{} {}{also}", meta_emoji(&outcome.primary.rule).unwrap_or(""), rule_label(&outcome.primary.rule))).to_string()
                };
                mod_input.log_fields =
                    build_punishment_log_fields(&label, outcome.primary.detail.as_deref(), &log_content(outcome, &message.effective_text()), Some(&message.channel_id));
                match execute_mod_action(app, mod_input).await {
                    Ok(_) => status = Some("applied"),
                    Err(e) => {
                        status = Some("failed");
                        failure = Some((e.code.as_str().to_string(), e.error.clone()));
                    }
                }
            }
        } else {
            status = Some("no-member");
        }
    }
    applied.status = status;
    // A ban that cleared the member's messages removed this one too, even when the delete above could not.
    if !del.deleted && status == Some("applied") && outcome.delete_message_seconds.is_some() {
        sink.stop("automod");
        applied.stopped = true;
    }

    // In-channel notice (a short-lived nicety).
    let label = rule_label(&outcome.primary.rule);
    if outcome.notice && ((del.deleted && !del.gone) || status == Some("applied")) && allow(app, |t| &t.notice, &format!("{g}:{}", message.author_id)) {
        let (text, mention) = build_notice_text(&NoticeInput {
            deleted: del.deleted,
            status,
            punishment: outcome.punishment.as_deref(),
            author_id: &message.author_id,
            author_username: Some(&message.author_username).filter(|u| !u.is_empty()).map(String::as_str),
            label: &label,
            timeout_seconds: outcome.timeout_seconds,
        });
        applied.notice = true;
        send_notice(app, guild_id, message, text, mention).await;
    }

    // Mod log: exactly one embed per handled message at most.
    let word = punishment_word(outcome.punishment.as_deref());
    let failure_text = || {
        let (code, error) = failure.clone().unwrap_or_else(|| ("ERROR".into(), "unknown error".into()));
        format!("{code}: {}", clamp_text(&error, 300))
    };
    let base = |title: &'static str| (title, None::<String>, "");
    let mut embed: Option<(bool, String, Option<String>, &str)> = None;
    if status == Some("applied") || status == Some("suppressed") {
        // moderation's case embed is the only one, or one per burst is enough
    } else if del.deleted && outcome.punishment.is_none() {
        if allow(app, |t| &t.delete_log, &format!("{g}:{}:{}", message.author_id, outcome.primary.rule)) {
            let (title, problem, _) = base("Automod — Message Deleted");
            embed = Some((true, title.into(), problem, " • Further removals for this member and rule are not logged for 10 s"));
        }
    } else if del.deleted && (status == Some("failed") || status == Some("no-member")) {
        let problem = if status == Some("no-member") { "The member is no longer in the server; no punishment was applied.".to_string() } else { failure_text() };
        embed = Some((false, format!("Automod — Message Deleted, {word} Not Applied"), Some(problem), ""));
    } else if let Some(problem) = del.problem {
        if allow(app, |t| &t.not_deleted, &format!("{g}:{}", message.channel_id)) {
            let text = if problem == "missing" {
                format!("I need **Manage Messages** in <#{}>. The message is still there: [jump]({}). No punishment was applied.", message.channel_id, message.url())
            } else {
                format!("Deleting failed (Discord error). The message may still be there: [jump]({}). No punishment was applied.", message.url())
            };
            embed = Some((false, "Automod — Message NOT Deleted".into(), Some(text), ""));
        }
    } else if !outcome.deletes && outcome.punishment.is_some() && status == Some("failed") {
        embed = Some((false, "Automod — Warn Not Applied".into(), Some(failure_text()), ""));
    }
    if let Some((warn, title, problem, footer_extra)) = embed {
        let e = build_log_embed(warn, &LogEmbedInput { title: &title, message, is_edit, outcome, problem, footer_extra });
        applied.logged = Some(title);
        post_mod_log(app, guild_id, settings, e).await;
    }
    applied
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

/// A member lookup that failed for a reason other than "not in the server".
#[derive(Debug)]
pub struct LookupFailed(pub String);

/// Looks a member up: `Ok(Some(member))`, `Ok(None)` when they left, `Err` for a failure that proves nothing.
async fn resolve_member(app: &App, guild_id: GuildId, known: Option<&Member>, user_id: &str) -> Result<Option<Member>, LookupFailed> {
    if let Some(m) = known {
        return Ok(Some(m.clone()));
    }
    let Some(uid) = parse_id(user_id).map(UserId::new) else { return Ok(None) };
    if let Some(d) = app.discord()
        && let Some(m) = d.cache.guild(guild_id).and_then(|g| g.members.get(&uid).cloned())
    {
        return Ok(Some(m));
    }
    let Some(http) = app.http() else { return Err(LookupFailed("no connection".into())) };
    match http.get_member(guild_id, uid).await {
        Ok(m) => Ok(Some(m)),
        // 10007 Unknown Member / 10013 Unknown User: the author left, so they are not staff.
        Err(e) if matches!(bot_core::discord::error_code(&e), Some(10007 | 10013)) => Ok(None),
        Err(e) => Err(LookupFailed(bot_core::discord::error_message(&e))),
    }
}

/// The author when automod may act on them: `(member, roleIds)` (member `None` when they left); `None` for the
/// owner, staff, an exempt role or a lookup that failed.
async fn resolve_subject(
    app: &App,
    guild_id: GuildId,
    owner_id: Option<&str>,
    message: &AutomodMessage,
    known: Option<&Member>,
    settings: Option<&GuildSettings>,
    config: &AutomodConfig,
) -> Option<(Option<Member>, Vec<String>)> {
    if owner_id == Some(message.author_id.as_str()) {
        return None;
    }
    let member = match resolve_member(app, guild_id, known, &message.author_id).await {
        Ok(m) => m,
        Err(LookupFailed(e)) => {
            warn_once(app, &format!("member:{guild_id}"), format!("Automod could not look up a member in {guild_id}: {e}"));
            return None;
        }
    };
    if member.is_some() && is_staff(app, guild_id, member.as_ref(), settings) {
        return None;
    }
    // `member.roles.cache` keys: @everyone first, then the member's roles.
    let role_ids: Vec<String> = match &member {
        Some(m) => std::iter::once(guild_id.to_string()).chain(m.roles.iter().map(|r| r.to_string())).collect(),
        None => Vec::new(),
    };
    if role_ids.iter().any(|id| config.exempt_role_ids.contains(id)) {
        return None;
    }
    Some((member, role_ids))
}

/// The message event automod judges.
#[derive(Debug, Clone)]
pub struct AutomodInput {
    pub guild_id: GuildId,
    pub message: AutomodMessage,
    /// The member the event carried (`ctx.member ?? message.member`).
    pub member: Option<Member>,
    pub channel: Option<ChannelScope>,
    /// Edits: the old message when it was cached.
    pub old_message: Option<AutomodMessage>,
    pub old_known: bool,
}

impl AutomodInput {
    fn scope(app: &App, guild_id: GuildId, channel_id: ChannelId, channel: Option<&GuildChannel>) -> Option<ChannelScope> {
        let ch = channel.cloned().or_else(|| bot_core::interaction_helpers::cached_channel(app, guild_id, channel_id));
        Some(match ch {
            Some(c) => ChannelScope::of(app, guild_id, &c),
            None => ChannelScope { id: channel_id.to_string(), ..Default::default() },
        })
    }

    /// The view of a message whose `mentions.roles` holds only the roles of the cached guild (discord.js's
    /// `MessageMentions` keeps a mentioned role only when `guild.roles.cache` has it). Every id counts when the guild
    /// is not cached.
    pub fn message_of(app: &App, message: &Message, guild_id: GuildId) -> AutomodMessage {
        let mut view = AutomodMessage::of(message, guild_id);
        let cached = bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| {
            let known: HashSet<_> = message.mention_roles.iter().filter(|r| g.roles.contains_key(*r)).collect();
            known.len()
        });
        if let Some(n) = cached {
            view.mention_role_count = n;
        }
        view
    }

    /// From a `messageCreate` payload.
    pub fn from_create(app: &App, e: &MessageCreate) -> Self {
        Self {
            guild_id: e.guild_id,
            message: Self::message_of(app, &e.message, e.guild_id),
            member: e.member.clone(),
            channel: Self::scope(app, e.guild_id, e.channel_id, e.channel.as_ref()),
            old_message: None,
            old_known: false,
        }
    }

    /// From a `messageUpdate` payload.
    pub fn from_update(app: &App, e: &MessageUpdate) -> Self {
        Self {
            guild_id: e.guild_id,
            message: Self::message_of(app, &e.message, e.guild_id),
            member: e.member.clone(),
            channel: Self::scope(app, e.guild_id, e.channel_id, e.channel.as_ref()),
            old_message: e.old_message.as_ref().map(|m| Self::message_of(app, m, e.guild_id)),
            old_known: e.old_known,
        }
    }
}

async fn guild_invites(app: &App, guild_id: GuildId) -> GuildInvites {
    let vanity_code = app.discord().and_then(|d| d.cache.guild(guild_id).and_then(|g| g.vanity_url_code.clone()));
    // `guild.invites.cache`: the invite tracker's snapshot of this guild's invites.
    let codes = feat_logging::invite_tracker::current_snapshot(app, &guild_id.to_string())
        .await
        .map(|s| s.codes.keys().cloned().collect())
        .unwrap_or_default();
    GuildInvites { vanity_code, codes }
}

/// `runAutomod(ctx, { isEdit })`: runs every automod rule against a message (messageCreate) or an edit
/// (messageUpdate). Never fails. Returns what was applied, when anything was.
pub async fn run_automod(app: &App, sink: &dyn AutomodSink, settings: Option<&GuildSettings>, input: &AutomodInput, is_edit: bool) -> Option<Applied> {
    let message = &input.message;
    let guild_id = input.guild_id;
    if message.author_bot || message.webhook || message.system {
        return None;
    }
    let settings = settings?;
    if !settings.automod_enabled {
        return None;
    }
    let compiled = get_automod_config(app, Some(settings));
    let (config, matchers) = (&compiled.config, &compiled.matchers);
    let owner_id = app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.owner_id.to_string()));

    // Honeypot first: it works with every rule off, and an exempt channel or category never shields it.
    if !is_edit && is_honeypot_channel(input.channel.as_ref(), &config.honeypot) {
        let (member, _) = resolve_subject(app, guild_id, owner_id.as_deref(), message, input.member.as_ref(), Some(settings), config).await?;
        let outcome = honeypot_outcome(&config.honeypot);
        return Some(
            apply_outcome(
                app,
                sink,
                ApplyInput { guild_id, message, channel: input.channel.as_ref(), member: member.as_ref(), settings: Some(settings), outcome: &outcome, is_edit },
            )
            .await,
        );
    }
    if active_rule_keys(config).is_empty() {
        return None;
    }

    // Exempt channel: thread, its parent and the category are all compared.
    let channel_ids = scope_channel_ids(input.channel.as_ref());
    if channel_ids.iter().any(|id| config.exempt_channel_ids.contains(id)) {
        return None;
    }

    let (member, role_ids) = resolve_subject(app, guild_id, owner_id.as_deref(), message, input.member.as_ref(), Some(settings), config).await?;

    let active: Vec<&str> = RULE_KEYS
        .iter()
        .copied()
        .filter(|k| {
            let r = config.rule(k);
            r.action != "off" && rule_applies(&r, &role_ids, &channel_ids) && (!is_edit || rule_meta(k).is_some_and(|m| m.on_edit))
        })
        .collect();
    if active.is_empty() {
        return None;
    }

    let needs_invites = active.contains(&"invites")
        && (!find_invites(&text_for_links(&message.effective_text())).is_empty()
            || input.old_message.as_ref().is_some_and(|o| !find_invites(&text_for_links(&o.effective_text())).is_empty()));
    let invites = if needs_invites { guild_invites(app, guild_id).await } else { GuildInvites::default() };

    let mut matches = evaluate_content_rules(message, &active, config, matchers, &invites);

    // A rule the old content already broke is not punished again: a typo fix must not re-warn.
    if is_edit
        && input.old_known
        && let Some(old) = &input.old_message
    {
        let old_matches = evaluate_content_rules(old, &active, config, matchers, &invites);
        matches.retain(|m| !old_matches.iter().any(|o| o.rule == m.rule));
    }

    if !is_edit {
        let repeated = async {
            if active.contains(&"repeatedText") { check_repeated_text(app, message, config).await } else { None }
        };
        let spam = async {
            if active.contains(&"spam") { check_spam(app, message, config).await } else { None }
        };
        let (r, s) = tokio::join!(repeated, spam);
        matches.extend(r); // None = Redis down: fail open
        matches.extend(s);
    }

    let outcome = resolve_outcome(&matches)?;
    Some(
        apply_outcome(
            app,
            sink,
            ApplyInput { guild_id, message, channel: input.channel.as_ref(), member: member.as_ref(), settings: Some(settings), outcome: &outcome, is_edit },
        )
        .await,
    )
}
