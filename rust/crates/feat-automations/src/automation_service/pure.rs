//! The pure helpers of `automationService.js`: normalisers, validators, the condition evaluator, the loop-protection
//! trackers, presets, JSON import / export, descriptions and the Discord error texts. Nothing here does I/O.

use std::collections::{HashMap, HashSet, VecDeque};

use bot_config::limits;
use bot_util::js::{lower, trim, utf16_len};
use bot_util::text::{clamp_text, format_number, join_with_limit, default_more};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, Guild, RoleId};
use unicode_segmentation::UnicodeSegmentation;

use super::tables::*;

// ── Small helpers ────────────────────────────────────────────────────────────

/// `Number.isInteger(v)`: a finite whole number (JSON `3.0` and `1e2` count), as an `i64`.
pub fn js_int(v: &Value) -> Option<i64> {
    let n = v.as_f64()?;
    if !n.is_finite() || n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(n as i64)
}

/// `Number.isInteger(v)` for an optional value.
fn js_int_opt(v: Option<&Value>) -> Option<i64> {
    v.and_then(js_int)
}

/// `undefined` or `null`.
fn nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// JS `String(v)` of a JSON value (`undefined` for `None`).
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `toLocaleLowerCase('en-US')`.
pub(crate) fn lc(s: &str) -> String {
    lower(s)
}

/// Removes U+FE0F (the emoji presentation selector).
pub fn strip_fe0f(s: &str) -> String {
    s.chars().filter(|c| *c != '\u{FE0F}').collect()
}

/// JS `/\s+/g` → `' '`.
pub(crate) fn collapse_spaces(s: &str) -> String {
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

/// `oneLine(s)`: whitespace runs to one space, trimmed.
pub(crate) fn one_line(s: &str) -> String {
    trim(&collapse_spaces(s)).to_string()
}

/// `codeSafe(s)`: backticks become `ˋ` so the text can sit in a code span.
pub(crate) fn code_safe(s: &str) -> String {
    s.replace('`', "ˋ")
}

fn str_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// The string field of an object (`''` when absent or not a string).
pub(crate) fn sfield<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The string items of an array field.
pub(crate) fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// `normalizeAutomationName(input)`: the slug (letters, digits, `-`, `_`, up to 32), or `None`.
pub fn normalize_automation_name(input: &str) -> Option<String> {
    feat_embeds::embed_service::normalize_embed_name(input)
}

/// `normalizeAutomationName` of a JSON value (`None` for anything but a string).
pub fn normalize_automation_name_value(input: Option<&Value>) -> Option<String> {
    str_of(input).and_then(normalize_automation_name)
}

/// `normalizeTypeInput(str)`: `'Gained role'` / `'gained-role'` → `'GAINED_ROLE'`.
pub fn normalize_type_input(s: &str) -> String {
    let up = trim(s).to_uppercase();
    let mut out = String::with_capacity(up.len());
    let mut in_run = false;
    for c in up.chars() {
        if c == '-' || bot_util::js::is_js_space(c) {
            if !in_run {
                out.push('_');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// `isSnowflake(str)`: 17 to 20 ASCII digits.
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `AUTOMATION_ID_RE`: `^[a-z0-9]{8,40}$` (case-insensitive).
pub fn is_automation_id(s: &str) -> bool {
    (8..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Which mention form [`parse_id_list`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Channel,
    Role,
    User,
}

impl IdKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "channel" => Some(Self::Channel),
            "role" => Some(Self::Role),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

fn digit_run(b: &[u8], from: usize) -> usize {
    b[from..].iter().take_while(|c| c.is_ascii_digit()).count()
}

/// `parseIdList(text, kind)`: ids from free text, in order, unique. `<#id>` counts for channels, `<@&id>` for roles,
/// `<@id>` / `<@!id>` for users; a bare snowflake for every kind.
pub fn parse_id_list(text: Option<&str>, kind: IdKind) -> Vec<String> {
    let s = text.unwrap_or("");
    let b = s.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mention = |i: usize, prefix: &[u8]| -> Option<(usize, usize)> {
        if !b[i..].starts_with(prefix) {
            return None;
        }
        let start = i + prefix.len();
        let n = digit_run(b, start);
        if (17..=20).contains(&n) && b.get(start + n) == Some(&b'>') { Some((start, n)) } else { None }
    };
    while i < b.len() {
        let mut hit: Option<(Option<String>, usize)> = None;
        if let Some((st, n)) = mention(i, b"<#") {
            hit = Some(((kind == IdKind::Channel).then(|| s[st..st + n].to_string()), st + n + 1));
        } else if let Some((st, n)) = mention(i, b"<@&") {
            hit = Some(((kind == IdKind::Role).then(|| s[st..st + n].to_string()), st + n + 1));
        } else if let Some((st, n)) = mention(i, b"<@!").or_else(|| mention(i, b"<@")) {
            hit = Some(((kind == IdKind::User).then(|| s[st..st + n].to_string()), st + n + 1));
        } else if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let n = digit_run(b, i);
            if (17..=20).contains(&n) {
                hit = Some((Some(s[i..i + n].to_string()), i + n));
            }
        }
        match hit {
            Some((id, end)) => {
                if let Some(id) = id
                    && !out.contains(&id)
                {
                    out.push(id);
                }
                i = end;
            }
            None => {
                // Advance one character (UTF-8 aware).
                i += s[i..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    out
}

/// `parseWords(text)`: comma-separated words, trimmed, empties dropped, case-insensitively unique.
pub fn parse_words(text: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in text.unwrap_or("").split(',') {
        let w = trim(part);
        if w.is_empty() {
            continue;
        }
        if seen.insert(lc(w)) {
            out.push(w.to_string());
        }
    }
    out
}

// ── Emoji ────────────────────────────────────────────────────────────────────

/// `CUSTOM_EMOJI_RE`: `<(a?):(\w{2,32}):(\d{17,20})>` → `(animated, name, id)`.
pub fn parse_custom_emoji(s: &str) -> Option<(bool, &str, &str)> {
    let inner = s.strip_prefix('<')?.strip_suffix('>')?;
    let (animated, rest) = match inner.strip_prefix("a:") {
        Some(r) => (true, r),
        None => (false, inner.strip_prefix(':')?),
    };
    let (name, id) = rest.split_once(':')?;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    if !(2..=32).contains(&name.len()) || !name.bytes().all(word) || !is_snowflake(id) {
        return None;
    }
    Some((animated, name, id))
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// `PICTOGRAPH_RE.test(s)`: an Extended_Pictographic or Regional_Indicator code point, or a keycap.
fn has_pictograph(s: &str) -> bool {
    let cs: Vec<char> = s.chars().collect();
    for (i, c) in cs.iter().enumerate() {
        if bot_core::custom_embed::is_extended_pictographic(*c) || is_regional_indicator(*c) {
            return true;
        }
        if matches!(c, '#' | '*' | '0'..='9') {
            let mut j = i + 1;
            if cs.get(j) == Some(&'\u{FE0F}') {
                j += 1;
            }
            if cs.get(j) == Some(&'\u{20E3}') {
                return true;
            }
        }
    }
    false
}

/// `isStoredEmoji(str)`: `<:name:id>` / `<a:name:id>`, or one unicode emoji grapheme (at most 32 UTF-16 units).
pub fn is_stored_emoji(s: &str) -> bool {
    let n = utf16_len(s);
    if n == 0 || n > 32 {
        return false;
    }
    if parse_custom_emoji(s).is_some() {
        return true;
    }
    s.graphemes(true).take(2).count() == 1 && has_pictograph(s)
}

/// `emojiKeyOf(stored)`: the id of a custom emoji, otherwise the string without U+FE0F.
pub fn emoji_key_of(stored: &str) -> String {
    match parse_custom_emoji(stored) {
        Some((_, _, id)) => id.to_string(),
        None => strip_fe0f(stored),
    }
}

/// A custom emoji the bot can use: `{ id, name, animated }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomEmoji {
    pub id: String,
    pub name: String,
    pub animated: bool,
}

/// `parseEmojiInput(str, resolveCustom)`: user input to a stored emoji.
pub fn parse_emoji_input(s: &str, resolve_custom: impl Fn(&str) -> Option<CustomEmoji>) -> Result<String, String> {
    let s = trim(s);
    if s.is_empty() {
        return Err("That is not an emoji.".into());
    }
    if parse_custom_emoji(s).is_some() {
        return Ok(s.to_string());
    }
    let named = s
        .strip_prefix(':')
        .and_then(|r| r.strip_suffix(':'))
        .filter(|n| (2..=32).contains(&n.len()) && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
    if named.is_some() || is_snowflake(s) {
        let found = resolve_custom(named.unwrap_or(s));
        if let Some(e) = found.filter(|e| !e.id.is_empty() && !e.name.is_empty()) {
            return Ok(format!("<{}:{}:{}>", if e.animated { "a" } else { "" }, e.name, e.id));
        }
        return Err("I can only use custom emojis from servers I am in.".into());
    }
    if is_stored_emoji(s) {
        return Ok(s.to_string());
    }
    Err("That is not an emoji.".into())
}

/// `emojiResolver(guild)`: the guild's emojis by id, then by name.
pub fn emoji_resolver(guild: Option<&Guild>) -> impl Fn(&str) -> Option<CustomEmoji> + '_ {
    move |id_or_name: &str| {
        let g = guild?;
        let by_id = id_or_name.parse::<u64>().ok().filter(|v| *v != 0).and_then(|v| g.emojis.get(&serenity::all::EmojiId::new(v)));
        let e = by_id.or_else(|| g.emojis.values().find(|e| e.name == id_or_name))?;
        Some(CustomEmoji { id: e.id.to_string(), name: e.name.clone(), animated: e.animated })
    }
}

// ── Attachments ──────────────────────────────────────────────────────────────

const EXT_IMAGE: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"];
const EXT_VIDEO: &[&str] = &["mp4", "mov", "webm", "mkv", "avi", "m4v"];
const EXT_AUDIO: &[&str] = &["mp3", "wav", "ogg", "oga", "flac", "m4a", "opus", "aac"];
const EXT_TEXT: &[&str] = &["txt", "md", "log", "csv", "json"];

/// `attachmentKind({ name, contentType })`: `AUDIO`, `IMAGE`, `VIDEO` or `TEXT`; the content type wins over the name.
pub fn attachment_kind(name: Option<&str>, content_type: Option<&str>) -> Option<&'static str> {
    let ty = content_type.unwrap_or("").split(';').next().map(|t| trim(t).to_ascii_lowercase()).unwrap_or_default();
    if !ty.is_empty() {
        if ty.starts_with("image/") {
            return Some("IMAGE");
        }
        if ty.starts_with("video/") {
            return Some("VIDEO");
        }
        if ty.starts_with("audio/") {
            return Some("AUDIO");
        }
        if ty.starts_with("text/") || ty == "application/json" {
            return Some("TEXT");
        }
    }
    let name = name.unwrap_or("");
    let ext: String = match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() && name[dot + 1..].bytes().all(|b| b.is_ascii_alphanumeric()) => name[dot + 1..].to_ascii_lowercase(),
        _ => return None,
    };
    for (kind, list) in [("IMAGE", EXT_IMAGE), ("VIDEO", EXT_VIDEO), ("AUDIO", EXT_AUDIO), ("TEXT", EXT_TEXT)] {
        if list.contains(&ext.as_str()) {
            return Some(kind);
        }
    }
    None
}

// ── Normalisers ──────────────────────────────────────────────────────────────

type Norm = Result<Value, String>;

fn read_ids(raw: Option<&Value>, field: &str, min: usize, max: usize) -> Result<Vec<Value>, String> {
    let Some(arr) = raw.and_then(Value::as_array) else { return Err(format!("{field} must be a list.")) };
    if arr.len() > max {
        return Err(format!("{field} can hold at most {max} entries."));
    }
    let mut out: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    for v in arr {
        let id = match v {
            Value::String(s) => trim(s).to_string(),
            _ => return Err(format!("{field} has an invalid id.")),
        };
        if !is_snowflake(&id) {
            return Err(format!("{field} has an invalid id."));
        }
        if seen.insert(id.clone()) {
            out.push(Value::String(id));
        }
    }
    if out.len() < min {
        return Err(format!("{field} needs at least {min} entr{}.", if min == 1 { "y" } else { "ies" }));
    }
    Ok(out)
}

fn read_strings(raw: Option<&Value>, field: &str, max_each: usize, max: usize) -> Result<Vec<Value>, String> {
    let Some(arr) = raw.and_then(Value::as_array) else { return Err(format!("{field} must be a list.")) };
    if arr.len() > max {
        return Err(format!("{field} can hold at most {max} entries."));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for v in arr {
        let s = v.as_str().map(trim).unwrap_or("");
        let n = utf16_len(s);
        if n < 1 || n > max_each {
            return Err(format!("Each entry of {field} must be 1-{max_each} characters."));
        }
        if seen.insert(lc(s)) {
            out.push(Value::String(s.to_string()));
        }
    }
    if out.is_empty() {
        return Err(format!("{field} needs at least 1 entry."));
    }
    Ok(out)
}

fn read_optional_id(v: Option<&Value>, field: &str) -> Result<Value, String> {
    if nullish(v) {
        return Ok(Value::Null);
    }
    match v {
        Some(Value::String(s)) if is_snowflake(trim(s)) => Ok(Value::String(trim(s).to_string())),
        _ => Err(format!("{field} is not a valid id.")),
    }
}

fn read_content(v: Option<&Value>) -> Result<String, String> {
    let s = str_of(v).map(trim).unwrap_or("");
    let n = utf16_len(s);
    if !(1..=CONTENT_MAX).contains(&n) {
        return Err(format!("content must be 1-{CONTENT_MAX} characters."));
    }
    Ok(s.to_string())
}

fn read_emoji(v: Option<&Value>) -> Result<String, String> {
    match v {
        Some(Value::String(s)) if is_stored_emoji(trim(s)) => Ok(trim(s).to_string()),
        _ => Err("emoji is not a valid emoji.".into()),
    }
}

/// `normalizeButton(raw)`: `Ok(Value::Null)` for "no button".
pub fn normalize_button(raw: Option<&Value>) -> Norm {
    if nullish(raw) {
        return Ok(Value::Null);
    }
    let Some(obj) = raw.and_then(Value::as_object) else { return Err("The button must be an object.".into()) };
    let label = str_of(obj.get("label")).map(trim).unwrap_or("");
    let n = utf16_len(label);
    if !(1..=BUTTON_LABEL_MAX).contains(&n) {
        return Err(format!("The button label must be 1-{BUTTON_LABEL_MAX} characters."));
    }
    let raw_url = obj.get("url").map(|u| match u {
        Value::String(s) => Value::String(trim(s).to_string()),
        other => other.clone(),
    });
    match &raw_url {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.is_empty() => {}
        Some(Value::String(s)) => {
            if utf16_len(s) > BUTTON_URL_MAX || !bot_core::custom_embed::is_http_url(s) {
                return Err("The button link must start with http:// or https://.".into());
            }
            return Ok(json!({ "label": label, "url": s, "style": "PRIMARY", "index": null }));
        }
        Some(_) => return Err("The button link must start with http:// or https://.".into()),
    }
    let index = js_int_opt(obj.get("index")).filter(|i| (1..=MAX_BUTTON_INDEX).contains(i));
    let Some(index) = index else {
        return Err(format!("An automation button needs a number from 1 to {MAX_BUTTON_INDEX}."));
    };
    let style = match obj.get("style") {
        None | Some(Value::Null) => "PRIMARY",
        Some(Value::String(s)) if BUTTON_STYLES.contains(&s.as_str()) => s.as_str(),
        Some(_) => return Err(format!("The button style must be one of {}.", BUTTON_STYLES.join(", "))),
    };
    Ok(json!({ "label": label, "url": null, "style": style, "index": index }))
}

fn obj_with(type_: &str, field: &str, value: Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String(type_.into()));
    m.insert(field.into(), value);
    Value::Object(m)
}

fn normalize_flags(raw: Option<&Value>) -> Result<String, String> {
    if nullish(raw) {
        return Ok("i".into());
    }
    let Some(s) = str_of(raw).filter(|s| s.len() <= 4 && s.bytes().all(|b| matches!(b, b'i' | b'm' | b's' | b'u'))) else {
        return Err("The regex flags can only use i, m, s and u.".into());
    };
    let mut chars: Vec<char> = s.chars().collect::<HashSet<_>>().into_iter().collect();
    chars.sort_unstable();
    Ok(chars.into_iter().collect())
}

/// `normalizeCondition(raw, { keepHints })`: a fresh condition (unknown keys dropped) or the refusal.
pub fn normalize_condition(raw: &Value, keep_hints: bool) -> Norm {
    let Some(obj) = raw.as_object() else { return Err("A condition must be an object.".into()) };
    let Some(def) = str_of(obj.get("type")).and_then(condition_def) else { return Err("Unknown condition type.".into()) };
    let t = def.key;
    match t {
        "CHANNEL_IN" | "CHANNEL_NOT_IN" | "HAS_ANY_ROLE" | "HAS_ALL_ROLES" | "HAS_NOT_ANY_ROLE" | "HAS_NOT_ALL_ROLES" | "GAINED_ROLE"
        | "LOST_ROLE" | "USER_IS" | "CATEGORY_IN" | "CATEGORY_NOT_IN" => {
            let field = def.list.unwrap_or("roleIds");
            let ids = read_ids(obj.get(field), field, 1, 25)?;
            Ok(obj_with(t, field, Value::Array(ids)))
        }
        "CONTAINS" | "NOT_CONTAINS" => Ok(obj_with(t, "words", Value::Array(read_strings(obj.get("words"), "words", 100, 25)?))),
        "EXACT" => Ok(obj_with(t, "texts", Value::Array(read_strings(obj.get("texts"), "texts", 500, 25)?))),
        "MATCHES_REGEX" => {
            let pattern = str_of(obj.get("pattern")).unwrap_or("");
            let flags = normalize_flags(obj.get("flags"))?;
            if let Err(reason) = bot_util::safe_regex::validate_regex(pattern, &flags) {
                return Err(format!("Invalid regex: {reason}"));
            }
            Ok(json!({ "type": t, "pattern": pattern, "flags": flags }))
        }
        "ATTACHMENT_AUDIO" | "ATTACHMENT_IMAGE" | "ATTACHMENT_VIDEO" | "ATTACHMENT_TEXT" | "IS_REPLY" | "IS_NOT_REPLY" => Ok(json!({ "type": t })),
        "REACTED_WITH" => {
            let Some(arr) = obj.get("emojis").and_then(Value::as_array) else { return Err("emojis must be a list.".into()) };
            if arr.len() > 10 {
                return Err("emojis can hold at most 10 entries.".into());
            }
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            for v in arr {
                let e = read_emoji(Some(v))?;
                if seen.insert(emoji_key_of(&e)) {
                    out.push(Value::String(e));
                }
            }
            if out.is_empty() {
                return Err("emojis needs at least 1 entry.".into());
            }
            Ok(obj_with(t, "emojis", Value::Array(out)))
        }
        "HAS_REACTIONS" => {
            let Some(min) = js_int_opt(obj.get("min")).filter(|m| (1..=1000).contains(m)) else {
                return Err("min must be a whole number from 1 to 1000.".into());
            };
            let emoji = if nullish(obj.get("emoji")) { Value::Null } else { Value::String(read_emoji(obj.get("emoji"))?) };
            Ok(json!({ "type": t, "min": min, "emoji": emoji }))
        }
        "BUTTON_IS" => {
            let Some(arr) = obj.get("buttons").and_then(Value::as_array) else { return Err("buttons must be a list.".into()) };
            if arr.len() > 25 {
                return Err("buttons can hold at most 25 entries.".into());
            }
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            for b in arr {
                let Some(bo) = b.as_object() else { return Err("Each button reference must be an object.".into()) };
                let automation_id = str_of(bo.get("automationId")).map(trim).unwrap_or("");
                if !is_automation_id(automation_id) {
                    return Err("A button reference has an invalid automationId.".into());
                }
                let Some(index) = js_int_opt(bo.get("index")).filter(|i| (1..=MAX_BUTTON_INDEX).contains(i)) else {
                    return Err(format!("A button number must be from 1 to {MAX_BUTTON_INDEX}."));
                };
                if !seen.insert(format!("{automation_id}:{index}")) {
                    continue;
                }
                let mut entry = Map::new();
                entry.insert("automationId".into(), Value::String(automation_id.into()));
                entry.insert("index".into(), json!(index));
                if keep_hints
                    && let Some(name) = str_of(bo.get("automationName")).map(trim).filter(|n| !n.is_empty())
                {
                    entry.insert("automationName".into(), Value::String(clamp_text(name, 64)));
                }
                out.push(Value::Object(entry));
            }
            if out.is_empty() {
                return Err("buttons needs at least 1 entry.".into());
            }
            Ok(obj_with(t, "buttons", Value::Array(out)))
        }
        _ => Err("Unknown condition type.".into()),
    }
}

fn content_action(obj: &Map<String, Value>) -> Result<(String, Value), String> {
    let content = read_content(obj.get("content"))?;
    let btn = normalize_button(obj.get("button"))?;
    Ok((content, btn))
}

fn amount_action(t: &str, obj: &Map<String, Value>) -> Norm {
    match js_int_opt(obj.get("amount")).filter(|a| (1..=MAX_REWARD_AMOUNT).contains(a)) {
        Some(amount) => Ok(json!({ "type": t, "amount": amount })),
        None => Err(format!("amount must be a whole number from 1 to {}.", format_number(MAX_REWARD_AMOUNT as f64))),
    }
}

/// `normalizeAction(raw)`: a fresh action (unknown keys dropped) or the refusal.
pub fn normalize_action(raw: &Value) -> Norm {
    let Some(obj) = raw.as_object() else { return Err("An action must be an object.".into()) };
    let Some(def) = str_of(obj.get("type")).and_then(action_def) else { return Err("Unknown action type.".into()) };
    let t = def.key;
    match t {
        "CREATE_THREAD" => {
            let mut name = DEFAULT_THREAD_NAME.to_string();
            if !nullish(obj.get("name")) {
                name = str_of(obj.get("name")).map(trim).unwrap_or("").to_string();
                let n = utf16_len(&name);
                if !(1..=THREAD_NAME_MAX).contains(&n) {
                    return Err(format!("The thread name must be 1-{THREAD_NAME_MAX} characters."));
                }
            }
            let ch = read_optional_id(obj.get("channelId"), "channelId")?;
            let minutes = if nullish(obj.get("autoArchiveMinutes")) { Some(1440) } else { js_int_opt(obj.get("autoArchiveMinutes")) };
            let Some(minutes) = minutes.filter(|m| THREAD_ARCHIVE_MINUTES.contains(m)) else {
                let list: Vec<String> = THREAD_ARCHIVE_MINUTES.iter().map(i64::to_string).collect();
                return Err(format!("autoArchiveMinutes must be one of {}.", list.join(", ")));
            };
            Ok(json!({ "type": t, "name": name, "channelId": ch, "autoArchiveMinutes": minutes }))
        }
        "SEND_MESSAGE" => {
            let (content, button) = content_action(obj)?;
            let ch = read_optional_id(obj.get("channelId"), "channelId")?;
            Ok(json!({ "type": t, "content": content, "channelId": ch, "button": button }))
        }
        "REPLY" | "SEND_IN_THREAD" => {
            let (content, button) = content_action(obj)?;
            Ok(json!({ "type": t, "content": content, "button": button }))
        }
        "REPOST" => {
            let ch = read_optional_id(obj.get("channelId"), "channelId")?;
            if ch.is_null() {
                return Err("Pick the channel to repost to.".into());
            }
            Ok(json!({ "type": t, "channelId": ch }))
        }
        "PIN" | "DELETE" => Ok(json!({ "type": t })),
        "REACT" => Ok(json!({ "type": t, "emoji": read_emoji(obj.get("emoji"))? })),
        "REMOVE_REACTION" => {
            if nullish(obj.get("emoji")) {
                return Ok(json!({ "type": t, "emoji": null }));
            }
            Ok(json!({ "type": t, "emoji": read_emoji(obj.get("emoji"))? }))
        }
        "GIVE_ROLES" | "TAKE_ROLES" => Ok(json!({ "type": t, "roleIds": read_ids(obj.get("roleIds"), "roleIds", 1, 10)? })),
        "ADD_TO_THREAD" => {
            let empty = Value::Array(Vec::new());
            let raw_ids = if nullish(obj.get("userIds")) { Some(&empty) } else { obj.get("userIds") };
            Ok(json!({ "type": t, "userIds": read_ids(raw_ids, "userIds", 0, 10)? }))
        }
        "GIVE_COINS" | "TAKE_COINS" | "GIVE_XP" | "TAKE_XP" => amount_action(t, obj),
        "SEND_EMBED" => {
            let Some(embed_name) = normalize_automation_name_value(obj.get("embedName")) else {
                return Err("embedName must be a saved embed name.".into());
            };
            let ch = read_optional_id(obj.get("channelId"), "channelId")?;
            Ok(json!({ "type": t, "embedName": embed_name, "channelId": ch }))
        }
        _ => Err("Unknown action type.".into()),
    }
}

/// Which list [`normalize_list`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Condition,
    Action,
}

/// An element [`normalize_list`] dropped: its 1-based position and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct InvalidItem {
    pub index: usize,
    pub error: String,
}

/// The result of [`normalize_list`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NormalizedList {
    pub items: Vec<Value>,
    pub invalid: Vec<InvalidItem>,
}

/// `normalizeList(rawArray, kind, { keepHints })`: invalid elements are dropped and reported (1-based); at most 100
/// elements are read.
pub fn normalize_list(raw: &Value, kind: ListKind, keep_hints: bool) -> NormalizedList {
    let mut out = NormalizedList::default();
    let Some(arr) = raw.as_array() else { return out };
    for (i, el) in arr.iter().take(MAX_STORED_ELEMENTS).enumerate() {
        let r = match kind {
            ListKind::Condition => normalize_condition(el, keep_hints),
            ListKind::Action => normalize_action(el),
        };
        match r {
            Ok(v) => out.items.push(v),
            Err(error) => out.invalid.push(InvalidItem { index: i + 1, error }),
        }
    }
    out
}

/// `normalizeList(...).items`.
pub fn normalize_items(raw: &Value, kind: ListKind) -> Vec<Value> {
    normalize_list(raw, kind, false).items
}

// ── Shape validation ─────────────────────────────────────────────────────────

fn ctype(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

/// The button of an action when it is an automation button (`button.url == null` and an integer index).
pub fn automation_button_index(a: &Value) -> Option<i64> {
    let b = a.get("button").filter(|b| b.is_object())?;
    if !nullish(b.get("url")) {
        return None;
    }
    js_int_opt(b.get("index"))
}

/// `validateAutomationShape({ trigger, conditions, actions, cooldownSeconds })`: human-readable errors (empty =
/// valid). `cooldown` is the JSON value of `cooldownSeconds`.
pub fn validate_automation_shape(trigger: &str, conditions: &[Value], actions: &[Value], cooldown: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(tdef) = trigger_def(trigger) else { return vec!["Unknown trigger.".into()] };
    let t_label = tdef.label;
    if conditions.len() > limits::AUTOMATION_CONDITIONS {
        errors.push(error_texts::max_conditions());
    }
    if actions.len() > limits::AUTOMATION_ACTIONS {
        errors.push(error_texts::max_actions());
    }
    if !js_int(cooldown).is_some_and(|c| (0..=MAX_COOLDOWN_SECONDS).contains(&c)) {
        errors.push(format!("The cooldown must be a whole number of seconds from 0 to {}.", format_number(MAX_COOLDOWN_SECONDS as f64)));
    }
    let mut seen_flags: Vec<&str> = Vec::new();
    for (k, c) in conditions.iter().enumerate() {
        let i = k + 1;
        let Some(def) = ctype(c).and_then(condition_def) else {
            errors.push(format!("Condition {i}: unknown type."));
            continue;
        };
        if !def.allows(trigger) {
            errors.push(format!("Condition {i}: \"{}\" doesn't work with the trigger \"{t_label}\".", def.label));
        }
        if def.no_params {
            if seen_flags.contains(&def.key) {
                errors.push(format!("Condition {i} is already there."));
            }
            seen_flags.push(def.key);
        }
    }
    if seen_flags.contains(&"IS_REPLY") && seen_flags.contains(&"IS_NOT_REPLY") {
        errors.push("These two conditions can never both be true: \"Message is a reply\" and \"Message is not a reply\".".into());
    }
    let mut used_buttons = HashSet::new();
    let mut thread_before = false;
    for (k, a) in actions.iter().enumerate() {
        let i = k + 1;
        let Some(def) = ctype(a).and_then(action_def) else {
            errors.push(format!("Action {i}: unknown type."));
            continue;
        };
        if !def.allows(trigger) {
            errors.push(format!("Action {i}: \"{}\" doesn't work with the trigger \"{t_label}\".", def.label));
        }
        let has_channel = a.get("channelId").is_some_and(truthy);
        if (def.key == "SEND_MESSAGE" || def.key == "SEND_EMBED") && !has_channel && !T_CHANNEL.contains(&trigger) {
            errors.push(format!("Action {i}: this trigger doesn't happen in a channel, so pick a channel."));
        }
        if def.key == "CREATE_THREAD" && !has_channel && !T_LIVE.contains(&trigger) {
            errors.push(format!("Action {i}: there is no message to start a thread from, so pick a channel."));
        }
        if def.thread && !T_THREADY.contains(&trigger) && !thread_before {
            errors.push(format!("Action {i}: there is no thread here. Add \"Create a thread\" before it."));
        }
        if def.key == "CREATE_THREAD" {
            thread_before = true;
        }
        if let Some(index) = automation_button_index(a) {
            if used_buttons.contains(&index) {
                errors.push(format!("Action {i}: button number {index} is used twice."));
            }
            used_buttons.insert(index);
        }
    }
    errors
}

/// JS truthiness of a JSON value.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn highest_button_index(actions: &[Value]) -> i64 {
    actions.iter().filter_map(automation_button_index).filter(|i| *i > 0).max().unwrap_or(0)
}

/// `nextButtonIndex(actions)`: 1 + the highest automation-button number, or `None` past 99.
pub fn next_button_index(actions: &[Value]) -> Option<i64> {
    let next = highest_button_index(actions) + 1;
    (next <= MAX_BUTTON_INDEX).then_some(next)
}

/// `highestButtonIndex(actions)`.
pub fn highest_button(actions: &[Value]) -> i64 {
    highest_button_index(actions)
}

/// `nextButtonSeq(row)`: above both the row's `buttonSeq` and every existing number; `None` past 99.
pub fn next_button_seq(button_seq: Option<&Value>, actions: &[Value]) -> Option<i64> {
    let seq = js_int_opt(button_seq).unwrap_or(0);
    let next = seq.max(highest_button_index(actions)) + 1;
    (next <= MAX_BUTTON_INDEX).then_some(next)
}

fn condition_list_key(t: &str, entry: &Value) -> String {
    match t {
        "CONTAINS" | "NOT_CONTAINS" | "EXACT" => lc(entry.as_str().unwrap_or("")),
        "REACTED_WITH" => emoji_key_of(entry.as_str().unwrap_or("")),
        "BUTTON_IS" => format!("{}:{}", js_string(entry.get("automationId")), js_string(entry.get("index"))),
        _ => js_string(Some(entry)),
    }
}

/// The result of [`add_condition`] (`index` is 1-based).
#[derive(Debug, Clone, PartialEq)]
pub struct AddedCondition {
    pub conditions: Vec<Value>,
    pub merged: bool,
    pub index: usize,
    pub added_new: bool,
}

/// `addCondition(row, type, params)`: merges into the first condition of the same list type, otherwise appends.
pub fn add_condition(conditions: &[Value], type_: &str, params: &Value) -> Result<AddedCondition, AutomationError> {
    let Some(def) = condition_def(type_) else { return Err(AutomationError::new("Unknown condition type.")) };
    if let Some(list) = def.list
        && let Some(at) = conditions.iter().position(|c| ctype(c) == Some(def.key))
    {
        let existing: Vec<Value> = conditions[at].get(list).and_then(Value::as_array).cloned().unwrap_or_default();
        let incoming: Vec<Value> = params.get(list).and_then(Value::as_array).cloned().unwrap_or_default();
        let mut seen: HashSet<String> = existing.iter().map(|e| condition_list_key(def.key, e)).collect();
        let mut fresh = Vec::new();
        for e in incoming {
            if seen.insert(condition_list_key(def.key, &e)) {
                fresh.push(e);
            }
        }
        if fresh.is_empty() {
            return Ok(AddedCondition { conditions: conditions.to_vec(), merged: true, index: at + 1, added_new: false });
        }
        if existing.len() + fresh.len() > def.max.unwrap_or(usize::MAX) {
            return Err(AutomationError::new(format!("That list can hold at most {} entries.", def.max.unwrap_or(0))));
        }
        let mut merged = conditions[at].clone();
        if let Some(m) = merged.as_object_mut() {
            m.insert(list.into(), Value::Array(existing.into_iter().chain(fresh).collect()));
        }
        let next = normalize_condition(&merged, false).map_err(AutomationError::new)?;
        let mut out = conditions.to_vec();
        out[at] = next;
        return Ok(AddedCondition { conditions: out, merged: true, index: at + 1, added_new: true });
    }
    if conditions.len() >= limits::AUTOMATION_CONDITIONS {
        return Err(AutomationError::new(error_texts::max_conditions()));
    }
    let mut raw = params.as_object().cloned().unwrap_or_default();
    raw.insert("type".into(), Value::String(def.key.into()));
    let made = normalize_condition(&Value::Object(raw), false).map_err(AutomationError::new)?;
    let mut out = conditions.to_vec();
    out.push(made);
    Ok(AddedCondition { index: out.len(), conditions: out, merged: false, added_new: true })
}

/// The result of [`append_action`].
#[derive(Debug, Clone, PartialEq)]
pub struct AppendedAction {
    pub actions: Vec<Value>,
    /// 1-based.
    pub index: usize,
    pub button_index: Option<i64>,
    pub button_seq: Option<i64>,
}

/// `appendAction(row, action)`: a new automation button gets the next `buttonSeq` number (a removed button's number is
/// never reused); store the returned `button_seq` in the same locked write.
pub fn append_action(actions: &[Value], button_seq: Option<&Value>, action: &Value) -> Result<AppendedAction, AutomationError> {
    if actions.len() >= limits::AUTOMATION_ACTIONS {
        return Err(AutomationError::new(error_texts::max_actions()));
    }
    let mut raw = action.clone();
    let mut button_index = None;
    if let Some(button) = action.get("button").filter(|b| truthy(b)) {
        let url = button.get("url");
        let own = nullish(url) || url.and_then(Value::as_str) == Some("");
        if own {
            let Some(next) = next_button_seq(button_seq, actions) else {
                return Err(AutomationError::with_code(error_texts::BUTTONS_USED, ErrorCode::Buttons));
            };
            button_index = Some(next);
            let mut b = button.as_object().cloned().unwrap_or_default();
            b.insert("url".into(), Value::Null);
            b.insert("index".into(), json!(next));
            if let Some(m) = raw.as_object_mut() {
                m.insert("button".into(), Value::Object(b));
            }
        }
    }
    let made = normalize_action(&raw).map_err(AutomationError::new)?;
    let mut out = actions.to_vec();
    out.push(made);
    Ok(AppendedAction { index: out.len(), actions: out, button_index, button_seq: button_index })
}

/// `removeAt(list, index1)`: removes the 1-based element.
pub fn remove_at(list: &[Value], index1: i64) -> Result<(Vec<Value>, Value), AutomationError> {
    if index1 < 1 || index1 as usize > list.len() {
        return Err(AutomationError::new(format!("There are {} to choose from.", list.len())));
    }
    let k = (index1 - 1) as usize;
    let mut out = list.to_vec();
    let removed = out.remove(k);
    Ok((out, removed))
}

// ── Condition evaluator ──────────────────────────────────────────────────────

/// An attachment as the evaluator sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AttachmentFact {
    pub name: Option<String>,
    pub content_type: Option<String>,
}

/// The button a click came from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonFact {
    pub automation_id: String,
    pub index: i64,
}

/// The plain facts `evaluateCondition` reads (`buildFacts`). `None` is "unavailable": the condition fails.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Facts {
    pub user_id: Option<String>,
    pub channel_id: Option<String>,
    pub parent_id: Option<String>,
    pub category_id: Option<String>,
    pub content: Option<String>,
    pub attachments: Option<Vec<AttachmentFact>>,
    pub is_reply: Option<bool>,
    pub reaction_key: Option<String>,
    pub reaction_counts: Option<HashMap<String, f64>>,
    pub member_role_ids: Option<Vec<String>>,
    pub changed_role_ids: Option<Vec<String>>,
    pub button: Option<ButtonFact>,
}

fn attachments_have(f: &Facts, kind: &str) -> bool {
    f.attachments.as_ref().is_some_and(|a| a.iter().any(|x| attachment_kind(x.name.as_deref(), x.content_type.as_deref()) == Some(kind)))
}

/// `evaluateCondition(cond, facts)`: unavailable facts fail the condition (also the "not" types); an unknown type
/// always fails.
pub fn evaluate_condition(cond: &Value, f: &Facts) -> bool {
    let Some(def) = ctype(cond).and_then(condition_def) else { return false };
    let list = |key: &str| str_list(cond, key);
    let has = |v: &Option<String>, l: &[String]| v.as_ref().is_some_and(|x| l.contains(x));
    let held = f.member_role_ids.as_ref();
    match def.key {
        "CHANNEL_IN" => {
            let l = list("channelIds");
            has(&f.channel_id, &l) || has(&f.parent_id, &l)
        }
        "CHANNEL_NOT_IN" => {
            let l = list("channelIds");
            f.channel_id.is_some() && !has(&f.channel_id, &l) && !has(&f.parent_id, &l)
        }
        "CATEGORY_IN" => has(&f.category_id, &list("categoryIds")),
        "CATEGORY_NOT_IN" => f.channel_id.is_some() && (f.category_id.is_none() || !has(&f.category_id, &list("categoryIds"))),
        "CONTAINS" => f.content.as_ref().is_some_and(|c| list("words").iter().any(|w| bot_util::safe_regex::contains_word(c, w, false))),
        "NOT_CONTAINS" => f.content.as_ref().is_some_and(|c| !list("words").iter().any(|w| bot_util::safe_regex::contains_word(c, w, false))),
        "EXACT" => f.content.as_ref().is_some_and(|c| {
            let c = lc(trim(c));
            list("texts").iter().any(|t| lc(t) == c)
        }),
        "MATCHES_REGEX" => f.content.as_ref().is_some_and(|c| {
            match bot_util::safe_regex::compile_safe_regex(sfield(cond, "pattern"), sfield(cond, "flags")) {
                Some(re) => bot_util::safe_regex::safe_regex_test(&re, c, None),
                None => false,
            }
        }),
        "ATTACHMENT_AUDIO" => attachments_have(f, "AUDIO"),
        "ATTACHMENT_IMAGE" => attachments_have(f, "IMAGE"),
        "ATTACHMENT_VIDEO" => attachments_have(f, "VIDEO"),
        "ATTACHMENT_TEXT" => attachments_have(f, "TEXT"),
        "REACTED_WITH" => f.reaction_key.as_ref().is_some_and(|k| list("emojis").iter().any(|e| &emoji_key_of(e) == k)),
        "HAS_REACTIONS" => {
            let Some(counts) = f.reaction_counts.as_ref() else { return false };
            let Some(min) = js_int_opt(cond.get("min")) else { return false };
            let count = match cond.get("emoji").and_then(Value::as_str) {
                Some(e) if !e.is_empty() => counts.get(&emoji_key_of(e)).copied().unwrap_or(0.0),
                _ => counts.values().sum(),
            };
            count >= min as f64
        }
        "IS_REPLY" => f.is_reply == Some(true),
        "IS_NOT_REPLY" => f.is_reply == Some(false),
        "HAS_ANY_ROLE" => held.is_some_and(|h| list("roleIds").iter().any(|r| h.contains(r))),
        "HAS_ALL_ROLES" => held.is_some_and(|h| list("roleIds").iter().all(|r| h.contains(r))),
        "HAS_NOT_ANY_ROLE" => held.is_some_and(|h| !list("roleIds").iter().any(|r| h.contains(r))),
        "HAS_NOT_ALL_ROLES" => held.is_some_and(|h| list("roleIds").iter().any(|r| !h.contains(r))),
        "GAINED_ROLE" | "LOST_ROLE" => f.changed_role_ids.as_ref().is_some_and(|ch| list("roleIds").iter().any(|r| ch.contains(r))),
        "USER_IS" => has(&f.user_id, &list("userIds")),
        "BUTTON_IS" => f.button.as_ref().is_some_and(|b| {
            cond.get("buttons").and_then(Value::as_array).is_some_and(|arr| {
                arr.iter().any(|x| x.get("automationId").and_then(Value::as_str) == Some(b.automation_id.as_str()) && js_int_opt(x.get("index")) == Some(b.index))
            })
        }),
        _ => false,
    }
}

/// `evaluateConditions(conds, facts)`: every condition must pass; none always passes.
pub fn evaluate_conditions(conds: &[Value], facts: &Facts) -> bool {
    conds.iter().all(|c| evaluate_condition(c, facts))
}

/// `splitConditions(conds)`: `(cheap, needsMessage)`; evaluating both halves equals evaluating all.
pub fn split_conditions(conds: &[Value]) -> (Vec<Value>, Vec<Value>) {
    conds.iter().cloned().partition(|c| !ctype(c).is_some_and(|t| NEEDS_MESSAGE.contains(&t)))
}

/// `roleDiff(old, new, guildId)`: `(gained, lost)`; `@everyone` (the guild id) never counts.
pub fn role_diff<S: AsRef<str>>(old: &[S], new: &[S], guild_id: &str) -> (Vec<String>, Vec<String>) {
    let uniq = |xs: &[S]| {
        let mut out: Vec<String> = Vec::new();
        for x in xs {
            if !out.iter().any(|o| o == x.as_ref()) {
                out.push(x.as_ref().to_string());
            }
        }
        out
    };
    let (before, after) = (uniq(old), uniq(new));
    let gained = after.iter().filter(|id| !before.contains(id) && id.as_str() != guild_id).cloned().collect();
    let lost = before.iter().filter(|id| !after.contains(id) && id.as_str() != guild_id).cloned().collect();
    (gained, lost)
}

/// `voiceTriggers(change)`: a move fires `VOICE_LEFT` (old channel) then `VOICE_JOINED` (new channel).
pub fn voice_triggers(change: &str) -> &'static [&'static str] {
    match change {
        "join" => &["VOICE_JOINED"],
        "leave" => &["VOICE_LEFT"],
        "move" => &["VOICE_LEFT", "VOICE_JOINED"],
        _ => &[],
    }
}

// ── Rate limits and loop protection ──────────────────────────────────────────

/// `createRunBudget({ max, windowMs })`: a per-guild sliding window.
#[derive(Debug)]
pub struct RunBudget {
    max: usize,
    window_ms: i64,
    runs: IndexMap<String, VecDeque<i64>>,
}

impl Default for RunBudget {
    fn default() -> Self {
        Self::new(RUN_BUDGET_MAX, RUN_BUDGET_WINDOW_MS)
    }
}

impl RunBudget {
    pub fn new(max: usize, window_ms: i64) -> Self {
        Self { max, window_ms, runs: IndexMap::new() }
    }

    /// `take(guildId, now)`: false when `max` runs started within the last window.
    pub fn take(&mut self, guild_id: &str, now: i64) -> bool {
        let w = self.window_ms;
        let mut exists = false;
        if let Some(list) = self.runs.get_mut(guild_id) {
            while list.front().is_some_and(|t| now - t >= w) {
                list.pop_front();
            }
            if list.is_empty() {
                self.runs.shift_remove(guild_id);
            } else {
                exists = true;
            }
        }
        if exists {
            let list = self.runs.get_mut(guild_id).expect("present");
            if list.len() >= self.max {
                return false;
            }
            list.push_back(now);
            return true;
        }
        if self.runs.len() >= 5_000 {
            self.runs.retain(|_, l| !(l.is_empty() || l.back().is_some_and(|t| now - t >= w)));
            if self.runs.len() >= 5_000 {
                self.runs.shift_remove_index(0);
            }
        }
        self.runs.insert(guild_id.to_string(), VecDeque::from([now]));
        true
    }
}

/// `createCauseTracker({ ttlMs, cap })`: the chain depth of role changes the bot is about to cause, per (guild, user).
#[derive(Debug)]
pub struct CauseTracker {
    ttl_ms: i64,
    cap: usize,
    map: IndexMap<String, (u32, i64)>,
}

impl Default for CauseTracker {
    fn default() -> Self {
        Self::new(CAUSE_TTL_MS, 10_000)
    }
}

impl CauseTracker {
    pub fn new(ttl_ms: i64, cap: usize) -> Self {
        Self { ttl_ms, cap, map: IndexMap::new() }
    }

    /// `mark(guildId, userId, depth, now)`: keeps the highest live depth.
    pub fn mark(&mut self, guild_id: &str, user_id: &str, depth: u32, now: i64) {
        let key = format!("{guild_id}:{user_id}");
        let live = self.map.get(&key).filter(|(_, exp)| *exp > now).map_or(0, |(d, _)| *d);
        self.map.shift_remove(&key);
        self.map.insert(key, (live.max(depth), now + self.ttl_ms));
        while self.map.len() > self.cap {
            self.map.shift_remove_index(0);
        }
    }

    /// `peek(guildId, userId, now)`.
    pub fn peek(&self, guild_id: &str, user_id: &str, now: i64) -> u32 {
        self.map.get(&format!("{guild_id}:{user_id}")).filter(|(_, exp)| *exp > now).map_or(0, |(d, _)| *d)
    }
}

/// `createLocalExpiringSet({ cap })`: bounded keys that expire.
#[derive(Debug)]
pub struct LocalExpiringSet {
    cap: usize,
    map: IndexMap<String, i64>,
}

impl Default for LocalExpiringSet {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl LocalExpiringSet {
    pub fn new(cap: usize) -> Self {
        Self { cap, map: IndexMap::new() }
    }

    /// `add(key, ttlMs, now)`: true when the key was newly added (not live).
    pub fn add(&mut self, key: &str, ttl_ms: i64, now: i64) -> bool {
        if self.map.get(key).is_some_and(|exp| *exp > now) {
            return false;
        }
        self.map.shift_remove(key);
        if self.map.len() >= self.cap {
            self.map.retain(|_, exp| *exp > now);
            while self.map.len() >= self.cap {
                self.map.shift_remove_index(0);
            }
        }
        self.map.insert(key.to_string(), now + ttl_ms.max(1));
        true
    }

    pub fn has(&self, key: &str, now: i64) -> bool {
        self.map.get(key).is_some_and(|exp| *exp > now)
    }

    pub fn delete(&mut self, key: &str) -> bool {
        self.map.shift_remove(key).is_some()
    }
}

// ── Descriptions ─────────────────────────────────────────────────────────────

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `entityText(kind, id, guild)`: a mention; `⚠️ deleted (id)` when the guild is known to lack it.
pub(crate) fn entity_text(kind: IdKind, id: &str, guild: Option<&Guild>) -> String {
    let mention = match kind {
        IdKind::Role => format!("<@&{id}>"),
        IdKind::User => format!("<@{id}>"),
        IdKind::Channel => format!("<#{id}>"),
    };
    let Some(g) = guild else { return mention };
    let exists = match kind {
        IdKind::User => return mention,
        IdKind::Role => parse_id(id).is_some_and(|v| g.roles.contains_key(&RoleId::new(v))),
        IdKind::Channel => parse_id(id).is_some_and(|v| bot_core::permissions::find_channel(g, ChannelId::new(v)).is_some()),
    };
    if exists { mention } else { format!("⚠️ deleted ({id})") }
}

fn entity_list(kind: IdKind, ids: &[String], guild: Option<&Guild>, max: usize) -> String {
    let items: Vec<String> = ids.iter().map(|id| entity_text(kind, id, guild)).collect();
    join_with_limit(&items, ", ", max, default_more)
}

fn quoted_list(items: &[String]) -> String {
    let q: Vec<String> = items.iter().map(|w| format!("\"{}\"", clamp_text(&one_line(w), 40))).collect();
    join_with_limit(&q, ", ", 120, default_more)
}

/// `describeCondition(cond, { guild })`: one line, at most 200 characters.
pub fn describe_condition(cond: &Value, guild: Option<&Guild>) -> String {
    let Some(def) = ctype(cond).and_then(condition_def) else { return "Unknown condition".into() };
    let list = |k: &str| str_list(cond, k);
    let detail = match def.key {
        "CHANNEL_IN" | "CHANNEL_NOT_IN" => entity_list(IdKind::Channel, &list("channelIds"), guild, 120),
        "CATEGORY_IN" | "CATEGORY_NOT_IN" => entity_list(IdKind::Channel, &list("categoryIds"), guild, 120),
        "CONTAINS" | "NOT_CONTAINS" => quoted_list(&list("words")),
        "EXACT" => quoted_list(&list("texts")),
        "MATCHES_REGEX" => format!("`/{}/{}`", code_safe(&clamp_text(sfield(cond, "pattern"), 100)), sfield(cond, "flags")),
        "REACTED_WITH" => join_with_limit(&list("emojis"), " ", 120, default_more),
        "HAS_REACTIONS" => {
            let min = js_int_opt(cond.get("min"));
            let min_text = min.map_or_else(|| "NaN".to_string(), |m| format_number(m as f64));
            let emoji = cond.get("emoji").and_then(Value::as_str).unwrap_or("");
            let s = format!("Message has at least {min_text} {emoji} reaction{}", if min == Some(1) { "" } else { "s" });
            return clamp_text(&collapse_spaces(&s), 200);
        }
        "HAS_ANY_ROLE" | "HAS_ALL_ROLES" | "HAS_NOT_ANY_ROLE" | "HAS_NOT_ALL_ROLES" | "GAINED_ROLE" | "LOST_ROLE" => {
            entity_list(IdKind::Role, &list("roleIds"), guild, 120)
        }
        "USER_IS" => entity_list(IdKind::User, &list("userIds"), guild, 120),
        "BUTTON_IS" => {
            let items: Vec<String> = cond
                .get("buttons")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|b| format!("button {} of `{}`", js_string(b.get("index")), code_safe(&clamp_text(sfield(b, "automationId"), 12))))
                        .collect()
                })
                .unwrap_or_default();
            join_with_limit(&items, ", ", 120, default_more)
        }
        _ => String::new(),
    };
    clamp_text(&if detail.is_empty() { def.label.to_string() } else { format!("{}: {detail}", def.label) }, 200)
}

fn button_text(button: Option<&Value>) -> String {
    let Some(b) = button.filter(|b| b.is_object()) else { return String::new() };
    let label = clamp_text(&one_line(sfield(b, "label")), 30);
    if nullish(b.get("url")) {
        format!(" with button {} \"{label}\"", js_string(b.get("index")))
    } else {
        format!(" with link button \"{label}\"")
    }
}

/// `describeAction(action, { guild, settings })`: one line, at most 200 characters.
pub fn describe_action(action: &Value, guild: Option<&Guild>, settings: Option<&bot_db::GuildSettings>) -> String {
    let Some(def) = ctype(action).and_then(action_def) else { return "Unknown action".into() };
    let channel_id = action.get("channelId").and_then(Value::as_str).filter(|s| !s.is_empty());
    let in_ch = |id: Option<&str>| id.map(|id| format!(" in {}", entity_text(IdKind::Channel, id, guild))).unwrap_or_default();
    let content = || clamp_text(&one_line(sfield(action, "content")), 50);
    let text = match def.key {
        "CREATE_THREAD" => format!("Create a thread \"{}\"{}", clamp_text(&one_line(sfield(action, "name")), 40), in_ch(channel_id)),
        "SEND_MESSAGE" => format!("Send \"{}\"{}{}", content(), in_ch(channel_id), button_text(action.get("button"))),
        "REPLY" => format!("Reply \"{}\"{}", content(), button_text(action.get("button"))),
        "REPOST" => format!("Repost the message{}", channel_id.map(|id| format!(" to {}", entity_text(IdKind::Channel, id, guild))).unwrap_or_default()),
        "PIN" => "Pin the message".into(),
        "DELETE" => "Delete the message".into(),
        "REACT" => format!("React with {}", js_string(action.get("emoji"))),
        "REMOVE_REACTION" => match action.get("emoji").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            Some(e) => format!("Remove the {e} reaction"),
            None => "Remove the reaction that triggered this".into(),
        },
        "GIVE_ROLES" => format!("Give roles: {}", entity_list(IdKind::Role, &str_list(action, "roleIds"), guild, 100)),
        "TAKE_ROLES" => format!("Take roles: {}", entity_list(IdKind::Role, &str_list(action, "roleIds"), guild, 100)),
        "ADD_TO_THREAD" => {
            let users = str_list(action, "userIds");
            let who = if users.is_empty() { "the member".to_string() } else { entity_list(IdKind::User, &users, guild, 100) };
            format!("Add {who} to the thread")
        }
        "SEND_IN_THREAD" => format!("Send \"{}\" in the thread{}", content(), button_text(action.get("button"))),
        "GIVE_COINS" | "TAKE_COINS" => {
            let verb = if def.key == "GIVE_COINS" { "Give" } else { "Take" };
            let (emoji, name) = match settings {
                Some(s) => (s.currency_emoji.clone(), s.currency_name.clone()),
                None => (String::new(), "coins".to_string()),
            };
            format!("{verb} {} {emoji} {name}", amount_text(action))
        }
        "GIVE_XP" => format!("Give {} XP", amount_text(action)),
        "TAKE_XP" => format!("Take {} XP", amount_text(action)),
        "SEND_EMBED" => format!("Send the saved embed **{}**{}", js_string(action.get("embedName")), in_ch(channel_id)),
        _ => def.label.to_string(),
    };
    clamp_text(trim(&collapse_spaces(&text)), 200)
}

fn amount_text(action: &Value) -> String {
    match action.get("amount").and_then(Value::as_f64) {
        Some(n) => format_number(n),
        None => "NaN".into(),
    }
}

// ── Presets ──────────────────────────────────────────────────────────────────

/// `commandsOnlyPattern(prefix)`: messages that do not start with the prefix; `None` when prefix commands are off.
pub fn commands_only_pattern(prefix: Option<&str>) -> Option<String> {
    prefix.filter(|p| !p.is_empty()).map(|p| format!("^(?!{})", bot_util::safe_regex::escape_regex(p)))
}

/// The options of [`build_preset_automation`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PresetOptions {
    pub channel_id: Option<String>,
    pub message: Option<String>,
    pub role_id: Option<String>,
    pub emoji: Option<String>,
    pub threshold: Option<i64>,
    pub user_id: Option<String>,
    /// The custom-command prefix (`None` = prefix commands off).
    pub prefix: Option<String>,
    /// Staff roles that may still talk (Commands Only).
    pub exempt_role_ids: Vec<String>,
}

/// A trigger with normalised, shape-valid conditions and actions.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltAutomation {
    pub trigger: String,
    pub conditions: Vec<Value>,
    pub actions: Vec<Value>,
}

fn must_normalize(list: Vec<Value>, kind: ListKind) -> Result<Vec<Value>, AutomationError> {
    list.iter()
        .map(|raw| {
            match kind {
                ListKind::Condition => normalize_condition(raw, false),
                ListKind::Action => normalize_action(raw),
            }
            .map_err(AutomationError::new)
        })
        .collect()
}

fn opt_str(v: &Option<String>) -> Value {
    v.as_ref().map_or(Value::Null, |s| Value::String(s.clone()))
}

/// `buildPresetAutomation(preset, opts)`.
pub fn build_preset_automation(preset: &str, opts: &PresetOptions) -> Result<BuiltAutomation, AutomationError> {
    let (trigger, conditions, actions): (&str, Vec<Value>, Vec<Value>) = match preset {
        "welcomer" => {
            let mut actions = vec![json!({
                "type": "SEND_MESSAGE", "channelId": opt_str(&opts.channel_id),
                "content": opts.message.clone().unwrap_or_else(|| WELCOMER_DEFAULT_TEXT.to_string()), "button": null
            })];
            if let Some(role) = opts.role_id.as_ref().filter(|r| !r.is_empty()) {
                actions.push(json!({ "type": "GIVE_ROLES", "roleIds": [role] }));
            }
            ("MEMBER_JOINED", Vec::new(), actions)
        }
        "starboard" => {
            let emoji = opts.emoji.clone().unwrap_or_else(|| DEFAULT_STAR.to_string());
            (
                "REACTION_ADDED",
                vec![
                    json!({ "type": "REACTED_WITH", "emojis": [emoji] }),
                    json!({ "type": "HAS_REACTIONS", "min": opts.threshold.unwrap_or(DEFAULT_THRESHOLD), "emoji": emoji }),
                    json!({ "type": "CHANNEL_NOT_IN", "channelIds": [opt_str(&opts.channel_id)] }),
                ],
                vec![json!({ "type": "REPOST", "channelId": opt_str(&opts.channel_id) })],
            )
        }
        "thread-joiner" => (
            "THREAD_CREATED",
            match opts.channel_id.as_ref().filter(|c| !c.is_empty()) {
                Some(c) => vec![json!({ "type": "CHANNEL_IN", "channelIds": [c] })],
                None => Vec::new(),
            },
            vec![json!({ "type": "ADD_TO_THREAD", "userIds": [opt_str(&opts.user_id)] })],
        ),
        "commands-only" => {
            let mut conditions = vec![json!({ "type": "CHANNEL_IN", "channelIds": [opt_str(&opts.channel_id)] })];
            if let Some(pattern) = commands_only_pattern(opts.prefix.as_deref()) {
                conditions.push(json!({ "type": "MATCHES_REGEX", "pattern": pattern, "flags": "" }));
            }
            let mut exempt: Vec<String> = Vec::new();
            for r in &opts.exempt_role_ids {
                if !exempt.contains(r) {
                    exempt.push(r.clone());
                }
            }
            exempt.truncate(condition_def("HAS_NOT_ANY_ROLE").and_then(|d| d.max).unwrap_or(25));
            if !exempt.is_empty() {
                conditions.push(json!({ "type": "HAS_NOT_ANY_ROLE", "roleIds": exempt }));
            }
            ("MESSAGE_SENT", conditions, vec![json!({ "type": "DELETE" })])
        }
        _ => return Err(AutomationError::new("Unknown preset.")),
    };
    let conditions = must_normalize(conditions, ListKind::Condition)?;
    let actions = must_normalize(actions, ListKind::Action)?;
    let errors = validate_automation_shape(trigger, &conditions, &actions, &json!(0));
    if !errors.is_empty() {
        return Err(AutomationError::new(errors.join("\n")));
    }
    Ok(BuiltAutomation { trigger: trigger.into(), conditions, actions })
}

// ── JSON export / import ─────────────────────────────────────────────────────

/// `toExportObject(row, automationsById)`: `row` is the row as JSON (Prisma field names); `names` maps automation ids
/// to names, for the `automationName` hint of `BUTTON_IS` entries.
pub fn to_export_object(row: &Value, names: &HashMap<String, String>) -> Value {
    let null = Value::Null;
    let conditions: Vec<Value> = normalize_items(row.get("conditions").unwrap_or(&null), ListKind::Condition)
        .into_iter()
        .map(|c| {
            if ctype(&c) != Some("BUTTON_IS") {
                return c;
            }
            let mut c = c;
            if let Some(buttons) = c.get_mut("buttons").and_then(Value::as_array_mut) {
                for b in buttons.iter_mut() {
                    let id = sfield(b, "automationId").to_string();
                    if let Some(name) = names.get(&id).filter(|n| !n.is_empty()) {
                        *b = json!({ "automationId": id, "index": b.get("index").cloned().unwrap_or(Value::Null), "automationName": name });
                    }
                }
            }
            c
        })
        .collect();
    let mut out = Map::new();
    out.insert("format".into(), json!(EXPORT_FORMAT));
    out.insert("version".into(), json!(EXPORT_VERSION));
    out.insert("exportedId".into(), row.get("id").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null));
    out.insert("name".into(), row.get("name").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null));
    if let Some(t) = row.get("trigger") {
        out.insert("trigger".into(), t.clone());
    }
    out.insert("enabled".into(), json!(row.get("enabled") == Some(&Value::Bool(true))));
    out.insert("cooldownSeconds".into(), json!(js_int_opt(row.get("cooldownSeconds")).unwrap_or(0)));
    out.insert("conditions".into(), Value::Array(conditions));
    out.insert("actions".into(), Value::Array(normalize_items(row.get("actions").unwrap_or(&null), ListKind::Action)));
    Value::Object(out)
}

/// `toExportJson(row, automationsById)`: `JSON.stringify(obj, null, 2)`.
pub fn to_export_json(row: &Value, names: &HashMap<String, String>) -> String {
    serde_json::to_string_pretty(&to_export_object(row, names)).unwrap_or_default()
}

/// The parsed data of an import (`parseAutomationJson(...).data`).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportData {
    pub name: Option<String>,
    pub trigger: String,
    pub enabled: bool,
    pub cooldown_seconds: i64,
    pub conditions: Vec<Value>,
    pub actions: Vec<Value>,
    pub exported_id: Option<String>,
}

impl ImportData {
    /// The JS object, in the JS key order.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name, "trigger": self.trigger, "enabled": self.enabled, "cooldownSeconds": self.cooldown_seconds,
            "conditions": self.conditions, "actions": self.actions, "exportedId": self.exported_id
        })
    }
}

/// The result of [`parse_automation_json`]. `data` is `None` for the early refusals (as in the JS).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedImport {
    pub ok: bool,
    pub data: Option<ImportData>,
    pub errors: Vec<String>,
    /// Always empty (kept for the JS shape).
    pub warnings: Vec<String>,
}

impl ParsedImport {
    fn fail(msg: impl Into<String>) -> Self {
        Self { ok: false, data: None, errors: vec![msg.into()], warnings: Vec::new() }
    }

    /// The JS object.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("ok".into(), json!(self.ok));
        if let Some(d) = &self.data {
            m.insert("data".into(), d.to_json());
        }
        m.insert("errors".into(), json!(self.errors));
        m.insert("warnings".into(), json!(self.warnings));
        Value::Object(m)
    }
}

/// `parseAutomationJson(text)`: strict validation of pasted or attached JSON.
pub fn parse_automation_json(text: &str) -> ParsedImport {
    if text.len() > IMPORT_MAX_BYTES {
        return ParsedImport::fail("The JSON must be 32 KB or smaller.");
    }
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let Ok(parsed) = serde_json::from_str::<Value>(body) else { return ParsedImport::fail("That is not valid JSON.") };
    let Some(obj) = parsed.as_object() else { return ParsedImport::fail("The JSON must be an object from /automation export.") };
    if let Some(f) = obj.get("format")
        && f.as_str() != Some(EXPORT_FORMAT)
    {
        return ParsedImport::fail("This is not an automation export.");
    }
    if obj.get("version").and_then(Value::as_f64) != Some(EXPORT_VERSION as f64) {
        return ParsedImport::fail("Unsupported version (expected \"version\": 1).");
    }
    let Some(trigger) = str_of(obj.get("trigger")).and_then(trigger_key) else {
        return ParsedImport::fail(format!("Unknown trigger \"{}\".", clamp_text(&js_string(obj.get("trigger")), 40)));
    };
    let mut errors: Vec<String> = Vec::new();
    let mut name = None;
    if !nullish(obj.get("name")) {
        match obj.get("name") {
            Some(Value::String(s)) => name = normalize_automation_name(s),
            _ => errors.push("name must be text.".into()),
        }
    }
    let mut enabled = true;
    if let Some(e) = obj.get("enabled") {
        match e {
            Value::Bool(b) => enabled = *b,
            _ => errors.push("enabled must be true or false.".into()),
        }
    }
    let mut cooldown = 0;
    if let Some(c) = obj.get("cooldownSeconds") {
        match js_int(c).filter(|c| (0..=MAX_COOLDOWN_SECONDS).contains(c)) {
            Some(c) => cooldown = c,
            None => errors.push(format!("cooldownSeconds must be a whole number from 0 to {MAX_COOLDOWN_SECONDS}.")),
        }
    }
    let mut read_array = |value: Option<&Value>, label: &str, max: usize| -> Vec<Value> {
        let Some(v) = value else { return Vec::new() };
        let Some(arr) = v.as_array() else {
            errors.push(format!("{label}s must be a list."));
            return Vec::new();
        };
        if arr.len() > max {
            errors.push(format!("At most {max} {}s are allowed (found {}).", label.to_lowercase(), arr.len()));
            return Vec::new();
        }
        arr.clone()
    };
    let raw_conds = read_array(obj.get("conditions"), "Condition", limits::AUTOMATION_CONDITIONS);
    let raw_acts = read_array(obj.get("actions"), "Action", limits::AUTOMATION_ACTIONS);
    let mut conditions = Vec::new();
    let mut actions = Vec::new();
    for (k, raw) in raw_conds.iter().enumerate() {
        match normalize_condition(raw, true) {
            Ok(v) => conditions.push(v),
            Err(e) => errors.push(format!("Condition {}: {e}", k + 1)),
        }
    }
    for (k, raw) in raw_acts.iter().enumerate() {
        match normalize_action(raw) {
            Ok(v) => actions.push(v),
            Err(e) => errors.push(format!("Action {}: {e}", k + 1)),
        }
    }
    if errors.is_empty() {
        errors.extend(validate_automation_shape(trigger, &conditions, &actions, &json!(cooldown)));
    }
    let exported_id = str_of(obj.get("exportedId")).filter(|s| is_automation_id(s)).map(str::to_string);
    ParsedImport {
        ok: errors.is_empty(),
        data: Some(ImportData { name, trigger: trigger.into(), enabled, cooldown_seconds: cooldown, conditions, actions, exported_id }),
        errors,
        warnings: Vec::new(),
    }
}

// ── Discord errors ───────────────────────────────────────────────────────────

/// An error an action failed with (what `discordErrorText` reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// `TimeoutError`.
    Timeout,
    /// A Discord API error (or any other error): its JSON code and message.
    Api { code: Option<u64>, message: Option<String> },
}

impl ActionError {
    /// From a serenity error.
    pub fn from_serenity(err: &serenity::Error) -> Self {
        Self::Api { code: bot_core::discord::error_code(err), message: Some(bot_core::discord::error_message(err)) }
    }

    /// Any other error.
    pub fn other(message: impl Into<String>) -> Self {
        Self::Api { code: None, message: Some(message.into()) }
    }

    pub fn code(&self) -> Option<u64> {
        match self {
            Self::Api { code, .. } => *code,
            Self::Timeout => None,
        }
    }
}

impl From<serenity::Error> for ActionError {
    fn from(e: serenity::Error) -> Self {
        Self::from_serenity(&e)
    }
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("timed out"),
            Self::Api { message, .. } => f.write_str(message.as_deref().unwrap_or("Unknown error")),
        }
    }
}

/// `discordErrorText(err)`: the problem text, or `None` for an error to skip silently.
pub fn discord_error_text(err: &ActionError) -> Option<String> {
    let (code, message) = match err {
        ActionError::Timeout => return Some("The action took too long".into()),
        ActionError::Api { code, message } => (*code, message.as_deref()),
    };
    Some(match code {
        Some(10008 | 10007) => return None,
        Some(10003) => "The channel was deleted".into(),
        Some(10011) => "The role was deleted".into(),
        Some(10014) => "The emoji is not available".into(),
        Some(50001) => "I can't see that channel".into(),
        Some(50013) => "I'm missing a permission for this".into(),
        Some(30003) => "The channel has too many pinned messages".into(),
        Some(30010) => "The message has too many reactions".into(),
        Some(90001) => "Discord blocked the reaction".into(),
        Some(50083 | 160005) => "The thread is archived or locked".into(),
        Some(160006) => "The channel has too many active threads".into(),
        Some(30033) => "The thread is full".into(),
        Some(50035) => format!("Discord rejected the message: {}", clamp_text(message.unwrap_or("undefined"), 150)),
        _ => clamp_text(message.unwrap_or("Unknown error"), 200),
    })
}
