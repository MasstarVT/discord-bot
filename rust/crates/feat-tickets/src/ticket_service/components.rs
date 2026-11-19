//! Panel components, the intro and closed rows, `disableButtons` and the reason form (§6.4.2, §6.7.1). The rows are
//! built as discord.js JSON first (what the JS `toJSON()` sends), then turned into serenity builders, so tests can
//! compare them with JS output.

use bot_config::custom_id;
use bot_db::{TicketPanelOption, TicketPanelStyle};
use bot_util::text::clamp_text;
use serde_json::{Value, json};
use serenity::all::{Button, ReactionType};
use serenity::builder::{CreateActionRow, CreateButton, CreateInputText, CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption};
use serenity::all::InputTextStyle;

fn cid(r: Result<String, bot_config::CustomIdError>) -> String {
    r.unwrap_or_default()
}

/// `ButtonBuilder.setEmoji(str)` / a select option emoji as JSON: `{ id, name, animated }` for a custom emoji, `{ name }`
/// for a Unicode one.
pub fn emoji_json(s: &str) -> Value {
    if let Some(inner) = s.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() == 3 {
            return json!({ "id": parts[2], "name": parts[1], "animated": parts[0] == "a" });
        }
    }
    json!({ "name": s, "animated": false })
}

fn by_position(a: &TicketPanelOption, b: &TicketPanelOption) -> std::cmp::Ordering {
    a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id))
}

/// `buildPanelComponents(panel, options, { canUseEmoji })` as JSON rows. A custom emoji the bot can no longer see is
/// skipped, so Discord never rejects the whole message.
pub fn panel_components_json(panel_id: &str, style: TicketPanelStyle, options: &[TicketPanelOption], can_use_emoji: &dyn Fn(&str) -> bool) -> Vec<Value> {
    let mut list: Vec<&TicketPanelOption> = options.iter().collect();
    list.sort_by(|a, b| by_position(a, b));
    list.truncate(bot_config::limits::TICKET_PANEL_OPTIONS);
    if list.is_empty() {
        return Vec::new();
    }
    let emoji_of = |o: &TicketPanelOption| -> Option<String> {
        let e = o.emoji.as_deref().filter(|e| !e.is_empty())?;
        if !e.starts_with('<') || can_use_emoji(e) { Some(e.to_string()) } else { None }
    };
    if style == TicketPanelStyle::Select {
        let opts: Vec<Value> = list
            .iter()
            .map(|o| {
                let mut v = json!({ "label": clamp_text(&o.label, 100), "value": o.id });
                if let Some(d) = o.description.as_deref().filter(|d| !d.is_empty()) {
                    v["description"] = json!(clamp_text(d, 100));
                }
                if let Some(e) = emoji_of(o) {
                    v["emoji"] = emoji_json(&e);
                }
                v
            })
            .collect();
        let menu = json!({
            "type": 3, "custom_id": cid(custom_id!("ticket-select", panel_id)), "placeholder": "Select a topic…",
            "min_values": 1, "max_values": 1, "options": opts,
        });
        return vec![json!({ "type": 1, "components": [menu] })];
    }
    list.chunks(5)
        .take(5)
        .map(|chunk| {
            let buttons: Vec<Value> = chunk
                .iter()
                .map(|o| {
                    let style = if (1..=4).contains(&o.button_style) { o.button_style } else { 1 };
                    let mut b = json!({ "type": 2, "custom_id": cid(custom_id!("ticket", "open", panel_id, o.id)), "label": clamp_text(&o.label, 80), "style": style });
                    if let Some(e) = emoji_of(o) {
                        b["emoji"] = emoji_json(&e);
                    }
                    b
                })
                .collect();
            json!({ "type": 1, "components": buttons })
        })
        .collect()
}

/// `buildIntroRow(ticketId, { claimed })` as JSON: Close and Claim; with `claimed` the Claim button is disabled.
pub fn intro_row_json(ticket_id: &str, claimed: bool) -> Value {
    json!({ "type": 1, "components": [
        { "type": 2, "custom_id": cid(custom_id!("ticket", "close", ticket_id)), "label": "Close", "emoji": { "name": "🔒" }, "style": 4 },
        { "type": 2, "custom_id": cid(custom_id!("ticket", "claim", ticket_id)), "label": if claimed { "Claimed" } else { "Claim" }, "emoji": { "name": "🙋" }, "style": 3, "disabled": claimed },
    ]})
}

/// `buildClosedRow(ticketId)` as JSON: Reopen, Transcript and Delete.
pub fn closed_row_json(ticket_id: &str) -> Value {
    json!({ "type": 1, "components": [
        { "type": 2, "custom_id": cid(custom_id!("ticket", "reopen", ticket_id)), "label": "Reopen", "emoji": { "name": "🔓" }, "style": 2 },
        { "type": 2, "custom_id": cid(custom_id!("ticket", "transcript", ticket_id)), "label": "Transcript", "emoji": { "name": "📄" }, "style": 2 },
        { "type": 2, "custom_id": cid(custom_id!("ticket", "delete", ticket_id)), "label": "Delete", "emoji": { "name": "🗑️" }, "style": 4 },
    ]})
}

/// `disableButtons(messageRows, predicate)`: copies of the rows with the (non-link) buttons matching `predicate`
/// disabled; other components are kept.
pub fn disable_buttons_json(rows: &[Value], predicate: &dyn Fn(&Value) -> bool) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            let mut row = row.clone();
            let comps: Vec<Value> = row
                .get("components")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut c| {
                    if c.get("type").and_then(Value::as_u64) == Some(2) && c.get("style").and_then(Value::as_u64) != Some(5) && predicate(&c) {
                        c["disabled"] = json!(true);
                    }
                    c
                })
                .collect();
            row["components"] = Value::Array(comps);
            row
        })
        .collect()
}

fn reaction_of(v: &Value) -> Option<ReactionType> {
    let name = v.get("name").and_then(Value::as_str);
    match v.get("id").and_then(|i| i.as_str().map(str::to_string).or_else(|| i.as_u64().map(|n| n.to_string()))) {
        Some(id) => {
            let id: u64 = id.parse().ok()?;
            Some(ReactionType::Custom {
                animated: v.get("animated").and_then(Value::as_bool).unwrap_or(false),
                id: id.into(),
                name: name.map(str::to_string),
            })
        }
        None => name.map(|n| ReactionType::Unicode(n.to_string())),
    }
}

/// Discord JSON rows (a message's `components`, or the JSON builders above) as serenity builders.
pub fn rows_from_json(rows: &[Value]) -> Vec<CreateActionRow> {
    let mut out = Vec::new();
    for row in rows {
        let comps = row.get("components").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut buttons = Vec::new();
        for c in &comps {
            match c.get("type").and_then(Value::as_u64) {
                Some(2) => {
                    if let Ok(b) = serde_json::from_value::<Button>(c.clone()) {
                        buttons.push(CreateButton::from(b));
                    }
                }
                Some(t @ 3..=8) => {
                    let custom = c.get("custom_id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let kind = match t {
                        3 => CreateSelectMenuKind::String {
                            options: c
                                .get("options")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .map(|o| {
                                    let mut opt = CreateSelectMenuOption::new(
                                        o.get("label").and_then(Value::as_str).unwrap_or_default(),
                                        o.get("value").and_then(Value::as_str).unwrap_or_default(),
                                    );
                                    if let Some(d) = o.get("description").and_then(Value::as_str) {
                                        opt = opt.description(d);
                                    }
                                    if let Some(e) = o.get("emoji").and_then(reaction_of) {
                                        opt = opt.emoji(e);
                                    }
                                    if o.get("default").and_then(Value::as_bool) == Some(true) {
                                        opt = opt.default_selection(true);
                                    }
                                    opt
                                })
                                .collect(),
                        },
                        5 => CreateSelectMenuKind::User { default_users: None },
                        6 => CreateSelectMenuKind::Role { default_roles: None },
                        7 => CreateSelectMenuKind::Mentionable { default_users: None, default_roles: None },
                        _ => CreateSelectMenuKind::Channel { channel_types: None, default_channels: None },
                    };
                    let mut menu = CreateSelectMenu::new(custom, kind);
                    if let Some(p) = c.get("placeholder").and_then(Value::as_str) {
                        menu = menu.placeholder(p);
                    }
                    if let Some(n) = c.get("min_values").and_then(Value::as_u64) {
                        menu = menu.min_values(n as u8);
                    }
                    if let Some(n) = c.get("max_values").and_then(Value::as_u64) {
                        menu = menu.max_values(n as u8);
                    }
                    if c.get("disabled").and_then(Value::as_bool) == Some(true) {
                        menu = menu.disabled(true);
                    }
                    out.push(CreateActionRow::SelectMenu(menu));
                }
                _ => {}
            }
        }
        if !buttons.is_empty() {
            out.push(CreateActionRow::Buttons(buttons));
        }
    }
    out
}

/// A message's component rows as Discord JSON.
pub fn message_rows_json(message: &serenity::all::Message) -> Vec<Value> {
    message.components.iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect()
}

/// `buildPanelComponents(...)` as serenity rows.
pub fn build_panel_components(panel_id: &str, style: TicketPanelStyle, options: &[TicketPanelOption], can_use_emoji: &dyn Fn(&str) -> bool) -> Vec<CreateActionRow> {
    rows_from_json(&panel_components_json(panel_id, style, options, can_use_emoji))
}

/// `buildIntroRow(ticketId, { claimed })`.
pub fn build_intro_row(ticket_id: &str, claimed: bool) -> CreateActionRow {
    rows_from_json(&[intro_row_json(ticket_id, claimed)]).into_iter().next().unwrap_or(CreateActionRow::Buttons(Vec::new()))
}

/// `buildClosedRow(ticketId)`.
pub fn build_closed_row(ticket_id: &str) -> CreateActionRow {
    rows_from_json(&[closed_row_json(ticket_id)]).into_iter().next().unwrap_or(CreateActionRow::Buttons(Vec::new()))
}

/// The field id of the reason form (`ticket-reason`, a registered field prefix).
pub const REASON_FIELD: &str = "ticket-reason";

/// `buildReasonModal(panelId, optionId, label)` as JSON.
pub fn reason_modal_json(panel_id: &str, option_id: &str, label: &str) -> Value {
    json!({
        "custom_id": cid(custom_id!("ticket-modal", panel_id, option_id)),
        "title": clamp_text(&format!("Open a ticket: {label}"), 45),
        "components": [{ "type": 1, "components": [{
            "type": 4, "custom_id": REASON_FIELD, "label": "Why are you opening this ticket?", "style": 2,
            "required": true, "min_length": 1, "max_length": 1000, "placeholder": "Describe your issue…",
        }]}],
    })
}

/// `buildReasonModal(panelId, optionId, label)`.
pub fn build_reason_modal(panel_id: &str, option_id: &str, label: &str) -> CreateModal {
    let j = reason_modal_json(panel_id, option_id, label);
    let input = CreateInputText::new(InputTextStyle::Paragraph, "Why are you opening this ticket?", REASON_FIELD)
        .required(true)
        .min_length(1)
        .max_length(1000)
        .placeholder("Describe your issue…");
    CreateModal::new(j["custom_id"].as_str().unwrap_or_default(), j["title"].as_str().unwrap_or_default()).components(vec![CreateActionRow::InputText(input)])
}
