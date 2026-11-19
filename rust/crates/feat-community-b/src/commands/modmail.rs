//! Port of `src/commands/admin/modmail.js`: `/modmail setup | reply | close | block | unblock | blocked`. A staff
//! command (Moderator; `setup` needs Admin) that keeps working while the Modmail module is off (`gated = false`), so
//! an admin can set it up first. `reply` talks to a member, so it refuses while the module is off. The conversations
//! themselves are [`crate::plugins::modmail`].

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use bot_config::limits;
use bot_core::adapters;
use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Payload, cached_channel, cached_guild, pingable_role_ids, safe_defer, safe_reply};
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_core::permissions;
use bot_core::prelude::*;
use bot_db::{ModmailStatus, ModmailThread};
use bot_util::text::{clamp_text, join_with_limit};
use serde_json::json;
use serenity::all::{ChannelType, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::modmail_service::{self as svc, RelayMessage, limits as MODMAIL};
use crate::plugins::modmail::{Relay, archive_thread, channel_problems, close_and_notify, close_conversation, relay_to_member};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("modmail", "Members DM the bot to reach staff privately; each conversation is a thread.")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(
            Opt::sub_command("setup", "Pick the staff channel for conversations and turn modmail on.")
                .option(
                    Opt::channel("channel", "A staff-only text channel; conversations open as threads in it.").channel_types(&[channel_type::GUILD_TEXT]).required(true),
                )
                .option(Opt::role("ping_role", "Role pinged when a conversation opens (replaces the current ones).")),
        )
        .option(
            Opt::sub_command("reply", "Reply to the member (use inside a modmail thread).")
                .option(Opt::string("text", "Your reply.").required(true).max_length(MODMAIL::TEXT_MAX as u16))
                .option(Opt::boolean("anonymous", "Show you as \"Staff\" instead of your name.")),
        )
        .option(
            Opt::sub_command("close", "Close this conversation (use inside a modmail thread).")
                .option(Opt::string("reason", "Shown to the member.").max_length(MODMAIL::REASON_MAX as u16)),
        )
        .option(
            Opt::sub_command("block", "Stop a member from opening conversations.")
                .option(Opt::user("user", "The member to block.").required(true))
                .option(Opt::string("reason", "Why (for the log).").max_length(200)),
        )
        .option(Opt::sub_command("unblock", "Let a blocked member open conversations again.").option(Opt::user("user", "The member to unblock.").required(true)))
        .option(Opt::sub_command("blocked", "List the blocked members."))
}

/// The command (`module 'modmail'`, `gated = false`, MODERATOR, `setup` ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Modmail), execute)
        .gated(false)
        .permission_level(PermissionLevel::Moderator)
        .subcommand_permission("setup", PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

fn not_a_thread() -> CreateEmbed {
    error_embed("Not a Modmail Thread", Some("Use this command inside a modmail conversation thread."), None)
}

fn closed() -> CreateEmbed {
    error_embed("Conversation Closed", Some("This conversation is already closed."), None)
}

fn off() -> CreateEmbed {
    warn_embed("Module Disabled", Some("The **Modmail** module is turned off on this server. An admin can enable it with `/module enable modmail`."), None)
}

/// `finish(interaction, embed)`: the final answer; never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/modmail outside a guild"))
}

/// `conversationHere(interaction)`: the OPEN conversation of the thread this command runs in, or the embed.
async fn conversation_here(ctx: &CommandCtx, guild_id: GuildId) -> anyhow::Result<Result<ModmailThread, CreateEmbed>> {
    // `interaction.channel`: discord.js (Partials.Channel) adds the interaction's own channel to the cache when it was
    // not cached (an archived thread), so its type comes from the interaction when the cache has none.
    let in_thread = match cached_channel(&ctx.app, guild_id, ctx.channel_id()) {
        Some(c) => permissions::is_thread(&c),
        None => ctx
            .interaction
            .channel
            .as_ref()
            .is_some_and(|c| matches!(c.kind, ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread)),
    };
    if !in_thread {
        return Ok(Err(not_a_thread()));
    }
    let Some(row) = svc::find_by_thread(ctx.app.db(), &ctx.channel_id().to_string()).await?.filter(|r| r.guild_id == guild_id.to_string()) else {
        return Ok(Err(not_a_thread()));
    };
    if row.status != ModmailStatus::Open {
        return Ok(Err(closed()));
    }
    Ok(Ok(row))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("setup") => run_setup(ctx).await,
        Some("reply") => run_reply(ctx).await,
        Some("close") => run_close(ctx).await,
        Some("block") => run_block(ctx).await,
        Some("unblock") => run_unblock(ctx).await,
        Some("blocked") => run_blocked(ctx).await,
        _ => Ok(()),
    }
}

// ── /modmail setup ────────────────────────────────────────────────────────────

async fn run_setup(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let option = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let Some(channel) = cached_channel(&ctx.app, guild_id, option).filter(|c| c.kind == ChannelType::Text) else {
        return finish(&ctx, error_embed("Cannot Use That Channel", Some("Pick a text channel: conversations open as threads in it."), None)).await;
    };
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let guild = cached_guild(&ctx.app, guild_id);
    let role = ctx.get_role_id("ping_role");
    if let Some(role) = role {
        if role.get() == guild_id.get() {
            return finish(&ctx, error_embed("Cannot Ping That Role", Some("@everyone cannot be the ping role."), None)).await;
        }
        let executor = ctx.member().map(|m| (m.user.id, m.roles.clone()));
        let refused = match &guild {
            Some(guild) => !pingable_role_ids(guild, &channel, executor.as_ref().map(|(id, r)| (*id, r.as_slice())), &[role.to_string()]).refused.is_empty(),
            None => true,
        };
        if refused {
            let text = format!("<@&{role}> is not mentionable and you cannot mention every role. Make it mentionable, or pick another role.");
            return finish(&ctx, error_embed("Cannot Ping That Role", Some(&text), None)).await;
        }
    }
    let ping_role_ids: Vec<String> = match role {
        Some(r) => vec![r.to_string()],
        None => settings.modmail_ping_role_ids.iter().take(limits::MODMAIL_PING_ROLES).cloned().collect(),
    };

    let problems = channel_problems(&ctx.app, guild_id, channel.id, Some(&ping_role_ids));
    if !problems.errors.is_empty() {
        let text = format!("{}\nPick another channel, or fix my permissions there.", problems.errors.join("\n"));
        return finish(&ctx, error_embed("Cannot Use That Channel", Some(&text), None)).await;
    }
    let mut warnings = problems.warnings.clone();
    let bot_refused = match (&guild, ctx.app.discord().map(|d| d.bot_user_id())) {
        (Some(guild), Some(bot)) => {
            let me = permissions::me(guild, bot).map(|m| (bot, m.roles.clone()));
            pingable_role_ids(guild, &channel, me.as_ref().map(|(id, r)| (*id, r.as_slice())), &ping_role_ids).refused
        }
        _ => ping_role_ids.clone(),
    };
    if !bot_refused.is_empty() {
        let list = bot_refused.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ");
        warnings.push(format!("I cannot ping {list}: make the role mentionable or give me **Mention Everyone**."));
    }

    let channel_id = channel.id.to_string();
    let (cid, rid) = (channel_id.clone(), role.map(|r| r.to_string()));
    let saved = async {
        ctx.app.settings().ensure_guild_settings(&g).await?;
        ctx.app
            .settings()
            .update_settings_with(&g, move |_| {
                let mut patch = SettingsPatch::new().set("modmailChannelId", cid).set("modmailEnabled", true);
                if let Some(r) = rid {
                    patch = patch.set("modmailPingRoleIds", json!([r]));
                }
                Ok(patch)
            })
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(e) = saved {
        bot_util::log_error!(format!("[modmail] {g}: setup failed"), bot_core::error::log_arg(&e));
        return finish(&ctx, could_not_save()).await;
    }
    let before = settings.modmail_channel_id.clone().unwrap_or_else(|| "null".into());
    bot_util::log_info!(format!("[modmail] {g}: staff channel {before} → {channel_id} by {}", ctx.user().id));
    finish(&ctx, success_embed("Modmail Set Up", Some(&setup_text(&channel_id, &ping_role_ids, &warnings)), None)).await
}

/// The `/modmail setup` answer.
pub fn setup_text(channel_id: &str, ping_role_ids: &[String], warnings: &[String]) -> String {
    let mut lines = vec![
        format!("Modmail is on. Members can DM me; each conversation opens as a thread in <#{channel_id}>."),
        if ping_role_ids.is_empty() {
            "No role is pinged yet.".to_string()
        } else {
            format!("Pinged on a new conversation: {}", ping_role_ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" "))
        },
        "Keep the channel staff-only; members never need access to it.".to_string(),
    ];
    lines.extend(warnings.iter().map(|w| format!("⚠️ {w}")));
    lines.join("\n")
}

// ── /modmail reply ────────────────────────────────────────────────────────────

async fn run_reply(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Modmail)) {
        return finish(&ctx, off()).await;
    }
    let row = match conversation_here(&ctx, guild_id).await? {
        Ok(r) => r,
        Err(embed) => return finish(&ctx, embed).await,
    };
    let text = ctx.get_string("text").unwrap_or_default();
    // The command can only turn anonymity on: the admin's setting is never bypassed.
    let anonymous = ctx.get_boolean("anonymous") == Some(true) || settings.modmail_anonymous_replies;
    let member = ctx.member().map(|m| (adapters::member_display_name(m), Some(adapters::member_display_avatar_url_with(guild_id, m, adapters::ImageUrlOptions::default()))));
    let icon = ctx.app.discord().and_then(|d| d.cache.guild(guild_id).and_then(|g| adapters::guild_icon_url_default(guild_id, g.icon.as_ref())));
    let author = svc::staff_author(member.as_ref().map(|(n, i)| (n.as_str(), i.clone())), icon, anonymous);
    let embed = match relay_to_member(&ctx.app, &row, guild_id, Some(&settings), &author, &RelayMessage::text(&text)).await {
        Relay::Ok => {
            let footer = if anonymous { "Sent to the member as \"Staff\".".to_string() } else { format!("Sent to the member as {}.", author.name) };
            success_embed("Reply Sent", Some(&text), Some(&footer))
        }
        Relay::DmClosed => error_embed("Not Delivered", Some("The member's DMs are closed, so they did not get that reply. They can still write to me."), None),
        _ => error_embed("Not Delivered", Some("I couldn't deliver that reply. Please try again in a moment."), None),
    };
    finish(&ctx, embed).await
}

// ── /modmail close ────────────────────────────────────────────────────────────

/// The `/modmail close` answer.
pub fn close_text(closer_id: &str, reason: Option<&str>, notified: bool) -> String {
    let mut lines = vec![format!("Closed by <@{closer_id}>.")];
    if let Some(r) = reason.filter(|r| !r.is_empty()) {
        lines.push(format!("**Reason:** {}", clamp_text(r, MODMAIL::REASON_MAX)));
    }
    if !notified {
        lines.push("The member could not be told (their DMs are closed).".into());
    }
    lines.join("\n")
}

async fn run_close(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let row = match conversation_here(&ctx, guild_id).await? {
        Ok(r) => r,
        Err(embed) => return finish(&ctx, embed).await,
    };
    let reason = ctx.get_string("reason");
    let closer = ctx.user().id.to_string();
    let res = close_and_notify(&ctx.app, guild_id, &row, Some(&closer), reason.as_deref(), reason.as_deref(), true).await?;
    if !res.closed {
        return finish(&ctx, closed()).await;
    }
    finish(&ctx, success_embed("Conversation Closed", Some(&close_text(&closer, reason.as_deref(), res.notified)), None)).await?;
    // Locking and archiving come last: a message cannot be posted in an archived thread.
    // `interaction.channel`: the cache, else fetched.
    let thread = bot_core::interaction_helpers::resolve_channel(&ctx.app, guild_id, ctx.channel_id()).await;
    archive_thread(&ctx.app, guild_id, thread.as_ref()).await;
    Ok(())
}

// ── /modmail block | unblock | blocked ────────────────────────────────────────

async fn run_block(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let target = ctx.get_user("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
    let reason = ctx.get_string("reason");
    if target.bot {
        return finish(&ctx, error_embed("Cannot Block", Some("Bots cannot open conversations anyway."), None)).await;
    }
    if target.id == ctx.user().id {
        return finish(&ctx, error_embed("Cannot Block", Some("You cannot block yourself."), None)).await;
    }
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let member = ctx.app.discord().and_then(|d| d.cache.guild(guild_id).and_then(|gd| gd.members.get(&target.id).cloned()));
    if let Some(m) = member {
        let access = MemberAccess::from_cache(ctx.app.discord().map(|d| d.cache.as_ref()), guild_id, &m);
        if resolve_permission_level(Some(&access), Some(&settings), ctx.app.config().bot_owner_id.as_deref()) >= PermissionLevel::Moderator {
            return finish(&ctx, error_embed("Cannot Block", Some("Staff cannot be blocked."), None)).await;
        }
    }

    // 0 added, 1 already, 2 full.
    let outcome = Arc::new(AtomicU8::new(0));
    let (o, tid) = (outcome.clone(), target.id.to_string());
    let saved = ctx
        .app
        .settings()
        .update_settings_with(&g, move |current| {
            let list = current.modmail_blocked_user_ids.clone();
            if list.contains(&tid) {
                o.store(1, Ordering::SeqCst);
                return Ok(SettingsPatch::new());
            }
            if list.len() >= limits::MODMAIL_BLOCKED_USERS {
                o.store(2, Ordering::SeqCst);
                return Ok(SettingsPatch::new());
            }
            let mut next = list;
            next.push(tid);
            Ok(SettingsPatch::new().set("modmailBlockedUserIds", json!(next)))
        })
        .await;
    if let Err(e) = saved {
        bot_util::log_error!(format!("[modmail] {g}: block failed"), bot_core::error::log_arg(&e));
        return finish(&ctx, could_not_save()).await;
    }
    match outcome.load(Ordering::SeqCst) {
        1 => return finish(&ctx, error_embed("Already Blocked", Some(&format!("<@{}> is already blocked.", target.id)), None)).await,
        2 => {
            let text = format!("At most {} members can be blocked. Unblock someone first.", limits::MODMAIL_BLOCKED_USERS);
            return finish(&ctx, error_embed("Block List Full", Some(&text), None)).await;
        }
        _ => {}
    }
    let why = reason.as_deref().map(|r| format!(" ({})", clamp_text(r, 200))).unwrap_or_default();
    bot_util::log_info!(format!("[modmail] {g}: {} blocked by {}{why}", target.id, ctx.user().id));

    // An open conversation ends with the block; the member is only told that it closed.
    let mut was_closed = false;
    let closed_by = ctx.user().id.to_string();
    let res: anyhow::Result<()> = async {
        if let Some(row) = svc::find_open_in_guild(ctx.app.db(), &g, &target.id.to_string()).await? {
            let block_reason = match &reason {
                Some(r) => format!("Blocked: {r}"),
                None => "Blocked".to_string(),
            };
            was_closed = close_conversation(&ctx.app, guild_id, &row, Some(&closed_by), Some(&block_reason), None, Some("Closed because the member was blocked."))
                .await?
                .closed;
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_warn!(format!("[modmail] {g}: could not close the conversation of a blocked member: {e}"));
    }
    let extra = if was_closed { " Their open conversation was closed." } else { "" };
    finish(&ctx, success_embed("Member Blocked", Some(&format!("<@{}> can no longer open modmail conversations.{extra}", target.id)), None)).await
}

async fn run_unblock(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let target = ctx.get_user_id("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?.to_string();
    let removed = Arc::new(Mutex::new(false));
    let (r, tid) = (removed.clone(), target.clone());
    let saved = ctx
        .app
        .settings()
        .update_settings_with(&g, move |current| {
            let list = current.modmail_blocked_user_ids.clone();
            if !list.contains(&tid) {
                return Ok(SettingsPatch::new());
            }
            *r.lock().unwrap_or_else(|e| e.into_inner()) = true;
            Ok(SettingsPatch::new().set("modmailBlockedUserIds", json!(list.into_iter().filter(|id| *id != tid).collect::<Vec<_>>())))
        })
        .await;
    if let Err(e) = saved {
        bot_util::log_error!(format!("[modmail] {g}: unblock failed"), bot_core::error::log_arg(&e));
        return finish(&ctx, could_not_save()).await;
    }
    if !*removed.lock().unwrap_or_else(|e| e.into_inner()) {
        return finish(&ctx, error_embed("Not Blocked", Some(&format!("<@{target}> is not blocked.")), None)).await;
    }
    bot_util::log_info!(format!("[modmail] {g}: {target} unblocked by {}", ctx.user().id));
    finish(&ctx, success_embed("Member Unblocked", Some(&format!("<@{target}> can open modmail conversations again.")), None)).await
}

/// The `/modmail blocked` description and footer.
pub fn blocked_list(ids: &[String]) -> (String, String) {
    let hidden = ids.len().saturating_sub(MODMAIL::BLOCKED_LIST_MAX);
    let mut lines: Vec<String> = ids.iter().take(MODMAIL::BLOCKED_LIST_MAX).map(|id| format!("<@{id}> `{id}`")).collect();
    if hidden > 0 {
        lines.push(format!("… and {hidden} more"));
    }
    let text = if lines.is_empty() { "Nobody is blocked.".to_string() } else { join_with_limit(&lines, "\n", 3800, bot_util::text::default_more) };
    (text, format!("{} of {}", ids.len(), limits::MODMAIL_BLOCKED_USERS))
}

async fn run_blocked(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let (text, footer) = blocked_list(&settings.modmail_blocked_user_ids);
    finish(&ctx, guild_embed(Some(&settings), Some("🚫 Blocked Members"), Some(&text), Some(&footer))).await
}
