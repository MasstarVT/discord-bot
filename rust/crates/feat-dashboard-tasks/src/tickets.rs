//! Port of `src/services/dashboardTasks/tickets.js`: the dashboard tasks of the Ticketing module. The dashboard saves a
//! panel in the database; these tasks do the part that needs the gateway client, with the same service functions and
//! permission checks as /ticket-panel publish | categories | transcripts | managers | delete:
//!   `ticket.check`    `{ categoryIds, transcriptChannelId? }`  the bot's permissions in categories and a transcript channel
//!   `ticket.publish`  `{ panelId, channelId }`                  post the panel message, or move / update it (publishPanel)
//!   `ticket.refresh`  `{ panelId, message?, managersAdded?, managersRemoved? }`
//!                     update the posted message (refreshPanelMessage) and merge the changed manager roles into the
//!                     guild's one ticket.managersync job, which back-fills existing tickets, so the task stays short
//!   `ticket.delete`   `{ panelId }`                             delete the panel and its message (deletePanel)
//! Every task re-checks that the dashboard user is still an admin (the level /ticket-panel needs) and re-validates its
//! payload.

use bot_config::{JobType, PermissionLevel, limits};
use bot_core::App;
use bot_core::error::log_arg;
use bot_core::interaction_helpers::{ChannelUsable, PermTarget, bot_permissions_for, check_channel_usable_for, missing_bot_permissions, missing_from};
use bot_core::permissions::permission_names;
use bot_core::scheduler::ScheduleJob;
use bot_core::settings::Settings;
use bot_util::text::clamp_text;
use feat_tickets::ticket_service::{
    self as ts, OPEN_REQUIRED_PERMS, PanelWithOptions, PublishError, Refresh, manager_sync_dedupe_key, manager_sync_payload, merge_manager_sync,
};
use serde_json::{Value, json};
use serenity::all::{ChannelType, GuildChannel};

use crate::index::{Actor, DashboardTask, TaskCtx, TaskOutcome};
use crate::reaction_roles::is_panel_id;
use crate::shared::{TaskError, task_error};
use crate::util;

/// What the bot needs in a ticket category (the /ticket-panel categories and option add checks).
pub const CATEGORY_PERMS: [&str; 3] = ["ViewChannel", "ManageChannels", "ManageRoles"];
/// What the bot needs to post transcripts (the /ticket-panel transcripts check).
pub const TRANSCRIPT_PERMS: [&str; 4] = ["ViewChannel", "SendMessages", "EmbedLinks", "AttachFiles"];
pub const MAX_CATEGORIES: usize = limits::TICKET_PANEL_OPTIONS + 3;

fn postable(channel: &GuildChannel) -> bool {
    matches!(channel.kind, ChannelType::Text | ChannelType::News)
}

/// The settings, after checking the dashboard user may still change ticket panels (the Admin level /ticket-panel needs).
async fn authorize(app: &App, guild_id: &str, actor: &Actor) -> anyhow::Result<Option<Settings>> {
    if !actor.is_bot_owner && actor.member.is_none() {
        return Err(task_error("You are no longer a member of this server."));
    }
    let settings = app.settings().get_settings_safe(guild_id).await;
    if actor.is_bot_owner {
        return Ok(settings);
    }
    let level = actor.member.as_ref().map(|m| util::member_level(app, guild_id, m, settings.as_deref())).unwrap_or(PermissionLevel::Member);
    if level < PermissionLevel::Admin {
        return Err(task_error("You need the Admin level (Manage Server) to change ticket panels."));
    }
    Ok(settings)
}

async fn load_panel(app: &App, guild_id: &str, payload: &Value) -> anyhow::Result<PanelWithOptions> {
    let Some(id) = payload.get("panelId").and_then(Value::as_str).filter(|id| is_panel_id(id)) else {
        return Err(task_error("That panel is not valid."));
    };
    match ts::find_panel(app.db(), guild_id, id).await? {
        Some(p) if p.panel.id == id => Ok(p),
        _ => Err(task_error("That ticket panel no longer exists.")),
    }
}

fn snowflake(value: Option<&Value>, what: &str) -> Result<String, TaskError> {
    match value.and_then(Value::as_str) {
        Some(s) if util::is_snowflake(s) => Ok(s.to_string()),
        _ => Err(TaskError::new(format!("{what} is not valid."))),
    }
}

async fn channel_of(app: &App, guild_id: &str, id: &str) -> anyhow::Result<GuildChannel> {
    util::find_channel(app, guild_id, id).await.ok_or_else(|| task_error("That channel does not exist in this server any more. Pick another one."))
}

/// A usable-channel failure as one sentence.
fn unusable(channel: &GuildChannel, check: &ChannelUsable) -> String {
    if !check.missing.is_empty() {
        format!("I need {} in #{}.", permission_names(&check.missing).join(", "), channel.name)
    } else {
        format!("I can't use #{}: {}.", channel.name, check.reason.clone().unwrap_or_default())
    }
}

fn missing_in(app: &App, channel: &GuildChannel, perms: &[&str]) -> Vec<String> {
    missing_from(bot_permissions_for(app, channel.guild_id, channel), perms)
}

/// `ticket.check`.
pub async fn check(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    authorize(app, guild_id, &ctx.actor).await?;
    let raw: Vec<Value> = payload.get("categoryIds").and_then(Value::as_array).map(|a| a.iter().take(MAX_CATEGORIES).cloned().collect()).unwrap_or_default();
    let mut ids: Vec<Value> = Vec::new();
    for v in raw {
        if !ids.contains(&v) {
            ids.push(v);
        }
    }
    for raw in &ids {
        let channel = channel_of(app, guild_id, &snowflake(Some(raw), "A category")?).await?;
        if channel.kind != ChannelType::Category {
            return Err(task_error(format!("#{} is not a category.", channel.name)));
        }
        let missing = missing_in(app, &channel, &CATEGORY_PERMS);
        if !missing.is_empty() {
            return Err(task_error(format!("I need {} in the \"{}\" category.", permission_names(&missing).join(", "), channel.name)));
        }
    }
    let transcript = payload.get("transcriptChannelId");
    if !util::is_nullish(transcript) {
        let channel = channel_of(app, guild_id, &snowflake(transcript, "The transcript channel")?).await?;
        if !postable(&channel) {
            return Err(task_error("Transcripts can only be posted in text or announcement channels."));
        }
        let usable = check_channel_usable_for(app, channel.guild_id, &channel, &TRANSCRIPT_PERMS);
        if !usable.ok {
            return Err(task_error(unusable(&channel, &usable)));
        }
    }
    Ok(TaskOutcome::message("The bot can use those channels."))
}

/// The warnings that follow a successful publish: no manager roles, and an open category the bot cannot use.
fn publish_warnings(app: &App, guild_id: &str, panel: &PanelWithOptions) -> Vec<String> {
    let mut lines = Vec::new();
    let role_exists = |id: &str| util::role_cached(app, guild_id, id);
    if ts::live_manager_role_ids(&panel.panel.manager_role_ids, guild_id, &role_exists).is_empty() {
        lines.push("This panel has no ticket manager roles, so only moderators and admins can see its tickets.".to_string());
    }
    if let Some(open) = panel.panel.open_category_id.as_deref().and_then(|id| util::cached_channel(app, guild_id, id))
        && !missing_in(app, &open, &CATEGORY_PERMS).is_empty()
    {
        lines.push(format!("I can't manage channels in the \"{}\" category, so tickets would fail to open there.", open.name));
    }
    lines
}

/// `ticket.publish`.
pub async fn publish(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    let settings = authorize(app, guild_id, &ctx.actor).await?;
    let panel = load_panel(app, guild_id, payload).await?;
    let channel = channel_of(app, guild_id, &snowflake(payload.get("channelId"), "That channel")?).await?;
    if !postable(&channel) {
        return Err(task_error("Panels can only be posted in text or announcement channels."));
    }
    let usable = check_channel_usable_for(app, channel.guild_id, &channel, &["ViewChannel", "SendMessages", "EmbedLinks"]);
    if !usable.ok {
        return Err(task_error(unusable(&channel, &usable)));
    }
    let Some(g) = util::gid(guild_id) else { return Err(task_error("That channel is not valid.")) };
    let missing = missing_bot_permissions(app, PermTarget::Guild(g), &OPEN_REQUIRED_PERMS);
    if !missing.is_empty() {
        return Err(task_error(format!(
            "To open tickets I need {} on this server. Fix my role, then publish again.",
            permission_names(&missing).join(", ")
        )));
    }

    ts::invalidate_panel(app, &panel.panel.id); // the bot's own memo of the panel may predate the dashboard's changes
    let r = ts::publish_panel(app, g, &panel.panel.id, &channel, settings.map(|s| (*s).clone())).await;
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            let reason = match &e {
                PublishError::PanelGone => "the panel was deleted".to_string(),
                PublishError::Failed(err) => ts::describe_discord_error(err),
            };
            return Err(task_error(format!("I couldn't post the panel: {reason}.")));
        }
    };
    let lead = format!("{} \"{}\" in #{}.", if r.edited() { "Updated" } else { "Posted" }, clamp_text(&panel.panel.name, 80), channel.name);
    let message = std::iter::once(lead).chain(publish_warnings(app, guild_id, &panel)).collect::<Vec<_>>().join(" ");
    let sent = r.message();
    Ok(TaskOutcome::new(
        message,
        json!({
            "panelId": panel.panel.id,
            "channelId": channel.id.to_string(),
            "messageId": sent.id.to_string(),
            "url": sent.url,
            "mode": if r.edited() { "edited" } else { "posted" },
        }),
    ))
}

/// `refreshFailure(r, guild)`: the sentence for a failed refreshPanelMessage.
fn refresh_failure(app: &App, guild_id: &str, r: &Refresh) -> String {
    match r {
        Refresh::Gone => "The published panel message was deleted. Use \"Publish\" to post it again.".into(),
        Refresh::NoAccess { channel_id } => match util::cached_channel_name(app, guild_id, channel_id) {
            Some(name) => format!("I couldn't update the published panel in #{name} (missing access)."),
            None => "I couldn't update the published panel (missing access).".into(),
        },
        Refresh::Error { reason } => format!("I couldn't update the published panel ({reason})."),
        Refresh::Ok { .. } => "I couldn't update the published panel (unknown error).".into(),
    }
}

/// `roleList(value)`: a list of role ids from the payload, once each.
fn role_list(value: Option<&Value>) -> Result<Vec<String>, TaskError> {
    if util::is_nullish(value) {
        return Ok(Vec::new());
    }
    let Some(arr) = value.and_then(Value::as_array) else { return Err(TaskError::new("That request is not valid.")) };
    let mut out: Vec<String> = Vec::new();
    for v in arr.iter().take(limits::TICKET_MANAGER_ROLES) {
        let id = snowflake(Some(v), "A role")?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// `ticket.refresh`.
pub async fn refresh(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    authorize(app, guild_id, &ctx.actor).await?;
    let panel = load_panel(app, guild_id, payload).await?;
    let managers = &panel.panel.manager_role_ids;
    let added: Vec<String> =
        role_list(payload.get("managersAdded"))?.into_iter().filter(|id| managers.contains(id) && util::role_cached(app, guild_id, id)).collect();
    let removed: Vec<String> = role_list(payload.get("managersRemoved"))?.into_iter().filter(|id| !managers.contains(id)).collect();
    ts::invalidate_panel(app, &panel.panel.id);
    let Some(g) = util::gid(guild_id) else { return Err(task_error("That panel is not valid.")) };

    let mut parts: Vec<String> = Vec::new();
    let mut failure = None;
    if payload.get("message") != Some(&Value::Bool(false)) {
        let r = ts::refresh_panel_message(app, g, &panel.panel.id).await;
        match r {
            Refresh::Ok { skipped: true } => {
                if added.len() + removed.len() == 0 {
                    return Err(task_error("This panel is not published, so there is no message to update. Use \"Publish\" first."));
                }
                parts.push("The panel is not published, so there is no message to update.".into());
            }
            Refresh::Ok { skipped: false } => parts.push("Updated the published panel message.".into()),
            other => failure = Some(refresh_failure(app, guild_id, &other)),
        }
    }

    // Existing tickets get the manager roles too (the newest 50), like /ticket-panel managers add | remove. The guild's
    // one ticket.managersync job does it; the change is merged into that job (never a second copy, and a running one
    // is not reset); the job decides add or remove per role from the panel's managers when it runs.
    if !added.is_empty() || !removed.is_empty() {
        let roles: Vec<String> = added.iter().chain(removed.iter()).cloned().collect();
        let job = ScheduleJob::new(guild_id, JobType::TicketManagersync, app.clock().now())
            .dedupe_key(manager_sync_dedupe_key(guild_id))
            .payload(manager_sync_payload(&panel.panel.id, &roles))
            .max_attempts(3)
            .merge(merge_manager_sync);
        match app.scheduler().schedule_job(job).await {
            Ok(_) => parts.push("Existing tickets are being updated in the background (the 50 newest).".into()),
            Err(err) => {
                bot_util::log_error!(format!("Dashboard: could not queue the manager sync of panel {}", panel.panel.id), log_arg(&err));
                parts.push("I couldn't queue the update of existing tickets. Try saving the managers again.".into());
            }
        }
    }
    if let Some(failure) = failure {
        return Err(task_error(std::iter::once(failure).chain(parts).collect::<Vec<_>>().join(" ")));
    }
    let message = if parts.is_empty() { "Done.".to_string() } else { parts.join(" ") };
    Ok(TaskOutcome::new(message, json!({ "panelId": panel.panel.id })))
}

/// `ticket.delete`.
pub async fn remove(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    authorize(app, guild_id, &ctx.actor).await?;
    let panel = load_panel(app, guild_id, &ctx.payload).await?;
    let Some(g) = util::gid(guild_id) else { return Err(task_error("That panel is not valid.")) };
    let open = ts::delete_panel(app, g, &panel.panel).await?;
    let tail = if open > 0 { format!(" {open} ticket(s) keep their channels; only moderators and admins can manage them now.") } else { String::new() };
    let with_message = if panel.panel.channel_id.is_some() && panel.panel.message_id.is_some() { " and its published message" } else { "" };
    Ok(TaskOutcome::new(format!("Deleted \"{}\"{with_message}.{tail}", clamp_text(&panel.panel.name, 80)), json!({ "panelId": panel.panel.id })))
}

/// `tasks` of tickets.js.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![
        ("ticket.check", DashboardTask::new(check)),
        ("ticket.publish", DashboardTask::new(publish)),
        ("ticket.refresh", DashboardTask::new(refresh)),
        ("ticket.delete", DashboardTask::new(remove)),
    ]
}
