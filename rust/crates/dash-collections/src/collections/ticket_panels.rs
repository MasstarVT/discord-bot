//! Port of `src/dashboard/collections/ticketPanels.js`: ticket panels (TicketPanel rows) with their options
//! (TicketPanelOption rows as the `options` list), like /ticket-panel.

use std::collections::HashSet;
use std::sync::LazyLock;

use bot_core::custom_embed::{empty_template, normalize_template};
use bot_util::text::{clamp_text, clamp_text_with};
use regex::Regex;
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::collections::embeds::message_url;
use crate::collections::reaction_roles::parse_custom_emoji;
use crate::discord::DiscordResources;
use crate::errors::RecordError;
use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, js_string, len16, or_null, same_nullish, truthy};

pub const NAME_FORMAT_TOKENS: [&str; 6] = ["number", "user.name", "user.username", "user.id", "user.display_name", "topic"];
/// `DEFAULT_OPTION`.
pub fn default_option() -> Value {
    json!({ "label": "Open Ticket", "emoji": "✉️", "buttonStyle": 1 })
}
pub const MAX_OPEN_PER_USER: i64 = 10;
/// The `status` of a panel item.
pub const STATUS_PUBLISHED: &str = "Published";
pub const STATUS_DRAFT: &str = "Not published";

/// `defaultPanelTemplate(style)`: the panel message of a panel without its own.
pub fn default_panel_template(style: Option<&str>) -> Value {
    let description = if style == Some("SELECT") {
        "Choose a topic below to open a private ticket with our team."
    } else {
        "Press a button below to open a private ticket with our team."
    };
    json!({ "content": null, "embed": { "title": "How can we help?", "description": description }, "buttons": [] })
}

/// `DEFAULT_INTRO_TEMPLATE`: the intro message of a new ticket when neither the option nor the panel has one.
pub fn default_intro_template() -> Value {
    json!({
        "content": "{user}",
        "embed": {
            "title": "Ticket #{number}",
            "description": "Thanks for reaching out, {user.display_name}! A member of our team will be with you shortly.\nPlease describe your issue in as much detail as you can.",
        },
        "buttons": [],
    })
}

static NOT_NAME_CHAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}_-]").expect("valid regex"));
static DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").expect("valid regex"));

/// `sanitizeChannelName(str)`: a Discord-style channel name (ticketService.sanitizeChannelName).
pub fn sanitize_channel_name(s: &str) -> String {
    let lower: String = s.nfkc().collect::<String>().to_lowercase();
    let mut spaced = String::with_capacity(lower.len());
    let mut in_space = false;
    for c in lower.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                spaced.push('-');
            }
            in_space = true;
        } else {
            spaced.push(c);
            in_space = false;
        }
    }
    let kept = NOT_NAME_CHAR.replace_all(&spaced, "");
    let single = DASHES.replace_all(&kept, "-");
    let trimmed = single.trim_matches(|c| c == '-' || c == '_');
    clamp_text_with(trimmed, 100, "")
}

/// `{...}` tokens of a format (`/\{([^{}]*)\}/g`): (whole match range, inner text).
fn tokens(f: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let b = f.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'{' && b[j] != b'}' {
                j += 1;
            }
            if j < b.len() && b[j] == b'}' {
                out.push((i, j + 1, &f[i + 1..j]));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `validateNameFormat(fmt)`: 1-90 characters, only the known {variables}, at least one usable character.
pub fn validate_name_format(fmt: Option<&Value>) -> Result<String, String> {
    let raw = match fmt {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(Some(v)),
    };
    let f = bot_util::js::trim(&raw);
    if f.is_empty() || len16(f) > 90 {
        return Err("The name format must be 1–90 characters.".into());
    }
    for (_, _, inner) in tokens(f) {
        let t = bot_util::js::trim(inner).to_lowercase();
        if !NAME_FORMAT_TOKENS.contains(&t.as_str()) {
            return Err(format!("Unknown placeholder `{{{inner}}}`. Use {{number}}, {{user.name}}, {{user.id}}, {{user.display_name}} or {{topic}}."));
        }
    }
    let mut replaced = String::with_capacity(f.len());
    let mut last = 0;
    for (start, end, _) in tokens(f) {
        replaced.push_str(&f[last..start]);
        replaced.push('x');
        last = end;
    }
    replaced.push_str(&f[last..]);
    if sanitize_channel_name(&replaced).is_empty() {
        return Err("The name format has no usable characters.".into());
    }
    Ok(f.to_string())
}

/// The `check` of ticketPanels.
pub fn check_ticket_panel(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    if data.contains_key("channelNameFormat")
        && let Err(e) = validate_name_format(data.get("channelNameFormat"))
    {
        return Some(Problem::new("channelNameFormat", e));
    }
    if data.contains_key("options") {
        let mut labels: HashSet<String> = HashSet::new();
        let mut ids: Vec<Value> = Vec::new();
        for (i, option) in as_list(data.get("options")).iter().enumerate() {
            let label = js_string(option.get("label"));
            if !labels.insert(label.to_lowercase()) {
                return Some(Problem::new(format!("options.{i}.label"), format!("This panel already has an option called \"{label}\".")));
            }
            if let Some(id) = option.get("id").filter(|v| truthy(Some(v))) {
                if crate::util::list_has(&ids, id) {
                    return Some(Problem::new(format!("options.{i}.label"), "That option is on the list twice."));
                }
                ids.push(id.clone());
            }
        }
    }
    if ctx.mode == Mode::Update
        && data.get("channelId") == Some(&Value::Null)
        && ctx.current.is_some_and(|c| truthy(c.get("channelId")))
    {
        return Some(Problem::new(
            "channelId",
            "A published panel cannot be unpublished here. Pick another channel to move it, or delete the panel to remove its message.",
        ));
    }
    None
}

fn has_visible_embed(e: &Value) -> bool {
    if !truthy(Some(e)) {
        return false;
    }
    let t = |v: Option<&Value>| truthy(v);
    t(e.get("title"))
        || t(e.get("description"))
        || e.get("fields").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
        || e.get("fields").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
        || t(e.get("author").and_then(|a| a.get("name")))
        || t(e.get("footer").and_then(|f| f.get("text")))
        || t(e.get("image"))
        || t(e.get("thumbnail"))
}

/// `isEmptyTemplate(tpl)`: an empty form resets a message to its default; a colour-only embed shows nothing.
pub fn is_empty_template(tpl: &Value) -> bool {
    let t = normalize_template(tpl);
    let content = t.get("content").and_then(Value::as_str).is_some_and(|c| !bot_util::js::trim(c).is_empty());
    !content && !has_visible_embed(t.get("embed").unwrap_or(&Value::Null))
}

/// `toJson(tpl)`: a JSON-safe copy of a normalised template for a Json column.
pub fn to_json(tpl: &Value) -> Value {
    normalize_template(tpl)
}

/// `canonicalEmoji(emoji, resources, heldIds, path)`: a Unicode emoji as is, a custom emoji as the server writes it
/// (it must be one of this server's unless the panel already holds it).
pub fn canonical_emoji(
    emoji: Option<&Value>,
    resources: Option<&DiscordResources>,
    held_ids: &HashSet<String>,
    path: &str,
) -> Result<Value, RecordError> {
    let e = or_null(emoji);
    let Some((_, _, id)) = e.as_str().filter(|s| !s.is_empty()).and_then(parse_custom_emoji) else { return Ok(e) };
    if let Some(known) = resources.and_then(|r| r.emoji(id)) {
        return Ok(json!(format!("<{}:{}:{}>", if known.animated { "a" } else { "" }, known.name, known.id)));
    }
    if held_ids.contains(id) {
        return Ok(e);
    }
    Err(RecordError::invalid("That custom emoji is not from this server. Pick one of this server's emojis or a standard emoji.").with_path(path))
}

/// `normalizeOptions(options, resources, heldEmojiIds)`: the stored form of the submitted options.
pub fn normalize_options(options: &[Value], resources: Option<&DiscordResources>, held_emoji_ids: &HashSet<String>) -> Result<Vec<Value>, RecordError> {
    let mut out = Vec::new();
    for (i, o) in options.iter().enumerate() {
        let emoji = canonical_emoji(o.get("emoji"), resources, held_emoji_ids, &format!("options.{i}.emoji"))?;
        let description = match o.get("description").and_then(Value::as_str).map(bot_util::js::trim) {
            Some(d) if !d.is_empty() => json!(clamp_text(d, 100)),
            _ => Value::Null,
        };
        let intro = o.get("introTemplate").filter(|v| !v.is_null()).cloned().unwrap_or_else(empty_template);
        let intro_template = if is_empty_template(&intro) { Value::Null } else { to_json(o.get("introTemplate").unwrap_or(&Value::Null)) };
        let mut m = Map::new();
        m.insert("id".into(), or_null(o.get("id")));
        if let Some(l) = o.get("label") {
            m.insert("label".into(), l.clone());
        }
        m.insert("emoji".into(), emoji);
        m.insert("description".into(), description);
        if let Some(s) = o.get("buttonStyle") {
            m.insert("buttonStyle".into(), s.clone());
        }
        m.insert("categoryId".into(), or_null(o.get("categoryId")));
        m.insert("introTemplate".into(), intro_template);
        out.push(Value::Object(m));
    }
    Ok(out)
}

fn same_json(a: Option<&Value>, b: Option<&Value>) -> bool {
    let norm = |v: Option<&Value>| match v {
        Some(v) if truthy(Some(v)) => serde_json::to_string(&normalize_template(v)).unwrap_or_default(),
        _ => "null".to_string(),
    };
    norm(a) == norm(b)
}

/// What [`diff_options`] answers.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsDiff {
    pub update: Vec<Value>,
    pub create: Vec<Value>,
    pub remove: Vec<Value>,
    pub changed: bool,
}

impl OptionsDiff {
    /// The JS object `{ update, create, remove, changed }`.
    pub fn to_json(&self) -> Value {
        json!({ "update": self.update, "create": self.create, "remove": self.remove, "changed": self.changed })
    }
}

/// `diffOptions(oldRows, next)`: an entry keeps its row when it carries the row's id, else when its label matches
/// (case-insensitively) a row no entry claimed; every other entry is new and every unclaimed row goes.
pub fn diff_options(old_rows: &[Value], next: &[Value]) -> OptionsDiff {
    let id_of = |v: &Value| serde_json::to_string(v.get("id").unwrap_or(&Value::Null)).unwrap_or_default();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut matched: Vec<Option<&Value>> = vec![None; next.len()];
    for (i, n) in next.iter().enumerate() {
        if !truthy(n.get("id")) {
            continue;
        }
        let row = old_rows.iter().rev().find(|r| id_of(r) == id_of(n));
        if let Some(row) = row
            && !claimed.contains(&id_of(row))
        {
            matched[i] = Some(row);
            claimed.insert(id_of(row));
        }
    }
    for (i, n) in next.iter().enumerate() {
        if matched[i].is_some() {
            continue;
        }
        let key = js_string(n.get("label")).to_lowercase();
        if let Some(row) = old_rows.iter().find(|r| !claimed.contains(&id_of(r)) && js_string(r.get("label")).to_lowercase() == key) {
            matched[i] = Some(row);
            claimed.insert(id_of(row));
        }
    }
    let mut update = Vec::new();
    let mut create = Vec::new();
    for (position, n) in next.iter().enumerate() {
        let mut fields = n.as_object().cloned().unwrap_or_default();
        fields.shift_remove("id");
        let Some(row) = matched[position] else {
            fields.insert("position".into(), json!(position));
            create.push(Value::Object(fields));
            continue;
        };
        let mut data = Map::new();
        for k in ["label", "emoji", "description", "buttonStyle", "categoryId"] {
            if !same_nullish(row.get(k), fields.get(k)) {
                data.insert(k.into(), or_null(fields.get(k)));
            }
        }
        if !same_json(row.get("introTemplate"), fields.get("introTemplate"))
            && let Some(t) = fields.get("introTemplate")
        {
            data.insert("introTemplate".into(), t.clone());
        }
        if !row.get("position").is_some_and(|p| crate::util::strict_eq(p, &json!(position))) {
            data.insert("position".into(), json!(position));
        }
        if !data.is_empty() {
            update.push(json!({ "id": or_null(row.get("id")), "data": data }));
        }
    }
    let remove: Vec<Value> = old_rows.iter().filter(|r| !claimed.contains(&id_of(r))).cloned().collect();
    let changed = !update.is_empty() || !create.is_empty() || !remove.is_empty();
    OptionsDiff { update, create, remove, changed }
}

/// `isDefaultTemplate(template, defaults)`: whether a stored template is a built-in default.
pub fn is_default_template(template: &Value, defaults: &[Value]) -> bool {
    let a = serde_json::to_string(&normalize_template(template)).unwrap_or_default();
    defaults.iter().any(|d| serde_json::to_string(&normalize_template(d)).unwrap_or_default() == a)
}

/// `optionItem(row)`: a TicketPanelOption row as a list entry.
pub fn option_item(row: &Value) -> Value {
    let style = row.get("buttonStyle").and_then(Value::as_f64).filter(|s| [1.0, 2.0, 3.0, 4.0].contains(s)).map_or(json!(1), |_| row["buttonStyle"].clone());
    let intro = match row.get("introTemplate") {
        Some(t) if truthy(Some(t)) => normalize_template(t),
        _ => empty_template(),
    };
    strip_undefined(json!({
        "label": raw(row.get("label")),
        "emoji": or_null(row.get("emoji")),
        "description": or_null(row.get("description")),
        "buttonStyle": style,
        "categoryId": or_null(row.get("categoryId")),
        "introTemplate": intro,
        "id": raw(row.get("id")),
    }))
}

/// `panelItem(row, openTickets)`: a TicketPanel row (with its `options`) as a collection item.
pub fn panel_item(row: &Value, open_tickets: i64) -> Value {
    let published = truthy(row.get("channelId")) && truthy(row.get("messageId"));
    let mut opts: Vec<&Value> = as_list(row.get("options")).iter().collect();
    let pos = |v: &Value| v.get("position").filter(|p| !p.is_null()).map_or(0.0, |p| dash_schema::js::to_number(Some(p)));
    opts.sort_by(|a, b| {
        pos(a).partial_cmp(&pos(b)).unwrap_or(std::cmp::Ordering::Equal).then_with(|| {
            let (x, y) = (js_string(a.get("id")), js_string(b.get("id")));
            let (x16, y16): (Vec<u16>, Vec<u16>) = (x.encode_utf16().collect(), y.encode_utf16().collect());
            x16.cmp(&y16)
        })
    });
    let options: Vec<Value> = opts.into_iter().map(option_item).collect();
    let n = options.len();
    let style = row.get("style").and_then(Value::as_str);
    let template = match row.get("template") {
        Some(t) if truthy(Some(t)) => normalize_template(t),
        _ => default_panel_template(style),
    };
    let intro = match row.get("introTemplate").filter(|v| !v.is_null()) {
        Some(t) => normalize_template(t),
        None => normalize_template(&default_intro_template()),
    };
    let status = if published { STATUS_PUBLISHED } else { STATUS_DRAFT };
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "name": raw(row.get("name")),
        "style": raw(row.get("style")),
        "channelId": or_null(row.get("channelId")),
        "template": template,
        "options": options,
        "managerRoleIds": as_list(row.get("managerRoleIds")),
        "introTemplate": intro,
        "openCategoryId": or_null(row.get("openCategoryId")),
        "claimedCategoryId": or_null(row.get("claimedCategoryId")),
        "closedCategoryId": or_null(row.get("closedCategoryId")),
        "transcriptChannelId": or_null(row.get("transcriptChannelId")),
        "dmTranscript": row.get("dmTranscript") == Some(&Value::Bool(true)),
        "maxOpenPerUser": row.get("maxOpenPerUser").filter(|v| !v.is_null()).cloned().unwrap_or(json!(1)),
        "channelNameFormat": row.get("channelNameFormat").filter(|v| !v.is_null()).cloned().unwrap_or(json!("ticket-{number}")),
        "askReason": row.get("askReason") == Some(&Value::Bool(true)),
        "summary": format!("{} · {n} {} · {status}", if style == Some("SELECT") { "Dropdown" } else { "Buttons" }, if n == 1 { "option" } else { "options" }),
        "status": status,
        "openTickets": open_tickets,
        "messageUrl": if published { message_url(row.get("guildId"), row.get("channelId"), row.get("messageId")) } else { None },
    }))
}
