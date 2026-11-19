//! The texts of `ticketService.js` (§4.3): service-result payloads (`errorPayload`), panel-admin payloads
//! (`panelErrorPayload`), the refresh note, the public lifecycle notices and the panel views. Every payload pings
//! nobody unless the JS says otherwise.

use bot_core::App;
use bot_core::embed_builder::{error_embed, guild_embed, success_embed, warn_embed};
use bot_core::interaction_helpers::Payload;
use bot_core::permissions::permission_names;
use bot_db::{GuildSettings, TicketPanel, TicketPanelOption, TicketPanelStyle};
use bot_util::text::clamp_text;
use serde_json::Value;
use serenity::all::{ChannelId, GuildId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateEmbed};

use super::components::{build_closed_row, build_intro_row};
use super::io::{channel_cached, child_count, guild_available, role_cached, sf};
use super::rules::{CATEGORY_CHILD_LIMIT, CategoryKind, Fail, MAX_ADDED_USERS, PENDING_PREFIX, button_style_name, default_category_name, live_manager_role_ids};

/// `E(embed)`: one embed, no pings.
pub fn e(embed: CreateEmbed) -> Payload {
    Payload::embed(embed).no_mentions()
}

fn channel_ref(id: &str) -> String {
    if id.starts_with(PENDING_PREFIX) { "(being created)".into() } else { format!("<#{id}>") }
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect()).unwrap_or_default()
}

fn ms_of(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => bot_util::js::date_parse(s),
        _ => f64::NAN,
    }
}

/// `errorPayload(result)`'s embed.
pub fn error_embed_for(result: &Fail) -> CreateEmbed {
    let d = &result.data;
    let s = |k: &str| result.str(k);
    let er = |t: &str, x: &str| error_embed(t, Some(x), None);
    let wa = |t: &str, x: &str| warn_embed(t, Some(x), None);
    match result.code.as_str() {
        "PANEL_GONE" => er("Panel Not Found", "This ticket panel no longer exists. Ask a server admin to set it up again."),
        "OPTION_GONE" => er("Option Not Found", "This option was removed. Ask a server admin to republish the panel."),
        "OPEN_LIMIT" => {
            let ids = strs(d.get("ids"));
            let max = d.get("max").and_then(Value::as_i64).unwrap_or(0);
            if max == 1 {
                wa("Ticket Already Open", &format!("You already have an open ticket: {}", ids.first().map(|i| channel_ref(i)).unwrap_or_else(|| "<#undefined>".into())))
            } else {
                let list: Vec<String> = ids.iter().map(|i| channel_ref(i)).collect();
                wa("Ticket Already Open", &format!("You already have {} open tickets here (max {max}): {}", ids.len(), list.join(", ")))
            }
        }
        "BOT_PERMS" => {
            let names = s("names");
            let channel = s("channelId");
            if !channel.is_empty() {
                return er("Missing Permissions", &format!("I need **{names}** in <#{channel}> to do that."));
            }
            let where_ = s("where");
            let place = if where_.is_empty() { String::new() } else { format!(" in **{where_}**") };
            er("Missing Permissions", &format!("I need **{names}**{place} to open tickets. Ask a server admin to fix my permissions."))
        }
        "GUILD_FULL" => er("Server Is Full", "This server has reached Discord's limit of 500 channels, so I can't open a ticket. Ask an admin to delete old channels."),
        "CREATE_FAILED" => er("Could Not Open Ticket", &format!("Discord refused to create the channel: {}. Please try again later.", s("reason"))),
        "SLOW" => er("Try Again", "Ticketing is a bit slow right now. Please try again in a moment."),
        "THROTTLED" => wa("Slow Down", "You just opened a ticket. Please wait a few seconds."),
        "MODULE_OFF" => wa("Module Disabled", "The **Ticketing** module is turned off on this server."),
        "DB_ERROR" => er("Could Not Open Ticket", "Something went wrong while saving your ticket. Please try again."),
        "NOT_TICKET" => er("Not a Ticket", "Run this command inside a ticket channel."),
        "TICKET_NOT_FOUND" => er("Ticket Not Found", "This ticket no longer exists."),
        "TICKET_DELETED" => er("Ticket Deleted", "This ticket was already deleted. You can delete this channel manually."),
        "NOT_STAFF" => er("Staff Only", "Only ticket managers and moderators can do that."),
        "NOT_OWNER_OR_STAFF" => er("Not Allowed", "Only the person who opened this ticket or the ticket staff can close it."),
        "OWN_TICKET" => wa("Not Allowed", "You can't claim your own ticket."),
        "ALREADY_CLAIMED_SELF" => wa("Already Claimed", "You already claimed this ticket."),
        "ALREADY_CLAIMED" => wa("Already Claimed", &format!("This ticket is already claimed by <@{}>.", s("claimedById"))),
        "IS_CLOSED" => wa("Ticket Closed", "This ticket is closed. Reopen it first."),
        "ALREADY_CLOSED" => wa("Already Closed", "This ticket is already closed."),
        "NOT_CLOSED" => wa("Not Closed", "This ticket is not closed."),
        "CLOSE_FIRST" => wa("Close It First", "Close the ticket before deleting it with this button, or use `/ticket delete`."),
        "CONFLICT" => wa("Try Again", "This ticket was changed by someone else just now. Please try again."),
        "DELETING" => wa("Deleting", "This ticket is already being deleted."),
        "RENAME_LIMIT" => wa(
            "Too Many Renames",
            &format!(
                "Discord allows only 2 renames per channel every 10 minutes. Try again {}.",
                bot_util::time::discord_timestamp_ms(ms_of(d.get("retryAt")), "R")
            ),
        ),
        "BAD_NAME" => er("Invalid Name", "That name has no usable characters. Use letters, numbers, - or _."),
        "ADD_BOT" => er("Not Allowed", "Bots can't be added to tickets."),
        "NOT_MEMBER" => er("Member Not Found", "That user isn't in this server."),
        "IS_OWNER" => {
            if s("op") == "remove" {
                er("Not Allowed", "You can't remove the person who opened the ticket. Close the ticket instead.")
            } else {
                wa("Already In Ticket", "The ticket opener already has access.")
            }
        }
        "ALREADY_ADDED" => wa("Already Added", &format!("<@{}> already has access to this ticket.", s("userId"))),
        "ADDED_LIMIT" => er("Too Many Members", &format!("A ticket can have at most {MAX_ADDED_USERS} added members.")),
        "NOT_ADDED" => wa("Not Added", &format!("<@{}> was not added to this ticket with `/ticket add`.", s("userId"))),
        "TRANSCRIPT_THROTTLED" => wa("Please Wait", "A transcript of this ticket was made less than a minute ago. Try again shortly."),
        "TRANSCRIPT_FAILED" => er("Transcript Failed", &format!("I couldn't read this channel's history: {}.", s("reason"))),
        "TRANSCRIPT_CHANNEL" => er(
            "Transcript Channel Unusable",
            &format!(
                "I can't post transcripts in <#{}> ({}). Fix my permissions there or turn channel transcripts off with `/ticket-panel transcripts`. The ticket was not deleted.",
                s("channelId"),
                s("reason")
            ),
        ),
        "TRANSCRIPT_ABORT" => er("Transcript Failed", &format!("I couldn't save the transcript ({}), so the ticket was not deleted.", s("reason"))),
        "DELETE_FAILED" => {
            let saved = d.get("transcriptSaved").and_then(Value::as_bool).unwrap_or(false);
            er("Delete Failed", &format!("I couldn't delete this channel: {}.{}", s("reason"), if saved { " The transcript was already saved." } else { "" }))
        }
        _ => er("Something Went Wrong", "That did not work. Please try again."),
    }
}

/// `errorPayload(result)`: a service refusal as a reply payload (no `ephemeral`: the caller sets it).
pub fn error_payload(result: &Fail) -> Payload {
    e(error_embed_for(result))
}

/// `errorPayload({ code })`.
pub fn error_payload_code(code: &str) -> Payload {
    error_payload(&Fail::new(code))
}

/// `permLine(u, id)`: what is wrong with a channel (`missing` keys, else the `reason`).
pub fn perm_line(missing: &[String], reason: Option<&str>, id: &str) -> String {
    if missing.is_empty() {
        format!("{}.", reason.unwrap_or("undefined"))
    } else {
        format!("I need **{}** in <#{id}>.", permission_names(missing).join(", "))
    }
}

/// `panelErrorPayload(code, d)`'s embed. `d` holds the placeholders: `name`, `error`, `missing`, `reason`, `id`,
/// `label`, `roleId`, `panel`, `names`, `category`, `errors`.
pub fn panel_error_embed(code: &str, d: &Value) -> CreateEmbed {
    let s = |k: &str| match d.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => "undefined".into(),
        Some(v) => v.to_string(),
    };
    let er = |t: &str, x: &str| error_embed(t, Some(x), None);
    let wa = |t: &str, x: &str| warn_embed(t, Some(x), None);
    let missing = strs(d.get("missing"));
    let limits = |n: usize| n.to_string();
    match code {
        "NAME_TAKEN" => er("Name Taken", &format!("A panel called **{}** already exists.", s("name"))),
        "PANEL_LIMIT" => er("Too Many Panels", &format!("This server already has {} ticket panels. Delete one first.", limits(bot_config::limits::TICKET_PANELS))),
        "PANEL_NOT_FOUND" => er("Panel Not Found", "Pick a panel from the list."),
        "BAD_FORMAT" => er("Invalid Name Format", &s("error")),
        "BAD_PANEL_NAME" => er("Invalid Name", "Panel names must be 1–50 characters on one line."),
        "CHANNEL_UNUSABLE" => er("Cannot Post There", &perm_line(&missing, d.get("reason").and_then(Value::as_str), &s("id"))),
        "TRANSCRIPT_CHANNEL_UNUSABLE" => er("Cannot Post Transcripts There", &perm_line(&missing, d.get("reason").and_then(Value::as_str), &s("id"))),
        "PERMS_SERVER" => er(
            "Missing Permissions",
            &format!("To open tickets I need **{}** on this server. Fix my role, then publish again.", permission_names(&missing).join(", ")),
        ),
        "PUBLISH_FAILED" => er("Could Not Publish", &format!("I couldn't post the panel: {}.", s("reason"))),
        "BAD_LABEL" => er("Invalid Label", "Option labels must be 1–80 characters."),
        "OPTION_LIMIT" => er("Too Many Options", &format!("A panel can have at most {} options.", limits(bot_config::limits::TICKET_PANEL_OPTIONS))),
        "OPTION_DUP" => er("Duplicate Option", &format!("This panel already has an option called **{}**.", s("label"))),
        "BAD_EMOJI" => er("Invalid Emoji", &s("error")),
        "OPTION_NOT_FOUND" => er("Option Not Found", "Pick an option from the list."),
        "LAST_OPTION" => er("Last Option", "A panel needs at least one option. Add another option before removing this one."),
        "EVERYONE_ROLE" => er("Invalid Role", "@everyone cannot be a Ticket Manager role."),
        "ALREADY_MANAGER" => wa("Already a Manager", &format!("<@&{}> is already a Ticket Manager role of **{}**.", s("roleId"), s("panel"))),
        "NOT_MANAGER" => wa("Not a Manager", &format!("<@&{}> is not a Ticket Manager role of **{}**.", s("roleId"), s("panel"))),
        "MANAGER_LIMIT" => er(
            "Too Many Manager Roles",
            &format!("A panel can have at most {} Ticket Manager roles.", limits(bot_config::limits::TICKET_MANAGER_ROLES)),
        ),
        "CATEGORY_PERMS" => er("Missing Permissions", &format!("I need **{}** in the **{}** category.", s("names"), s("category"))),
        "CONFLICT_CHANNEL" => er("Conflicting Options", "Use either `channel` or `disable:true`, not both."),
        "CONFLICT_CATEGORY" => er("Conflicting Options", "Use either `category` or `clear_category:true`, not both."),
        "INVALID_MESSAGE" => {
            let lines: Vec<String> = strs(d.get("errors")).iter().map(|x| format!("• {x}")).collect();
            er("Invalid Message", &lines.join("\n"))
        }
        "BAD_COMBO" => er("Invalid Options", "The `option` setting only works with `type: Ticket intro`."),
        "NOT_ADMIN" => er("Insufficient Permissions", "You need the Admin level to edit ticket panels."),
        "FORM_EXPIRED" => er("Form Expired", "This form is no longer valid. Run `/ticket-panel message` again."),
        "PANEL_DELETED" => er("Panel Not Found", "This panel was deleted while the form was open."),
        "SLOW" => er("Try Again", "Ticketing is a bit slow right now. Please try again in a moment."),
        _ => er("Something Went Wrong", "That did not work. Please try again."),
    }
}

/// `panelErrorPayload(code, d)`.
pub fn panel_error_payload(code: &str, d: &Value) -> Payload {
    e(panel_error_embed(code, d))
}

/// The result of `refreshPanelMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refresh {
    /// Edited, or nothing published (`skipped`).
    Ok { skipped: bool },
    /// The published message (or its channel) is gone; the panel was cleared.
    Gone,
    /// The bot cannot use the channel.
    NoAccess { channel_id: String },
    /// Anything else.
    Error { reason: String },
}

impl Refresh {
    pub fn is_ok(&self) -> bool {
        matches!(self, Refresh::Ok { .. })
    }

    /// The JS result object.
    pub fn to_json(&self) -> Value {
        match self {
            Refresh::Ok { skipped: true } => serde_json::json!({ "ok": true, "skipped": true }),
            Refresh::Ok { skipped: false } => serde_json::json!({ "ok": true }),
            Refresh::Gone => serde_json::json!({ "ok": false, "code": "GONE" }),
            Refresh::NoAccess { channel_id } => serde_json::json!({ "ok": false, "code": "NO_ACCESS", "channelId": channel_id }),
            Refresh::Error { reason } => serde_json::json!({ "ok": false, "code": "ERROR", "reason": reason }),
        }
    }
}

/// `refreshNote(r)`: the line for a failed panel refresh, or '' when there is nothing to say.
pub fn refresh_note(r: Option<&Refresh>) -> String {
    match r {
        None | Some(Refresh::Ok { .. }) => String::new(),
        Some(Refresh::Gone) => "\n⚠️ The published panel message was deleted. Run `/ticket-panel publish` to post it again.".into(),
        Some(Refresh::NoAccess { channel_id }) => format!("\n⚠️ I couldn't update the published panel in <#{channel_id}> (missing access)."),
        Some(Refresh::Error { reason }) => format!("\n⚠️ I couldn't update the published panel ({reason})."),
    }
}

pub const MANAGERS_WARNING: &str = "⚠️ This panel has no Ticket Manager roles, so only moderators and admins can see its tickets. Add one with `/ticket-panel managers add`.";

// ── Notices (§6.8) ────────────────────────────────────────────────────────────

/// A lifecycle notice and what it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Claim { actor_id: String, previous_id: Option<String> },
    Close { actor_id: String, ticket_id: String, reason: Option<String> },
    Reopen { actor_id: String, ticket_id: String, claimed: bool },
    Add { actor_id: String, user_id: String },
    Remove { actor_id: String, user_id: String },
    Rename { actor_id: String, channel_id: String },
    Delete { actor_id: String, transcript_channel_id: Option<String>, dm: bool },
}

/// `deleteNoticeText(d)`: who deleted it, and where the transcript went.
pub fn delete_notice_text(actor_id: &str, transcript_channel_id: Option<&str>, dm: bool) -> String {
    let mut lines = vec![format!("Deleted by <@{actor_id}>. This channel will be deleted in 5 seconds.")];
    if let Some(c) = transcript_channel_id {
        lines.push(format!("The transcript was saved in <#{c}>."));
    }
    if dm {
        lines.push("A copy was sent to the opener by DM.".into());
    }
    lines.join("\n")
}

/// `buildNotice(kind, d, settings)`: the public notice for a lifecycle event.
pub fn build_notice(n: &Notice, settings: Option<&GuildSettings>) -> Payload {
    let ge = |t: &str, d: &str| guild_embed(settings, Some(t), Some(d), None);
    match n {
        Notice::Claim { actor_id, previous_id } => {
            let text = match previous_id {
                Some(p) => format!("<@{actor_id}> took over this ticket from <@{p}>."),
                None => format!("<@{actor_id}> is handling this ticket."),
            };
            e(ge("🙋 Ticket Claimed", &text))
        }
        Notice::Close { actor_id, ticket_id, reason } => {
            let mut embed = ge("🔒 Ticket Closed", &format!("Closed by <@{actor_id}>."));
            if let Some(r) = reason.as_deref().filter(|r| !r.is_empty()) {
                embed = embed.field("Reason", clamp_text(r, 1024), false);
            }
            e(embed).components(vec![build_closed_row(ticket_id)])
        }
        Notice::Reopen { actor_id, ticket_id, claimed } => {
            e(ge("🔓 Ticket Reopened", &format!("Reopened by <@{actor_id}>."))).components(vec![build_intro_row(ticket_id, *claimed)])
        }
        Notice::Add { actor_id, user_id } => {
            let users: Vec<UserId> = sf(user_id).map(UserId::new).into_iter().collect();
            Payload::embed(ge("➕ Member Added", &format!("<@{user_id}> was added by <@{actor_id}>."))).allowed_mentions(CreateAllowedMentions::new().users(users))
        }
        Notice::Remove { actor_id, user_id } => e(ge("➖ Member Removed", &format!("<@{user_id}> was removed by <@{actor_id}>."))),
        Notice::Rename { actor_id, channel_id } => e(ge("✏️ Ticket Renamed", &format!("Renamed to <#{channel_id}> by <@{actor_id}>."))),
        Notice::Delete { actor_id, transcript_channel_id, dm } => {
            e(ge("🗑️ Deleting Ticket", &delete_notice_text(actor_id, transcript_channel_id.as_deref(), *dm)))
        }
    }
}

// ── Views (§6.4.5) ────────────────────────────────────────────────────────────

fn style_name(style: TicketPanelStyle) -> &'static str {
    if style == TicketPanelStyle::Select { "Dropdown" } else { "Buttons" }
}

fn mention_roles(ids: &[String]) -> String {
    if ids.is_empty() { "none".into() } else { ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" ") }
}

/// The panel's live manager roles from the cache.
pub fn live_managers(app: &App, guild_id: GuildId, panel: &TicketPanel) -> Vec<String> {
    let g = guild_id.to_string();
    live_manager_role_ids(&panel.manager_role_ids, &g, &|id| role_cached(app, guild_id, id))
}

fn category_line(app: &App, guild_id: GuildId, label: &str, id: Option<&str>, kind: CategoryKind) -> String {
    let Some(id) = id else {
        return format!("**{label}:** not set — I create \"{}\" when it is first needed", default_category_name(kind));
    };
    if channel_cached(app, guild_id, id).is_some() {
        let n = sf(id).map(|c| child_count(app, guild_id, ChannelId::new(c))).unwrap_or(0);
        return format!("**{label}:** <#{id}> ({n}/{CATEGORY_CHILD_LIMIT} channels)");
    }
    if !guild_available(app, guild_id) {
        return format!("**{label}:** <#{id}>");
    }
    format!("**{label}:** deleted — I will create a new one when needed")
}

/// `categoriesText(panel, guild)`: the three category lines of a panel.
pub fn categories_text(app: &App, guild_id: GuildId, panel: &TicketPanel) -> String {
    [
        category_line(app, guild_id, "Open", panel.open_category_id.as_deref(), CategoryKind::Open),
        category_line(app, guild_id, "Claimed", panel.claimed_category_id.as_deref(), CategoryKind::Claimed),
        category_line(app, guild_id, "Closed", panel.closed_category_id.as_deref(), CategoryKind::Closed),
    ]
    .join("\n")
}

/// `transcriptsText(panel)`: the transcript settings of a panel.
pub fn transcripts_text(transcript_channel_id: Option<&str>, dm_transcript: bool) -> String {
    [
        format!("**Channel:** {}", transcript_channel_id.map(|c| format!("<#{c}>")).unwrap_or_else(|| "off".into())),
        format!("**DM the opener:** {}", if dm_transcript { "Yes" } else { "No" }),
        "Transcripts are made when a ticket is deleted, and with `/ticket transcript`.".into(),
    ]
    .join("\n")
}

/// `buildCategoriesEmbed(panel, guild, settings)`.
pub fn build_categories_embed(app: &App, guild_id: GuildId, panel: &TicketPanel, settings: Option<&GuildSettings>) -> CreateEmbed {
    guild_embed(settings, Some(&format!("🎫 {} — categories", clamp_text(&panel.name, 100))), Some(&categories_text(app, guild_id, panel)), None)
}

/// `buildTranscriptsEmbed(panel, settings)`.
pub fn build_transcripts_embed(panel: &TicketPanel, settings: Option<&GuildSettings>) -> CreateEmbed {
    guild_embed(
        settings,
        Some(&format!("🎫 {} — transcripts", clamp_text(&panel.name, 100))),
        Some(&transcripts_text(panel.transcript_channel_id.as_deref(), panel.dm_transcript)),
        None,
    )
}

/// `buildPanelDetailsEmbed(panel, guild, settings)`.
pub fn build_panel_details_embed(app: &App, guild_id: GuildId, panel: &TicketPanel, option_count: usize, settings: Option<&GuildSettings>) -> CreateEmbed {
    let published = match (&panel.channel_id, &panel.message_id) {
        (Some(c), Some(_)) => format!("<#{c}>"),
        _ => "not published".into(),
    };
    let managers = live_managers(app, guild_id, panel);
    let transcripts = format!(
        "{}, {}",
        panel.transcript_channel_id.as_ref().map(|c| format!("<#{c}>")).unwrap_or_else(|| "off".into()),
        if panel.dm_transcript { "DMs on" } else { "DMs off" }
    );
    guild_embed(settings, Some(&clamp_text(&format!("🎫 {}", clamp_text(&panel.name, 200)), 256)), None, None)
        .field("Style", style_name(panel.style), true)
        .field("Published", published, true)
        .field("Ask for a reason", if panel.ask_reason { "Yes" } else { "No" }, true)
        .field("Open tickets per member", panel.max_open_per_user.to_string(), true)
        .field("Channel name", clamp_text(&format!("`{}`", panel.channel_name_format), 1024), true)
        .field(
            "Manager roles",
            if managers.is_empty() { "none — only moderators and admins".to_string() } else { clamp_text(&mention_roles(&managers), 1024) },
            false,
        )
        .field("Categories", clamp_text(&categories_text(app, guild_id, panel), 1024), false)
        .field("Transcripts", transcripts, false)
        .field("Options", format!("{option_count}/{}", bot_config::limits::TICKET_PANEL_OPTIONS), true)
}

/// `optionLine(option, index, panel)`: one line of `option list`.
pub fn option_line(option: &TicketPanelOption, index: usize, panel_style: TicketPanelStyle) -> String {
    let style = if panel_style == TicketPanelStyle::Buttons {
        format!(" — {}", button_style_name(option.button_style).unwrap_or("Blue"))
    } else {
        String::new()
    };
    let desc = option.description.as_deref().filter(|d| !d.is_empty()).map(|d| format!(" · {}", clamp_text(d, 100))).unwrap_or_default();
    let cat = option.category_id.as_deref().filter(|c| !c.is_empty()).map(|c| format!(" · opens in <#{c}>")).unwrap_or_default();
    format!("**{}.** {} **{}**{style}{desc}{cat}", index + 1, option.emoji.as_deref().unwrap_or("▫️"), clamp_text(&option.label, 80))
}

/// `panelListLine({ panel, active }, guild)`: one line of `/ticket-panel list`.
pub fn panel_list_line(app: &App, guild_id: GuildId, panel: &TicketPanel, option_count: usize, active: i64) -> String {
    let published = match (&panel.channel_id, &panel.message_id) {
        (Some(c), Some(_)) => format!("<#{c}>"),
        _ => "not published".into(),
    };
    let managers = live_managers(app, guild_id, panel).len();
    format!(
        "**{}** — {} · {option_count} option(s) · {managers} manager role(s) · {active} active ticket(s) · {published}",
        clamp_text(&panel.name, 60),
        style_name(panel.style)
    )
}

/// The success reply of an open (`replyOpenResult`).
pub fn opened_embed(channel_id: &str, notes: &[String]) -> CreateEmbed {
    success_embed("Ticket Opened", Some(&format!("Your ticket is ready: <#{channel_id}>{}", notes_block(notes))), None)
}

/// `notesBlock(notes)`.
pub fn notes_block(notes: &[String]) -> String {
    if notes.is_empty() { String::new() } else { format!("\n\n{}", notes.iter().map(|n| format!("⚠️ {n}")).collect::<Vec<_>>().join("\n")) }
}
