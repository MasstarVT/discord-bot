//! Port of `src/dashboard/collections/reactionRoles.js`: role panels (ReactionRoleGroup rows with their
//! ReactionRoleItem rows as the `items` list), like /reactionrole.

use std::collections::HashSet;

use bot_util::text::clamp_text;
use serde_json::{Map, Value, json};

use crate::collections::embeds::message_url;
use crate::discord::DiscordResources;
use crate::errors::RecordError;
use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, js_string, or_null, same_nullish, strict_eq, truthy};

pub const RR_TYPES: [&str; 4] = ["NORMAL", "UNIQUE", "VERIFY", "REVERSE"];
pub const RR_LAYOUTS: [&str; 3] = ["BUTTONS", "SELECT", "REACTIONS"];
/// Most roles one panel can hold, per layout.
pub const MAX_ITEMS: [(&str, usize); 3] = [("BUTTONS", 25), ("SELECT", 25), ("REACTIONS", 20)];
pub const TYPE_LABELS: [(&str, &str); 4] = [("NORMAL", "Normal"), ("UNIQUE", "Unique"), ("VERIFY", "Verify"), ("REVERSE", "Reverse")];
pub const LAYOUT_LABELS: [(&str, &str); 3] = [("BUTTONS", "Buttons"), ("SELECT", "Dropdown"), ("REACTIONS", "Emoji reactions")];
pub const DEFAULT_TITLE: &str = "Reaction Roles";
pub const MAX_MAX_ROLES: i64 = 25;
/// What `messageId` holds while a panel has not been posted yet (the column cannot be null).
pub const UNPOSTED_MESSAGE_ID: &str = "0";

fn lookup<'a>(table: &[(&str, &'a str)], key: Option<&str>) -> Option<&'a str> {
    let k = key?;
    table.iter().find(|(x, _)| *x == k).map(|(_, v)| *v)
}

/// `CUSTOM_EMOJI_RE` (`/^<(a)?:(\w{1,32}):(\d{17,20})>$/`): (animated, name, id).
pub fn parse_custom_emoji(s: &str) -> Option<(bool, &str, &str)> {
    let inner = s.strip_prefix('<')?.strip_suffix('>')?;
    let (animated, rest) = match inner.strip_prefix("a:") {
        Some(r) => (true, r),
        None => (false, inner.strip_prefix(':')?),
    };
    let (name, id) = rest.split_once(':')?;
    let name_ok = (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let id_ok = (17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
    (name_ok && id_ok).then_some((animated, name, id))
}

/// `emojiKey(stored)`: the reaction key of a stored emoji: the custom emoji id, or the Unicode string without U+FE0F.
pub fn emoji_key(stored: Option<&Value>) -> Option<String> {
    let s = stored?.as_str().filter(|s| !s.is_empty())?;
    if let Some((_, _, id)) = parse_custom_emoji(s) {
        return Some(id.to_string());
    }
    let k: String = s.chars().filter(|c| *c != '\u{FE0F}').collect();
    (!k.is_empty()).then_some(k)
}

const ATTACHED_TEXT: &str =
    "This panel uses an existing message that the bot never edits, so only the title, role mode, role limit and roles can change.";
/// Fields of an attached panel whose message the bot never edits.
const LOCKED_WHEN_ATTACHED: [&str; 3] = ["description", "messageContent", "plainText"];

/// `itemsProblem(items, layout)`: the per-layout cap, one entry per role, and (emoji-reaction panels) an emoji on every
/// entry, each used once.
pub fn items_problem(items: &[Value], layout: Option<&str>) -> Option<Problem> {
    let max = lookup_n(layout).unwrap_or(25);
    if items.len() > max {
        let label = lookup(&LAYOUT_LABELS, layout).unwrap_or("Buttons").to_lowercase();
        return Some(Problem::new("items", format!("A {label} panel can hold at most {max} roles.")));
    }
    let mut roles: Vec<Value> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    for (i, item) in items.iter().enumerate() {
        let role = item.get("roleId").cloned().unwrap_or(Value::String("\u{0}undefined".into()));
        if crate::util::list_has(&roles, &role) {
            return Some(Problem::new(format!("items.{i}.roleId"), "This role is already on the panel. Each role can be added once."));
        }
        roles.push(role);
        if layout != Some("REACTIONS") {
            continue;
        }
        let Some(key) = emoji_key(item.get("emoji")) else {
            return Some(Problem::new(format!("items.{i}.emoji"), "Emoji-reaction panels need an emoji for each role."));
        };
        if !keys.insert(key) {
            return Some(Problem::new(format!("items.{i}.emoji"), "This emoji is already used for another role on this panel."));
        }
    }
    None
}

fn lookup_n(layout: Option<&str>) -> Option<usize> {
    let l = layout?;
    MAX_ITEMS.iter().find(|(k, _)| *k == l).map(|(_, n)| *n)
}

/// The `check` of reactionRoles.
pub fn check_reaction_roles(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let layout = if ctx.mode == Mode::Create {
        data.get("layout").and_then(Value::as_str).map(str::to_string)
    } else {
        match ctx.current.and_then(|c| c.get("layout")).filter(|v| !v.is_null()) {
            Some(v) => v.as_str().map(str::to_string),
            None => Some("BUTTONS".into()),
        }
    };
    if ctx.mode != Mode::Create
        && let Some(current) = ctx.current
        && current.get("attached") == Some(&Value::Bool(true))
    {
        for field in LOCKED_WHEN_ATTACHED {
            if data.contains_key(field) && !same_nullish(data.get(field), current.get(field)) {
                return Some(Problem::new(field, ATTACHED_TEXT));
            }
        }
    }
    if data.contains_key("items") {
        return items_problem(as_list(data.get("items")), layout.as_deref());
    }
    None
}

/// What [`diff_panel_items`] answers: the rows to remove, the entries to create (with their position), the rows to
/// update (`{ id, roleId, data }`), the emojis no entry uses any more, and whether anything changed.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelItemsDiff {
    pub remove: Vec<Value>,
    pub create: Vec<Value>,
    pub update: Vec<Value>,
    pub removed_emojis: Vec<Value>,
    pub changed: bool,
}

impl PanelItemsDiff {
    /// The JS object `{ remove, create, update, removedEmojis, changed }`.
    pub fn to_json(&self) -> Value {
        json!({
            "remove": self.remove, "create": self.create, "update": self.update,
            "removedEmojis": self.removed_emojis, "changed": self.changed,
        })
    }
}

fn key_of(v: Option<&Value>) -> String {
    serde_json::to_string(v.unwrap_or(&Value::String("\u{0}undefined".into()))).unwrap_or_default()
}

/// `diffPanelItems(oldRows, next)`: what changing a panel's role list to `next` does to the stored rows. Entries are
/// matched by role; the order of `next` is the new position.
pub fn diff_panel_items(old_rows: &[Value], next: &[Value]) -> PanelItemsDiff {
    let mut by_role: indexmap::IndexMap<String, &Value> = indexmap::IndexMap::new();
    for r in old_rows {
        by_role.insert(key_of(r.get("roleId")), r);
    }
    let kept: HashSet<String> = next.iter().map(|n| key_of(n.get("roleId"))).collect();
    let remove: Vec<Value> = old_rows.iter().filter(|r| !kept.contains(&key_of(r.get("roleId")))).cloned().collect();
    let mut create = Vec::new();
    let mut update = Vec::new();
    for (position, n) in next.iter().enumerate() {
        let Some(old) = by_role.get(&key_of(n.get("roleId"))) else {
            let mut c = n.as_object().cloned().unwrap_or_default();
            c.insert("position".into(), json!(position));
            create.push(Value::Object(c));
            continue;
        };
        let mut data = Map::new();
        for k in ["label", "emoji", "style", "description"] {
            if !same_nullish(old.get(k), n.get(k)) {
                data.insert(k.into(), or_null(n.get(k)));
            }
        }
        if !old.get("position").is_some_and(|p| strict_eq(p, &json!(position))) {
            data.insert("position".into(), json!(position));
        }
        if !data.is_empty() {
            let mut u = Map::new();
            if let Some(id) = old.get("id") {
                u.insert("id".into(), id.clone());
            }
            if let Some(r) = n.get("roleId") {
                u.insert("roleId".into(), r.clone());
            }
            u.insert("data".into(), Value::Object(data));
            update.push(Value::Object(u));
        }
    }
    let still_used: HashSet<String> = next.iter().filter_map(|n| emoji_key(n.get("emoji"))).collect();
    let mut gone: Vec<Value> = remove.iter().map(|r| or_null(r.get("emoji"))).collect();
    for u in &update {
        if u["data"].get("emoji").is_some()
            && let Some(old) = by_role.get(&key_of(u.get("roleId")))
        {
            gone.push(or_null(old.get("emoji")));
        }
    }
    let mut removed_emojis: Vec<Value> = Vec::new();
    for emoji in gone {
        if let Some(key) = emoji_key(Some(&emoji))
            && !still_used.contains(&key)
            && !removed_emojis.iter().any(|e| emoji_key(Some(e)).as_deref() == Some(key.as_str()))
        {
            removed_emojis.push(emoji);
        }
    }
    let changed = !remove.is_empty() || !create.is_empty() || !update.is_empty();
    PanelItemsDiff { remove, create, update, removed_emojis, changed }
}

/// `normalizeItems(items, layout, resources, heldEmojiKeys)`: the stored form of the submitted role list, as
/// /reactionrole add makes it, then the per-layout rules again on the canonical emojis.
pub fn normalize_items(
    items: &[Value],
    layout: Option<&str>,
    resources: Option<&DiscordResources>,
    held_emoji_keys: &HashSet<String>,
) -> Result<Vec<Value>, RecordError> {
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut emoji = or_null(item.get("emoji"));
        if let Some((_, _, id)) = emoji.as_str().filter(|s| !s.is_empty()).and_then(parse_custom_emoji) {
            if let Some(known) = resources.and_then(|r| r.emoji(id)) {
                emoji = json!(format!("<{}:{}:{}>", if known.animated { "a" } else { "" }, known.name, known.id));
            } else if !held_emoji_keys.contains(id) {
                return Err(RecordError::invalid("That custom emoji is not from this server. Pick one of this server's emojis or a standard emoji.")
                    .with_path(format!("items.{i}.emoji")));
            }
        }
        let typed = match item.get("label") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(Some(v)),
        };
        let typed = bot_util::js::trim(&typed);
        let role_id = item.get("roleId").and_then(Value::as_str).unwrap_or_default();
        let label_src = if !typed.is_empty() {
            typed.to_string()
        } else {
            resources.and_then(|r| r.role(role_id)).map(|r| r.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Role".into())
        };
        let label = clamp_text(&label_src, 80);
        let description = if layout == Some("SELECT") && truthy(item.get("description")) {
            let d = clamp_text(bot_util::js::trim(&js_string(item.get("description"))), 100);
            if d.is_empty() { Value::Null } else { json!(d) }
        } else {
            Value::Null
        };
        let mut o = Map::new();
        o.insert("roleId".into(), or_null(item.get("roleId")));
        o.insert("label".into(), json!(label));
        o.insert("emoji".into(), emoji);
        if layout == Some("BUTTONS") {
            if let Some(s) = item.get("style") {
                o.insert("style".into(), s.clone());
            }
        } else {
            o.insert("style".into(), json!(1));
        }
        o.insert("description".into(), description);
        out.push(Value::Object(o));
    }
    if let Some(p) = items_problem(&out, layout) {
        return Err(RecordError::invalid(p.message).with_path(p.path));
    }
    Ok(out)
}

/// `panelItem(row)`: a ReactionRoleGroup row (with its `items`) as a collection item.
pub fn panel_item(row: &Value) -> Value {
    let mut rows: Vec<&Value> = as_list(row.get("items")).iter().collect();
    let pos = |v: &Value| v.get("position").filter(|p| !p.is_null()).map_or(0.0, |p| dash_schema::js::to_number(Some(p)));
    rows.sort_by(|a, b| pos(a).partial_cmp(&pos(b)).unwrap_or(std::cmp::Ordering::Equal));
    let items: Vec<Value> = rows
        .iter()
        .map(|i| {
            let style = i.get("style").and_then(Value::as_f64).filter(|s| [1.0, 2.0, 3.0, 4.0].contains(s)).map_or(json!(1), |_| i["style"].clone());
            json!({
                "roleId": raw(i.get("roleId")),
                "label": raw(i.get("label")),
                "emoji": or_null(i.get("emoji")),
                "style": style,
                "description": or_null(i.get("description")),
            })
        })
        .collect();
    let unposted = row.get("messageId").and_then(Value::as_str) == Some(UNPOSTED_MESSAGE_ID);
    let status = if unposted {
        "Not posted yet"
    } else if truthy(row.get("messageMissingAt")) {
        "Message deleted"
    } else if truthy(row.get("attached")) {
        "Attached message"
    } else {
        "Posted"
    };
    let n = items.len();
    let layout = row.get("layout").and_then(Value::as_str);
    let ty = row.get("type").and_then(Value::as_str);
    let summary = format!(
        "{} · {} · {n} {} · {status}",
        lookup(&LAYOUT_LABELS, layout).map_or_else(|| js_string(row.get("layout")), str::to_string),
        lookup(&TYPE_LABELS, ty).map_or_else(|| js_string(row.get("type")), str::to_string),
        if n == 1 { "role" } else { "roles" },
    );
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "channelId": raw(row.get("channelId")),
        "layout": raw(row.get("layout")),
        "type": raw(row.get("type")),
        "title": raw(row.get("title")),
        "description": or_null(row.get("description")),
        "messageContent": or_null(row.get("messageContent")),
        "plainText": row.get("plainText") == Some(&Value::Bool(true)),
        "maxRoles": or_null(row.get("maxRoles")),
        "placeholder": or_null(row.get("placeholder")),
        "items": items,
        "summary": summary,
        "status": status,
        "attached": row.get("attached") == Some(&Value::Bool(true)),
        "messageUrl": if unposted { None } else { message_url(row.get("guildId"), row.get("channelId"), row.get("messageId")) },
    }))
}

/// `TEMPLATE_OPTIONS`: the create-only panel templates (`{ value, label, description? }`): none, or a role set
/// (`ROLE_TEMPLATES`) the bot creates. Read from the `template` field's options (generated from the JS).
pub fn template_options() -> &'static [Value] {
    static OPTIONS: std::sync::LazyLock<Vec<Value>> = std::sync::LazyLock::new(|| {
        crate::Collections::builtin()
            .get("reactionRoles")
            .and_then(|c| c.fields.iter().find(|f| f.field() == "template"))
            .and_then(|f| f.get("options"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    });
    &OPTIONS
}
