//! Port of `src/services/dashboardTasks/reactionRoles.js`: the dashboard tasks of the Reaction Roles module. The
//! dashboard saves a panel in the database; these tasks do the part that needs the gateway client, with the same
//! service functions and permission checks as /reactionrole create | add | remove | edit | repost | delete:
//!   `rr.publish`  `{ groupId, mode: 'post' | 'sync', removedEmojis? }`  post a panel that is not posted yet, or bring the
//!                 posted message (and the bot's reactions) up to date with the saved panel
//!   `rr.repost`   `{ groupId, channelId? }`  post the panel again and clean up the old message
//!   `rr.delete`   `{ groupId }`              delete the panel and its message
//!   `rr.roleTemplate` `{ groupId, template }` create or reuse a role template's roles, add them to the (empty) panel
//!                 and post it
//! Every task re-checks that the dashboard user is still an admin (the level /reactionrole needs) and re-validates its
//! payload.

use bot_config::PermissionLevel;
use bot_core::App;
use bot_core::deletion_tracker::mark_bot_deletion;
use bot_core::interaction_helpers::check_channel_usable_for;
use bot_core::settings::Settings;
use bot_util::text::clamp_text;
use feat_reactionroles::reaction_role_service::{
    self as rr, EnsureResult, Fetched, MessageInfo, NewItem, Panel, PanelArg, discord_ops, emoji_key_of_stored, index_items,
};
use feat_reactionroles::role_template_setup::{self, RoleTemplateError};
use feat_reactionroles::role_templates::get_role_template;
use serde_json::{Value, json};
use serenity::all::{ChannelType, GuildChannel};

use crate::index::{Actor, DashboardTask, TaskCtx, TaskOutcome};
use crate::shared::{TaskError, task_error};
use crate::util;

/// The bot's message of a panel that has not been posted yet (reactionRoles.store.js writes it).
pub const UNPOSTED: &str = "0";
const MAX_REMOVED_EMOJIS: usize = 25;

/// `/^[A-Za-z0-9_-]{1,40}$/`.
pub fn is_panel_id(s: &str) -> bool {
    (1..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The settings, after checking the dashboard user may still change panels (the Admin level /reactionrole needs).
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
        return Err(task_error("You need the Admin level (Manage Server) to change reaction role panels."));
    }
    Ok(settings)
}

fn group_id_of(payload: &Value) -> Result<String, TaskError> {
    match payload.get("groupId").and_then(Value::as_str) {
        Some(id) if is_panel_id(id) => Ok(id.to_string()),
        _ => Err(TaskError::new("That panel is not valid.")),
    }
}

async fn load_group(app: &App, guild_id: &str, payload: &Value) -> anyhow::Result<Panel> {
    let id = group_id_of(payload)?;
    rr::get_panel(app.db(), guild_id, &id).await?.ok_or_else(|| task_error("That panel no longer exists."))
}

/// A channel to post in: text or announcement, of this server.
async fn post_channel(app: &App, guild_id: &str, id: &str, strict_type: bool) -> anyhow::Result<GuildChannel> {
    let Some(channel) = util::find_channel(app, guild_id, id).await else {
        return Err(task_error("That channel does not exist in this server any more. Pick another one."));
    };
    if strict_type && !matches!(channel.kind, ChannelType::Text | ChannelType::News) {
        return Err(task_error("Panels can only be posted in text or announcement channels."));
    }
    Ok(channel)
}

/// What the bot needs in the channel to post this panel (the same list /reactionrole create and repost use).
pub fn needs_for(panel: &Panel) -> Vec<&'static str> {
    let mut needs = vec!["ViewChannel", "SendMessages"];
    if !panel.group.plain_text {
        needs.push("EmbedLinks");
    }
    if panel.layout() == "REACTIONS" {
        needs.extend(["AddReactions", "ReadMessageHistory"]);
    }
    needs
}

fn require_usable(app: &App, guild_id: &str, channel: &GuildChannel, needs: &[&str]) -> Result<(), TaskError> {
    let Some(g) = util::gid(guild_id) else { return Ok(()) };
    let check = check_channel_usable_for(app, g, channel, needs);
    if check.ok {
        return Ok(());
    }
    Err(TaskError::new(if !check.missing.is_empty() {
        format!("I need {} in #{}.", bot_core::permissions::permission_names(&check.missing).join(", "), channel.name)
    } else {
        format!("I can't post in #{}: {}.", channel.name, check.reason.unwrap_or_default())
    }))
}

fn lacks_manage_messages(app: &App, guild_id: &str, channel_id: &str) -> bool {
    !discord_ops(app).bot_has_manage_messages(guild_id, channel_id)
}

fn title(panel: &Panel) -> String {
    clamp_text(&panel.group.title, 100)
}

/// The text of the reaction failures of ensurePanelReactions, or None.
pub fn reaction_failures(res: &EnsureResult) -> Option<String> {
    if res.failed.is_empty() {
        return None;
    }
    Some(res.failed.iter().take(5).map(|f| format!("I couldn't add the {} reaction: {}.", f.emoji, f.reason)).collect::<Vec<_>>().join(" "))
}

fn color_of(settings: Option<&Settings>) -> Option<i64> {
    settings.and_then(|s| s.embed_color).map(i64::from)
}

/// Posts the panel as a new message in `channel` and points the panel at it (the shared part of create and repost).
async fn post_new(app: &App, guild_id: &str, panel: &Panel, channel: &GuildChannel, settings: Option<&Settings>) -> anyhow::Result<(MessageInfo, Vec<String>)> {
    require_usable(app, guild_id, channel, &needs_for(panel))?;
    let ops = discord_ops(app);
    let channel_id = channel.id.to_string();
    let body = rr::build_panel_payload(&panel.view(), color_of(settings)).to_api_body();
    let sent = match ops.send_message(guild_id, &channel_id, &body).await {
        Ok(m) => m,
        Err(e) => return Err(task_error(format!("I couldn't post in #{}: {}.", channel.name, rr::describe_discord_error(e.code)))),
    };
    let saved = match rr::move_panel(app.db(), guild_id, &panel.group.id, &channel_id, &sent.id, false).await {
        Ok(n) => Some(n),
        Err(err) => {
            bot_util::log_error!("dashboard rr.publish failed to save the panel", bot_util::logger::LogArg::error(&err));
            None
        }
    };
    if saved.unwrap_or(0) == 0 {
        mark_bot_deletion(app, &[sent.id.as_str()], Some("reaction-roles:rollback"), None);
        let _ = ops.delete_message(&sent.channel_id, &sent.id).await;
        return Err(task_error(if saved.is_some() {
            "That panel was deleted while it was being posted."
        } else {
            "I could not save the new message, so I removed it. Please try again."
        }));
    }
    rr::invalidate_panel_index(app, guild_id).await;

    let mut warnings = Vec::new();
    if panel.layout() == "REACTIONS"
        && let Some(failed) = reaction_failures(&rr::ensure_panel_reactions(app, &sent, &index_items(&panel.items)).await)
    {
        warnings.push(failed);
    }
    if panel.kind() == "UNIQUE" && panel.layout() == "REACTIONS" && lacks_manage_messages(app, guild_id, &channel_id) {
        warnings.push("I don't have Manage Messages there, so I can't remove a member's old reaction when they pick another role.".into());
    }
    Ok((sent, warnings))
}

fn done(text: &str, warnings: Vec<String>, data: Value) -> TaskOutcome {
    let message = std::iter::once(text.to_string()).chain(warnings).collect::<Vec<_>>().join(" ");
    TaskOutcome::new(message, data)
}

/// `emojisToRemove(group, payload)`: the payload's list, minus any emoji a role on the panel still uses.
pub fn emojis_to_remove(panel: &Panel, payload: &Value) -> Vec<String> {
    let empty = Vec::new();
    let list = payload.get("removedEmojis").and_then(Value::as_array).unwrap_or(&empty);
    let used: Vec<String> = panel.items.iter().filter_map(|i| emoji_key_of_stored(i.emoji.as_deref())).collect();
    let mut out: Vec<String> = Vec::new();
    for emoji in list.iter().take(MAX_REMOVED_EMOJIS) {
        let Some(emoji) = emoji.as_str().filter(|e| util::len16(e) <= 100) else { continue };
        if let Some(key) = emoji_key_of_stored(Some(emoji))
            && !used.contains(&key)
            && !out.iter().any(|o| o == emoji)
        {
            out.push(emoji.to_string());
        }
    }
    out
}

/// `rr.publish`.
pub async fn publish(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    publish_inner(&ctx.app, &ctx.guild_id, &ctx.actor, &ctx.payload).await
}

async fn publish_inner(app: &App, guild_id: &str, actor: &Actor, payload: &Value) -> anyhow::Result<TaskOutcome> {
    let settings = authorize(app, guild_id, actor).await?;
    let mode = payload.get("mode").and_then(Value::as_str);
    if mode != Some("post") && mode != Some("sync") {
        return Err(task_error("That request is not valid."));
    }
    let panel = load_group(app, guild_id, payload).await?;

    // A panel that was never posted is posted now (both modes: saving again retries a failed post).
    if panel.group.message_id == UNPOSTED {
        let channel = post_channel(app, guild_id, &panel.group.channel_id, true).await?;
        let (sent, warnings) = post_new(app, guild_id, &panel, &channel, settings.as_ref()).await?;
        return Ok(done(
            &format!("Posted \"{}\" in #{}.", title(&panel), channel.name),
            warnings,
            json!({ "groupId": panel.group.id, "channelId": channel.id.to_string(), "messageId": sent.id, "url": sent.url() }),
        ));
    }

    // Bring the posted message up to date (add / remove / edit). The index goes first, so our own reaction hooks never
    // see a half-updated panel.
    rr::invalidate_panel_index(app, guild_id).await;
    if !panel.group.attached {
        let sync = rr::sync_panel(app, guild_id, PanelArg::Row(Box::new(panel.clone())), color_of(settings.as_ref())).await;
        if sync.status == "missing" {
            return Err(task_error("The panel message was deleted in Discord. Use \"Repost\" to post it again."));
        }
        if sync.status == "failed" {
            return Err(task_error(format!("I couldn't update the panel message: {}.", sync.reason.unwrap_or_default())));
        }
    }
    let mut notes = Vec::new();
    if panel.layout() == "REACTIONS" {
        match rr::fetch_panel(app, guild_id, &panel).await {
            Fetched::Ok { message, .. } => {
                for emoji in emojis_to_remove(&panel, payload) {
                    rr::remove_panel_reaction(app, guild_id, &message, &emoji).await; // cosmetic
                }
                if let Some(failed) = reaction_failures(&rr::ensure_panel_reactions(app, &message, &index_items(&panel.items)).await) {
                    return Err(task_error(failed));
                }
            }
            Fetched::Missing { .. } => return Err(task_error("The panel message was deleted in Discord. Use \"Repost\" to post it again.")),
            Fetched::Failed { reason, .. } => {
                if panel.group.attached {
                    return Err(task_error(format!("I couldn't reach the panel message: {reason}.")));
                }
            }
        }
        if panel.kind() == "UNIQUE" && lacks_manage_messages(app, guild_id, &panel.group.channel_id) {
            notes.push("I don't have Manage Messages in that channel, so I can't remove a member's old reaction when they pick another role.".to_string());
        }
    }
    rr::invalidate_panel_index(app, guild_id).await;
    let what = if panel.group.attached { "the reactions on" } else { "the panel message of" };
    Ok(done(&format!("Updated {what} \"{}\".", title(&panel)), notes, json!({ "groupId": panel.group.id })))
}

/// `rr.repost`.
pub async fn repost(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    let settings = authorize(app, guild_id, &ctx.actor).await?;
    let panel = load_group(app, guild_id, payload).await?;
    let channel_id = match payload.get("channelId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if util::is_snowflake(s) => Some(s.clone()),
        _ => return Err(task_error("That channel is not valid.")),
    };

    let target = match &channel_id {
        Some(id) => Some(post_channel(app, guild_id, id, true).await?),
        None => util::find_channel(app, guild_id, &panel.group.channel_id).await,
    };
    let Some(target) = target else { return Err(task_error("The panel's channel no longer exists. Pick a channel to post it in.")) };

    let unposted = panel.group.message_id == UNPOSTED;
    let restore_only = panel.group.attached && channel_id.is_none();
    if restore_only {
        match rr::fetch_panel(app, guild_id, &panel).await {
            Fetched::Ok { message, .. } => {
                let res = rr::ensure_panel_reactions(app, &message, &index_items(&panel.items)).await;
                return Ok(done(
                    "Put the role reactions back on the attached message.",
                    reaction_failures(&res).into_iter().collect(),
                    json!({ "groupId": panel.group.id }),
                ));
            }
            Fetched::Failed { reason, .. } => return Err(task_error(format!("I couldn't reach the attached message: {reason}."))),
            // The attached message is gone: the panel becomes a message the bot posts below.
            Fetched::Missing { .. } => {}
        }
    }

    // Look the old message up before the panel moves: a fetch that finds nothing flags the panel as missing.
    let old_message = if !restore_only && !unposted { Some(rr::fetch_panel(app, guild_id, &panel).await) } else { None };

    let (sent, warnings) = post_new(app, guild_id, &panel, &target, settings.as_ref()).await?;

    if let Some(Fetched::Ok { message: old, .. }) = &old_message
        && old.id != sent.id
    {
        if !panel.group.attached {
            mark_bot_deletion(app, &[old.id.as_str()], Some("reaction-roles:repost"), None);
            let _ = discord_ops(app).delete_message(&old.channel_id, &old.id).await;
        } else {
            rr::remove_own_reactions(app, old, &index_items(&panel.items)).await; // its content is never touched
        }
    }
    let mut all = Vec::new();
    if panel.group.attached {
        all.push("It is a message the bot posted now, so the dashboard can change its text. The original message was left alone.".to_string());
    }
    all.extend(warnings);
    Ok(done(
        &format!("\"{}\" is now in #{}.", title(&panel), target.name),
        all,
        json!({ "groupId": panel.group.id, "channelId": target.id.to_string(), "messageId": sent.id, "url": sent.url() }),
    ))
}

/// `rr.delete`.
pub async fn remove(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    authorize(app, guild_id, &ctx.actor).await?;
    let panel = load_group(app, guild_id, &ctx.payload).await?;
    let name = title(&panel);
    let data = json!({ "groupId": panel.group.id });

    rr::delete_panel(app.db(), guild_id, &panel.group.id).await?;
    rr::invalidate_panel_index(app, guild_id).await;

    if panel.group.message_id == UNPOSTED {
        return Ok(done(&format!("Deleted \"{name}\"."), Vec::new(), data));
    }
    let Fetched::Ok { message, .. } = rr::fetch_panel(app, guild_id, &panel).await else {
        return Ok(done(&format!("Deleted \"{name}\". Its message was already gone, or I couldn't reach it."), Vec::new(), data));
    };
    if panel.group.attached {
        rr::remove_own_reactions(app, &message, &index_items(&panel.items)).await; // never delete or edit an attached message
        return Ok(done(&format!("Deleted \"{name}\". The original message was left alone."), Vec::new(), data));
    }
    mark_bot_deletion(app, &[message.id.as_str()], Some("reaction-roles:delete"), None);
    let deleted = discord_ops(app).delete_message(&message.channel_id, &message.id).await.is_ok();
    Ok(done(
        &if deleted { format!("Deleted \"{name}\", along with its message.") } else { format!("Deleted \"{name}\". I couldn't delete its message, so delete it by hand.") },
        Vec::new(),
        data,
    ))
}

/// `plain(text)`: no bold, no backticks.
fn plain(text: &str) -> String {
    text.replace("**", "").replace('`', "")
}

/// `rr.roleTemplate`.
pub async fn role_template(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    let actor = &ctx.actor;
    authorize(app, guild_id, actor).await?;
    let key = payload.get("template").and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(template) = get_role_template(&key) else { return Err(task_error("That template is not valid.")) };
    let panel = load_group(app, guild_id, payload).await?;
    if !panel.items.is_empty() {
        return Err(task_error("That panel already has roles."));
    }
    if panel.layout() == "REACTIONS" {
        return Err(task_error("Template panels use a dropdown or buttons."));
    }
    let attempt: anyhow::Result<role_template_setup::SetupResult> = async {
        // The channel first, so roles are never created for a panel that cannot be posted.
        let channel = post_channel(app, guild_id, &panel.group.channel_id, true).await?;
        require_usable(app, guild_id, &channel, &needs_for(&panel))?;
        role_template_setup::setup_role_template(app, guild_id, &key, actor.member.as_ref(), &format!("Role template (dashboard, by {})", actor.id)).await
    }
    .await;
    let setup = match attempt {
        Ok(s) => s,
        Err(err) => {
            // Nothing was posted: drop the empty panel here too, in case the dashboard stopped waiting for this task.
            let _ = rr::delete_panel(app.db(), guild_id, &panel.group.id).await;
            rr::invalidate_panel_index(app, guild_id).await;
            if let Some(e) = err.chain().find_map(|e| e.downcast_ref::<RoleTemplateError>()) {
                return Err(task_error(plain(&e.0)));
            }
            return Err(err);
        }
    };
    let mut items = Vec::new();
    for r in &setup.roles {
        let description = if panel.layout() == "SELECT" { r.description.clone() } else { None };
        let item = NewItem { role_id: r.role_id.clone(), label: r.label.clone(), emoji: r.emoji.clone(), style: Some(2), description };
        let added = rr::add_item(app.db(), &panel.group.id, &items, &item).await?;
        items.push(added);
    }
    rr::invalidate_panel_index(app, guild_id).await;
    let posted = publish_inner(app, guild_id, actor, &json!({ "groupId": panel.group.id, "mode": "post" })).await?;
    let extra: String = [role_template_setup::describe_reused(&setup.roles), template.note.map(str::to_string)]
        .into_iter()
        .flatten()
        .filter(|t| !t.is_empty())
        .map(|t| format!(" {}", plain(&t)))
        .collect();
    let message = format!("{} {}{extra}", role_template_setup::describe_setup(setup.created, setup.reused), posted.message);
    Ok(TaskOutcome { message, data: posted.data })
}

/// `tasks` of reactionRoles.js.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![
        ("rr.publish", DashboardTask::new(publish)),
        ("rr.repost", DashboardTask::new(repost)),
        ("rr.delete", DashboardTask::new(remove)),
        ("rr.roleTemplate", DashboardTask::new(role_template)),
    ]
}
