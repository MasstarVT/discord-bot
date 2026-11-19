//! Port of `src/commands/admin/sticky.js`: `/sticky set | remove | list | pause | resume`. An admin config command:
//! replies are public and it keeps working while the Sticky Messages module is off (`gated = false`). The behaviour is
//! [`crate::plugins::sticky`] and [`crate::sticky_service`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Choice, Payload, cached_channel, check_channel_usable, respond_autocomplete, safe_defer, safe_reply};
use bot_core::prelude::*;
use serenity::all::{ChannelId, GuildId, Permissions};
use serenity::builder::{CreateEmbed, CreateEmbedFooter};

use crate::plugins::sticky::{apply_set, build_sticky_modal};
use crate::sticky_service::{
    ResendStatus, SetArgs, count_stickies, drop_channel, embed_part, get_sticky, list_stickies, missing_for, remove_sticky, set_sticky_enabled,
};

const CHANNEL_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn channel_option(text: &str) -> Opt {
    Opt::channel("channel", text).required(true).channel_types(&CHANNEL_TYPES)
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("sticky", "Keep a message at the bottom of a channel.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("set", "Set or replace the sticky message in a channel.")
                .option(channel_option("The channel."))
                .option(Opt::string("text", "The message ({server} works). Leave out to type it in a form.").max_length(2000))
                .option(Opt::string("embed", "A saved embed (/embed) to post with it.").max_length(100).autocomplete(true)),
        )
        .option(Opt::sub_command("remove", "Remove a channel's sticky message.").option(channel_option("The channel.")))
        .option(Opt::sub_command("list", "Show every sticky message."))
        .option(Opt::sub_command("pause", "Stop re-posting a sticky message (the current one is deleted).").option(channel_option("The channel.")))
        .option(Opt::sub_command("resume", "Start re-posting a paused sticky message.").option(channel_option("The channel.")))
}

/// The command (`module 'sticky'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Sticky), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

/// `MSG` of the command.
pub mod msg {
    use super::*;
    pub fn settings_unavailable() -> CreateEmbed {
        error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
    }
    pub fn unavailable() -> CreateEmbed {
        error_embed("Temporarily Unavailable", Some("That took too long. Please try again in a moment."), None)
    }
    pub fn could_not_save() -> CreateEmbed {
        error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
    }
    pub fn cannot_use(reason: &str) -> CreateEmbed {
        error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)
    }
    pub fn limit() -> CreateEmbed {
        error_embed(
            "Limit Reached",
            Some(&format!(
                "A server can have at most **{}** sticky messages. Remove one with `/sticky remove` first.",
                bot_config::limits::STICKY_MESSAGES
            )),
            None,
        )
    }
    pub fn none(channel_id: &str) -> CreateEmbed {
        error_embed("No Sticky Message", Some(&format!("<#{channel_id}> has no sticky message. Create one with `/sticky set`.")), None)
    }
    pub fn no_embed(name: &str) -> CreateEmbed {
        let clean: String = name.chars().filter(|c| *c != '`' && *c != '\\').collect();
        error_embed(
            "Embed Not Found",
            Some(&format!("There is no saved embed named `{}`. Create it with `/embed create`.", bot_util::text::clamp_text(&clean, 40))),
            None,
        )
    }
}

/// `OFF_NOTE`.
pub const OFF_NOTE: &str = "\n\n⚠️ The **Sticky Messages** module is off. Turn it on with `/module enable sticky`.";

/// `finish(interaction, embed)`: the final answer (overwrites the deferred reply); never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/sticky outside a guild"))
}

fn picked_option(ctx: &CommandCtx) -> anyhow::Result<ChannelId> {
    ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))
}

// ── /sticky set ───────────────────────────────────────────────────────────────

/// No text and no embed: the form, prefilled with the current text. The form must be the first (un-deferred) answer.
async fn open_form(ctx: &CommandCtx, g: GuildId, channel: ChannelId) -> BotResult {
    let usable = check_channel_usable(&ctx.app, g, Some(channel), &["ViewChannel", "SendMessages"]);
    if !usable.ok {
        safe_reply(&ctx.responder, Payload::embed(msg::cannot_use(usable.reason.as_deref().unwrap_or_default()))).await;
        return Ok(());
    }
    let (pool, gid, cid) = (ctx.app.db().clone(), g.to_string(), channel.to_string());
    let lookup = async move {
        let existing = get_sticky(&pool, &gid, &cid).await?;
        let count = count_stickies(&pool, &gid).await?;
        Ok::<_, anyhow::Error>((existing, count))
    };
    let (existing, count) = match bot_util::async_util::with_timeout(lookup, ctx.ack_budget_ms(), "sticky set lookup").await {
        Ok(v) => v,
        Err(e) => {
            if !matches!(e, bot_util::async_util::WithTimeoutError::Timeout(_)) {
                bot_util::log_warn!(format!("[sticky] {g}: set lookup failed: {e}"));
            }
            safe_reply(&ctx.responder, Payload::embed(msg::unavailable())).await;
            return Ok(());
        }
    };
    if existing.is_none() && count >= bot_config::limits::STICKY_MESSAGES as i64 {
        safe_reply(&ctx.responder, Payload::embed(msg::limit())).await;
        return Ok(());
    }
    if let Err(e) = ctx.show_modal(build_sticky_modal(&channel.to_string(), existing.as_ref().and_then(|r| r.content.as_deref()))).await {
        bot_util::log_debug!(format!("[sticky] {g}: could not show the form: {e}"));
    }
    Ok(())
}

async fn run_set(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?;
    let option = picked_option(ctx)?;
    // `guild.channels.cache.get(option.id)`: discord.js put the resolved option channel there; fetched (and cached).
    let channel = bot_core::interaction_helpers::resolve_channel(&ctx.app, g, option).await.map(|c| c.id);
    let text = ctx.get_string("text");
    let embed_name = ctx.get_string("embed");
    let Some(channel) = channel else {
        safe_reply(&ctx.responder, Payload::embed(msg::cannot_use("Channel not found."))).await;
        return Ok(());
    };
    if text.is_none() && embed_name.is_none() {
        return open_form(ctx, g, channel).await;
    }

    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    // A slash option cannot hold a line break, so a typed \n becomes one.
    let mut content = text.map(|t| t.replace("\\n", "\n"));
    let mut part = None;
    if let Some(name) = &embed_name {
        let Some(template) = feat_embeds::embed_service::get_saved_embed_template(&ctx.app, &g.to_string(), name).await else {
            return finish(ctx, msg::no_embed(name)).await;
        };
        part = embed_part(&template);
        if content.is_none() {
            content = template["content"].as_str().map(str::to_string);
        }
    }
    let args = SetArgs { guild_id: g.to_string(), channel_id: channel.to_string(), user_id: ctx.user().id.to_string(), content, embed: part, keep_embed: false };
    apply_set(&ctx.app, &ctx.responder, args).await
}

// ── /sticky remove | pause | resume ───────────────────────────────────────────

async fn run_remove(ctx: &CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_of(ctx)?;
    let channel_id = picked_option(ctx)?.to_string();
    let row = match remove_sticky(&ctx.app, &g.to_string(), &channel_id).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[sticky] {g}: remove failed"), bot_core::error::log_arg(&e));
            return finish(ctx, msg::could_not_save()).await;
        }
    };
    if row.is_none() {
        return finish(ctx, msg::none(&channel_id)).await;
    }
    bot_util::log_info!(format!("[sticky] {g}: {channel_id} removed by {}", ctx.user().id));
    finish(ctx, success_embed("Sticky Message Removed", Some(&format!("<#{channel_id}> no longer has a sticky message.")), None)).await
}

/// The note of `/sticky resume` for a re-send status.
pub fn resume_note(status: Option<ResendStatus>) -> &'static str {
    match status {
        Some(ResendStatus::Sent) => "It is posted again and will move back down once chat has been quiet.",
        Some(ResendStatus::Off) => "⚠️ The **Sticky Messages** module is off. Turn it on with `/module enable sticky`.",
        Some(ResendStatus::Perms) => "⚠️ I can't post there right now. Check my permissions in the channel.",
        Some(ResendStatus::Current) => "It is already the last message there.",
        _ => "It will be posted after the next message.",
    }
}

async fn run_toggle(ctx: &CommandCtx, enabled: bool) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_of(ctx)?;
    let channel_id = picked_option(ctx)?.to_string();
    let result = match set_sticky_enabled(&ctx.app, &g.to_string(), &channel_id, enabled).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[sticky] {g}: {} failed", if enabled { "resume" } else { "pause" }), bot_core::error::log_arg(&e));
            return finish(ctx, msg::could_not_save()).await;
        }
    };
    if !result.found {
        return finish(ctx, msg::none(&channel_id)).await;
    }
    bot_util::log_info!(format!("[sticky] {g}: {channel_id} {} by {}", if enabled { "resumed" } else { "paused" }, ctx.user().id));
    if !enabled {
        let note = if result.changed { "The current sticky message was deleted." } else { "It was already paused." };
        return finish(
            ctx,
            success_embed("Sticky Message Paused", Some(&format!("<#{channel_id}> stays quiet until you run `/sticky resume`. {note}")), None),
        )
        .await;
    }
    finish(ctx, success_embed("Sticky Message Resumed", Some(&format!("<#{channel_id}> is sticky again. {}", resume_note(result.status))), None)).await
}

// ── /sticky list ──────────────────────────────────────────────────────────────

/// `preview(content)`: a one-line, at most 50 character preview without backticks and backslashes.
pub fn preview(content: Option<&str>) -> String {
    let clean: String = content.unwrap_or("").chars().filter(|c| *c != '`' && *c != '\\').collect();
    let mut out = String::new();
    let mut space = false;
    for c in clean.chars() {
        if bot_util::js::is_js_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    bot_util::text::clamp_text(bot_util::js::trim(&out), 50)
}

async fn run_list(ctx: &CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_of(ctx)?;
    let gid = g.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&gid).await else {
        return finish(ctx, msg::settings_unavailable()).await;
    };
    let channels: Option<std::collections::HashSet<String>> =
        bot_core::interaction_helpers::with_cached_guild(&ctx.app, g, |gd| gd.channels.keys().map(ToString::to_string).collect());
    let listed: anyhow::Result<Vec<bot_db::StickyMessage>> = async {
        let mut rows = list_stickies(ctx.app.db(), &gid).await?;
        // A channel deleted while the bot was offline left its row behind: drop it (the cache is complete once the guild is).
        if let Some(ch) = channels.as_ref().filter(|c| !c.is_empty()) {
            let orphans: Vec<String> = rows.iter().filter(|r| !ch.contains(&r.channel_id)).map(|r| r.channel_id.clone()).collect();
            for o in &orphans {
                drop_channel(&ctx.app, &gid, o).await?;
            }
            rows.retain(|r| !orphans.contains(&r.channel_id));
        }
        Ok(rows)
    }
    .await;
    let rows = match listed {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!(format!("[sticky] {gid}: list failed: {e}"));
            return finish(ctx, msg::could_not_save()).await;
        }
    };
    let enabled = is_module_enabled(Some(&settings), Some(ModuleKey::Sticky));
    if rows.is_empty() {
        let text = format!("No sticky messages yet. Create one with `/sticky set`.{}", if enabled { "" } else { OFF_NOTE });
        return finish(ctx, guild_embed(Some(&settings), Some("📌 Sticky Messages"), Some(&text), None)).await;
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            let has_embed = r.embed.as_ref().is_some_and(|e| e.get("embed").is_some_and(|x| !x.is_null() && x != &serde_json::Value::Bool(false)));
            let mut bits: Vec<String> = Vec::new();
            if let Some(c) = r.content.as_deref().filter(|c| !c.is_empty()) {
                let p = preview(Some(c));
                bits.push(format!("`{}`", if p.is_empty() { "…".to_string() } else { p }));
            }
            if has_embed {
                bits.push("🖼️ embed".into());
            }
            let cid = r.channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new);
            let exists = cid.is_some_and(|c| cached_channel(&ctx.app, g, c).is_some());
            let missing = match (exists, cid) {
                (true, Some(c)) => missing_for(&ctx.app, g, c, has_embed),
                _ => Vec::new(),
            };
            let problem = if r.enabled && !missing.is_empty() { format!(" ⚠️ I need {}", missing.join(", ")) } else { String::new() };
            let shown = if bits.is_empty() { "(empty)".to_string() } else { bits.join(" + ") };
            format!("{} <#{}> {shown}{problem}", if r.enabled { "✅" } else { "⏸️" }, r.channel_id)
        })
        .collect();
    let head = if enabled { String::new() } else { format!("{}\n\n", bot_util::js::trim(OFF_NOTE)) };
    let body = format!("{head}{}", bot_util::text::join_with_limit(&lines, "\n", 3500, bot_util::text::default_more));
    let embed = guild_embed(Some(&settings), Some("📌 Sticky Messages"), Some(&body), None)
        .footer(CreateEmbedFooter::new(format!("{} of {} used", rows.len(), bot_config::limits::STICKY_MESSAGES)));
    finish(ctx, embed).await
}

// ── Autocomplete and dispatch ─────────────────────────────────────────────────

/// `embed` choices: the guild's saved embeds.
pub async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let Some((name, value)) = ctx.focused() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    if name != "embed" {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let rows = feat_embeds::embed_service::search_saved_embeds(ctx.app.db(), &g, &value, 25).await;
    let choices: Vec<Choice> = rows.into_iter().map(|r| Choice::new(r.name.clone(), r.name)).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("set") => run_set(&ctx).await,
        Some("remove") => run_remove(&ctx).await,
        Some("list") => run_list(&ctx).await,
        Some("pause") => run_toggle(&ctx, false).await,
        Some("resume") => run_toggle(&ctx, true).await,
        _ => Ok(()),
    }
}
