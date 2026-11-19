//! Port of `src/dashboard/collections/triggers.js`: the `triggers` collection (Custom Commands → auto-responders).

use serde_json::{Map, Value, json};

use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, int_of, iso, merged, truthy};

/// `TRIGGER_LIMITS` (tagService.TRIGGER_PATTERN_MAX and TRIGGER_COOLDOWN_MAX, and the response option's length).
pub mod trigger_limits {
    pub const PATTERN: usize = 500;
    pub const RESPONSE: usize = 2000;
    pub const COOLDOWN: i64 = 86_400;
}

/// `MATCH_TYPE_OPTIONS` (value, label).
pub const MATCH_TYPE_OPTIONS: [(&str, &str); 5] = [
    ("CONTAINS", "Contains"),
    ("EXACT", "Is exactly"),
    ("STARTS_WITH", "Starts with"),
    ("ENDS_WITH", "Ends with"),
    ("REGEX", "Regular expression"),
];

/// `checkTrigger(data, { mode, current })`: a REGEX pattern must pass safeRegex.
pub fn check_trigger(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let t = if ctx.mode == Mode::Update && ctx.current.is_some() { merged(ctx.current, data) } else { data.clone() };
    if !["trigger", "matchType", "caseSensitive"].iter().any(|k| data.contains_key(*k)) {
        return None;
    }
    let Some(trigger) = t.get("trigger").and_then(Value::as_str).filter(|s| !bot_util::js::trim(s).is_empty()) else {
        return Some(Problem::new("trigger", "The pattern is empty."));
    };
    if t.get("matchType").and_then(Value::as_str) == Some("REGEX") {
        let flags = if truthy(t.get("caseSensitive")) { "" } else { "i" };
        if let Err(reason) = bot_util::safe_regex::validate_regex(trigger, flags) {
            return Some(Problem::new("trigger", reason));
        }
    }
    None
}

/// `triggerItem(row)`: an AutoTrigger row as a collection item.
pub fn trigger_item(row: &Value) -> Value {
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "trigger": raw(row.get("trigger")),
        "matchType": raw(row.get("matchType")),
        "caseSensitive": row.get("caseSensitive") == Some(&Value::Bool(true)),
        "response": raw(row.get("response")),
        "cooldown": int_of(row.get("cooldown")).unwrap_or(0),
        "deleteMessage": row.get("deleteMessage") == Some(&Value::Bool(true)),
        "channelIds": as_list(row.get("channelIds")),
        "exemptRoleIds": as_list(row.get("exemptRoleIds")),
        "enabled": row.get("enabled") != Some(&Value::Bool(false)),
        "uses": int_of(row.get("uses")).unwrap_or(0),
        "createdAt": iso(row.get("createdAt")),
        "updatedAt": iso(row.get("updatedAt")),
    }))
}
