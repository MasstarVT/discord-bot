//! Port of `src/dashboard/collections/tags.js`: the `tags` collection (Custom Commands) and the pure conversion between
//! the stored action list (tagService.normalizeActions) and the form's shape.

use bot_config::constants::limits;
use serde_json::{Map, Value, json};

use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, int_of, iso, len16, list_has, merged, or_null, truthy};

/// `TAG_LIMITS`: limits of the custom-command writes.
pub mod tag_limits {
    pub const PLAIN: usize = 2000;
    pub const EMBED: usize = 4000;
    pub const DESCRIPTION: usize = 100;
    pub const COOLDOWN_SECONDS: i64 = 31_536_000;
    pub const NAME: usize = 32;
}

const MESSAGE_TYPES: [&str; 3] = ["reply", "dm", "send"];
const ROLE_TYPES: [&str; 3] = ["addRole", "removeRole", "toggleRole"];

/// Argument variables a custom command's replies support (besides the built-in ones).
pub const ARGUMENT_PLACEHOLDERS: [&str; 6] = ["{1}", "{1:default}", "{2...}", "{...3}", "{2...4}", "{...}"];

fn type_of(a: &Value) -> Option<&str> {
    a.get("type").and_then(Value::as_str)
}

fn is_message_type(a: &Value) -> bool {
    type_of(a).is_some_and(|t| MESSAGE_TYPES.contains(&t))
}

fn is_role_type(a: &Value) -> bool {
    type_of(a).is_some_and(|t| ROLE_TYPES.contains(&t))
}

/// `x >= 1` of a JS comparison (undefined is NaN, null 0).
fn at_least_one(v: Option<&Value>) -> bool {
    let n = match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(v) => dash_schema::js::to_number(Some(v)),
    };
    n >= 1.0
}

/// `checkTag(data, { mode, current })`: cross-field rules of a custom command (the /tag action add and cooldown
/// checks). On update `current` fills in what the request leaves out.
pub fn check_tag(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let t = if ctx.mode == Mode::Update && ctx.current.is_some() { merged(ctx.current, data) } else { data.clone() };
    for (i, a) in as_list(t.get("actions")).iter().enumerate() {
        let at = format!("actions.{i}");
        if is_message_type(a) {
            let replies = as_list(a.get("responses"));
            if replies.is_empty() {
                return Some(Problem::new(format!("{at}.responses"), "Add at least one reply."));
            }
            let embed = truthy(a.get("embed"));
            let max = if embed { tag_limits::EMBED } else { tag_limits::PLAIN };
            for (k, r) in replies.iter().enumerate() {
                let text = match r.get("text") {
                    None | Some(Value::Null) => String::new(),
                    Some(v) => dash_schema::js::to_js_string(Some(v)),
                };
                let n = len16(&text);
                if n > max {
                    let message = if embed {
                        format!("Embed replies can be at most 4,000 characters (this one is {n}).")
                    } else {
                        format!("Plain replies can be at most 2,000 characters (this one is {n}). Turn on \"Send as an embed\" for up to 4,000.")
                    };
                    return Some(Problem::new(format!("{at}.responses.{k}.text"), message));
                }
            }
            if type_of(a) == Some("send") && !truthy(a.get("channelId")) {
                return Some(Problem::new(format!("{at}.channelId"), "Pick the channel to send to."));
            }
            if type_of(a) == Some("dm") && !as_list(a.get("pingRoleIds")).is_empty() {
                return Some(Problem::new(format!("{at}.pingRoleIds"), "DMs never ping roles."));
            }
        } else if is_role_type(a) && !truthy(a.get("roleId")) {
            return Some(Problem::new(format!("{at}.roleId"), "Pick the role."));
        }
    }
    for (allow, deny, what) in [("allowRoleIds", "denyRoleIds", "role"), ("allowChannelIds", "denyChannelIds", "channel")] {
        let allowed = as_list(t.get(allow));
        if let Some(both) = as_list(t.get(deny)).iter().position(|id| list_has(allowed, id)) {
            return Some(Problem::new(format!("{deny}.{both}"), format!("A {what} cannot be both allowed and denied.")));
        }
    }
    let scope = t.get("cooldownScope");
    if truthy(scope) && scope != Some(&Value::String("NONE".into())) && !at_least_one(t.get("cooldownSeconds")) {
        return Some(Problem::new("cooldownSeconds", "Set a cooldown of at least 1 second, or pick \"No cooldown\"."));
    }
    None
}

/// `actionToForm(a)`: one stored action (normalizeActions shape) in the form's shape.
pub fn action_to_form(a: &Value) -> Value {
    let is_role = is_role_type(a);
    let ty = type_of(a);
    let is_dm = ty == Some("reply") && truthy(a.get("dm"));
    let mut out = Map::new();
    if is_dm {
        out.insert("type".into(), json!("dm"));
    } else if let Some(t) = a.get("type") {
        out.insert("type".into(), t.clone());
    }
    let responses: Vec<Value> = if is_role {
        Vec::new()
    } else {
        as_list(a.get("responses")).iter().map(|t| json!({ "text": dash_schema::js::to_js_string(Some(t)) })).collect()
    };
    out.insert("responses".into(), Value::Array(responses));
    out.insert("embed".into(), Value::Bool(!is_role && a.get("embed") == Some(&Value::Bool(true))));
    out.insert("channelId".into(), if ty == Some("send") { or_null(a.get("channelId")) } else { Value::Null });
    out.insert("roleId".into(), if is_role { or_null(a.get("roleId")) } else { Value::Null });
    out.insert("pingRoleIds".into(), if is_role || is_dm { json!([]) } else { Value::Array(as_list(a.get("pingRoleIds")).to_vec()) });
    Value::Object(out)
}

fn dedupe(list: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for v in list {
        if !list_has(&out, v) || !matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null) {
            out.push(v.clone());
        }
    }
    out
}

/// `actionToStored(f)`: one validated form action in the stored shape normalizeActions keeps.
pub fn action_to_stored(f: &Value) -> Value {
    let mut out = Map::new();
    let ty = f.get("type");
    if is_role_type(f) {
        if let Some(t) = ty {
            out.insert("type".into(), t.clone());
        }
        if let Some(r) = f.get("roleId") {
            out.insert("roleId".into(), r.clone());
        }
        return Value::Object(out);
    }
    let responses: Vec<Value> = as_list(f.get("responses")).iter().map(|r| or_null(r.get("text"))).collect();
    let embed = f.get("embed") == Some(&Value::Bool(true));
    let mut pings = dedupe(as_list(f.get("pingRoleIds")));
    pings.truncate(limits::TAG_PING_ROLES);
    match type_of(f) {
        Some("dm") => {
            return json!({ "type": "reply", "dm": true, "embed": embed, "responses": responses });
        }
        Some("send") => {
            out.insert("type".into(), json!("send"));
            if let Some(c) = f.get("channelId") {
                out.insert("channelId".into(), c.clone());
            }
            out.insert("embed".into(), Value::Bool(embed));
            out.insert("responses".into(), Value::Array(responses));
        }
        _ => {
            out.insert("type".into(), json!("reply"));
            out.insert("dm".into(), Value::Bool(false));
            out.insert("embed".into(), Value::Bool(embed));
            out.insert("responses".into(), Value::Array(responses));
        }
    }
    if !pings.is_empty() {
        out.insert("pingRoleIds".into(), Value::Array(pings));
    }
    Value::Object(out)
}

/// `tagItem(row, effectiveActions)`: a CustomTag row as a collection item (`effective_actions`:
/// tagService.getEffectiveActions(row)).
pub fn tag_item(row: &Value, effective_actions: &[Value]) -> Value {
    let scope = row.get("cooldownScope").and_then(Value::as_str).filter(|s| ["NONE", "SERVER", "USER"].contains(s)).unwrap_or("NONE");
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "name": raw(row.get("name")),
        "description": or_null(row.get("description")),
        "enabled": row.get("enabled") != Some(&Value::Bool(false)),
        "actions": effective_actions.iter().map(action_to_form).collect::<Vec<_>>(),
        "allowRoleIds": as_list(row.get("allowRoleIds")),
        "denyRoleIds": as_list(row.get("denyRoleIds")),
        "allowChannelIds": as_list(row.get("allowChannelIds")),
        "denyChannelIds": as_list(row.get("denyChannelIds")),
        "cooldownScope": scope,
        "cooldownSeconds": int_of(row.get("cooldownSeconds")).map_or(0, |n| n.max(0)),
        "deleteInvocation": row.get("deleteInvocation") == Some(&Value::Bool(true)),
        "uses": int_of(row.get("uses")).unwrap_or(0),
        "createdAt": iso(row.get("createdAt")),
        "updatedAt": iso(row.get("updatedAt")),
    }))
}
