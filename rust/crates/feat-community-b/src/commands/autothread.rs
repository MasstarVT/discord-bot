//! Port of `src/commands/admin/autothread.js`: `/autothread add | remove | list | publish add | publish remove`. An
//! admin config command: replies are public and it keeps working while the Auto Threads module is off
//! (`gated = false`). The behaviour is [`crate::plugins::auto_threads`]; the rules and the lists are
//! [`crate::auto_thread_service`].

use bot_config::limits;
use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Payload, cached_channel, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::text::{clamp_text, join_with_limit};
use serenity::all::{ChannelId, ChannelType, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::auto_thread_service::{
    AutoThreadError, PUBLISH_NEEDS, ThreadChange, add_publish_channel, archive_label, consts as AUTOTHREAD, missing_names, read_entries, read_publish_ids,
    remove_publish_channel, remove_thread_channel, set_thread_channel, thread_needs, unknown_tokens,
};

const THREAD_CHANNELS: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

/// The command's data.
pub fn data() -> SlashCommand {
    let mut archive = Opt::string("archive", "Archive a thread after this long without messages (default 1 day).");
    for v in AUTOTHREAD::ARCHIVE_CHOICES {
        archive = archive.choice(archive_label(v), v);
    }
    SlashCommand::new("autothread", "Start a thread on every message in a channel, and publish announcement posts.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("add", "Start a thread on every member message in a channel.")
                .option(Opt::channel("channel", "The channel.").required(true).channel_types(&THREAD_CHANNELS))
                .option(
                    Opt::string("name", "Thread name: {user} {content} {channel} {date} (default {content}).")
                        .min_length(1)
                        .max_length(AUTOTHREAD::NAME_MAX as u16),
                )
                .option(archive)
                .option(Opt::boolean("close_button", "Post a Close button in each thread (default off).")),
        )
        .option(
            Opt::sub_command("remove", "Stop starting threads in a channel.")
                .option(Opt::channel("channel", "The channel.").required(true).channel_types(&THREAD_CHANNELS)),
        )
        .option(Opt::sub_command("list", "Show the auto-thread and auto-publish channels."))
        .option(
            Opt::sub_command_group("publish", "Publish announcement posts to the servers that follow them.")
                .option(
                    Opt::sub_command("add", "Publish every new post in an announcement channel.").option(
                        Opt::channel("channel", "The announcement channel.").required(true).channel_types(&[channel_type::GUILD_ANNOUNCEMENT]),
                    ),
                )
                .option(
                    Opt::sub_command("remove", "Stop publishing posts in an announcement channel.").option(
                        Opt::channel("channel", "The announcement channel.").required(true).channel_types(&[channel_type::GUILD_ANNOUNCEMENT]),
                    ),
                ),
        )
}

/// The command (`module 'autothreads'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Autothreads), execute).gated(false).permission_level(PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

fn not_listed(channel_id: &str) -> CreateEmbed {
    error_embed("Not In The List", Some(&format!("<#{channel_id}> is not in that list.")), None)
}

fn bad_channel(what: &str) -> CreateEmbed {
    error_embed("Wrong Channel Type", Some(&format!("Pick {what}.")), None)
}

/// `OFF_NOTE`.
pub const OFF_NOTE: &str = "⚠️ The **Auto Threads** module is off, so nothing happens yet. Turn it on with `/module enable autothreads`.";

async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

/// `notes({ settings, problems })`: what is missing and whether the module is off.
pub fn notes(settings: Option<&GuildSettings>, problems: &[String]) -> String {
    let mut out: Vec<String> = problems.iter().map(|p| format!("⚠️ {p}")).collect();
    if settings.is_some_and(|s| !is_module_enabled(Some(s), Some(ModuleKey::Autothreads))) {
        out.push(OFF_NOTE.into());
    }
    if out.is_empty() { String::new() } else { format!("\n\n{}", out.join("\n")) }
}

/// `shown(text)`: a name as shown in an answer.
pub fn shown(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| *c != '`' && *c != '\\').collect();
    let collapsed = bot_util::jsre::JsRegex::new(r"\s+", "g").replace_all(&cleaned, " ");
    let out = clamp_text(bot_util::js::trim(&collapsed), 60);
    if out.is_empty() { "(empty)".into() } else { out }
}

fn channel_known(app: &App, guild_id: GuildId, channel_id: &str) -> Option<serenity::all::GuildChannel> {
    channel_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c)))
}

/// `threadProblem(channel, entry)`: what the bot lacks for a thread channel, as a sentence (`''` = nothing).
pub fn thread_problem(app: &App, guild_id: GuildId, channel_id: &str, close_button: bool) -> String {
    let Some(ch) = channel_known(app, guild_id, channel_id) else { return "I cannot see that channel.".into() };
    let missing = missing_names(app, guild_id, ch.id, thread_needs(close_button));
    if missing.is_empty() { String::new() } else { format!("I need **{}** in <#{}> to start threads there.", missing.join(", "), ch.id) }
}

/// `publishProblem(channel)`: what the bot lacks to publish in an announcement channel (`''` = nothing).
pub fn publish_problem(app: &App, guild_id: GuildId, channel_id: &str) -> String {
    let Some(ch) = channel_known(app, guild_id, channel_id) else { return "I cannot see that channel.".into() };
    if ch.kind != ChannelType::News {
        return format!("<#{}> is no longer an announcement channel.", ch.id);
    }
    let missing = missing_names(app, guild_id, ch.id, &PUBLISH_NEEDS);
    if missing.is_empty() { String::new() } else { format!("I need **{}** in <#{}> to publish posts.", missing.join(", "), ch.id) }
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    match (group.as_deref(), sub.as_deref()) {
        (Some("publish"), Some("add")) => run_publish_add(ctx).await,
        (Some("publish"), Some("remove")) => run_publish_remove(ctx).await,
        (Some("publish"), _) => Ok(()),
        (_, Some("add")) => run_add(ctx).await,
        (_, Some("remove")) => run_remove(ctx).await,
        (_, Some("list")) => run_list(ctx).await,
        _ => Ok(()),
    }
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/autothread outside a guild"))
}

fn channel_option(ctx: &CommandCtx) -> anyhow::Result<ChannelId> {
    ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))
}

fn autothread_error(e: &anyhow::Error) -> Option<CreateEmbed> {
    e.downcast_ref::<AutoThreadError>().map(|a| error_embed(&a.title, Some(&a.message), None))
}

// ── /autothread add | remove ──────────────────────────────────────────────────

async fn run_add(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let option = channel_option(&ctx)?;
    let Some(channel) = cached_channel(&ctx.app, guild_id, option).filter(|c| matches!(c.kind, ChannelType::Text | ChannelType::News)) else {
        return finish(&ctx, bad_channel("a text or announcement channel")).await;
    };
    let name = ctx.get_string("name").map(|n| bot_util::js::trim(&n).to_string()).filter(|n| !n.is_empty());
    if let Some(n) = &name {
        let unknown = unknown_tokens(n);
        if !unknown.is_empty() {
            let list = unknown.iter().take(3).map(|t| format!("`{}`", shown(t))).collect::<Vec<_>>().join(", ");
            let text = format!("{list} is not a placeholder. Use `{{user}}`, `{{content}}`, `{{channel}}` or `{{date}}`.");
            return finish(&ctx, error_embed("Unknown Placeholder", Some(&text), None)).await;
        }
    }
    let g = guild_id.to_string();
    let change = ThreadChange {
        channel_id: channel.id.to_string(),
        name,
        archive_minutes: ctx.get_string("archive"),
        close_button: ctx.get_boolean("close_button"),
    };
    let result = match set_thread_channel(&ctx.app, &g, change).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(embed) = autothread_error(&e) {
                return finish(&ctx, embed).await;
            }
            bot_util::log_error!(format!("[auto-threads] {g}: add failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    let entry = &result.entry;
    bot_util::log_info!(format!("[auto-threads] {g}: {} {} by {}", channel.id, if result.created { "added" } else { "updated" }, ctx.user().id));
    let lines = [
        format!("Every member message in <#{}> gets its own thread.", channel.id),
        format!(
            "Name: `{}` · archives after {} · close button {}",
            shown(&entry.name),
            archive_label(&entry.archive_minutes),
            if entry.close_button { "on" } else { "off" }
        ),
    ];
    let problem = thread_problem(&ctx.app, guild_id, &channel.id.to_string(), entry.close_button);
    let problems: Vec<String> = [problem].into_iter().filter(|p| !p.is_empty()).collect();
    let note = notes(Some(&result.settings), &problems);
    let title = if result.created { "Auto Thread Added" } else { "Auto Thread Updated" };
    finish(&ctx, success_embed(title, Some(&format!("{}{note}", lines.join("\n"))), None)).await
}

async fn run_remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let channel_id = channel_option(&ctx)?.to_string();
    let found = match remove_thread_channel(&ctx.app, &g, &channel_id).await {
        Ok(f) => f,
        Err(e) => {
            bot_util::log_error!(format!("[auto-threads] {g}: remove failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    if !found {
        return finish(&ctx, not_listed(&channel_id)).await;
    }
    bot_util::log_info!(format!("[auto-threads] {g}: {channel_id} removed by {}", ctx.user().id));
    finish(&ctx, success_embed("Auto Thread Removed", Some(&format!("<#{channel_id}> no longer gets a thread on every message. Existing threads stay.")), None)).await
}

// ── /autothread publish add | remove ──────────────────────────────────────────

async fn run_publish_add(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let option = channel_option(&ctx)?;
    let Some(channel) = cached_channel(&ctx.app, guild_id, option).filter(|c| c.kind == ChannelType::News) else {
        return finish(&ctx, bad_channel("an announcement channel")).await;
    };
    let g = guild_id.to_string();
    let result = match add_publish_channel(&ctx.app, &g, &channel.id.to_string()).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(embed) = autothread_error(&e) {
                return finish(&ctx, embed).await;
            }
            bot_util::log_error!(format!("[auto-threads] {g}: publish add failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    let problem = publish_problem(&ctx.app, guild_id, &channel.id.to_string());
    let problems: Vec<String> = [problem].into_iter().filter(|p| !p.is_empty()).collect();
    let note = notes(Some(&result.settings), &problems);
    if !result.added {
        return finish(&ctx, info_embed("Already Publishing", Some(&format!("New posts in <#{}> are already published.{note}", channel.id)), None)).await;
    }
    bot_util::log_info!(format!("[auto-threads] {g}: publishing in {}, added by {}", channel.id, ctx.user().id));
    let text = format!(
        "New posts in <#{}> (members, bots and webhooks) are published to the servers that follow it. Discord allows {} per channel per hour; the rest stay unpublished.{note}",
        channel.id,
        AUTOTHREAD::PUBLISH_MAX
    );
    finish(&ctx, success_embed("Auto Publish Added", Some(&text), None)).await
}

async fn run_publish_remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let channel_id = channel_option(&ctx)?.to_string();
    let found = match remove_publish_channel(&ctx.app, &g, &channel_id).await {
        Ok(f) => f,
        Err(e) => {
            bot_util::log_error!(format!("[auto-threads] {g}: publish remove failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    if !found {
        return finish(&ctx, not_listed(&channel_id)).await;
    }
    bot_util::log_info!(format!("[auto-threads] {g}: publishing in {channel_id} removed by {}", ctx.user().id));
    finish(&ctx, success_embed("Auto Publish Removed", Some(&format!("New posts in <#{channel_id}> are no longer published automatically.")), None)).await
}

// ── /autothread list ──────────────────────────────────────────────────────────

/// The `/autothread list` description.
pub fn list_text(app: &App, guild_id: GuildId, settings: &GuildSettings) -> String {
    let entries = read_entries(&settings.auto_thread_channels);
    let publish_ids = read_publish_ids(&settings.auto_publish_channel_ids);
    let thread_lines: Vec<String> = entries
        .iter()
        .map(|e| {
            let problem = thread_problem(app, guild_id, &e.channel_id, e.close_button);
            format!(
                "<#{}> · `{}` · {}{}{}",
                e.channel_id,
                shown(&e.name),
                archive_label(&e.archive_minutes),
                if e.close_button { " · 🔒 close button" } else { "" },
                if problem.is_empty() { String::new() } else { format!("\n⚠️ {problem}") }
            )
        })
        .collect();
    let publish_lines: Vec<String> = publish_ids
        .iter()
        .map(|id| {
            let problem = publish_problem(app, guild_id, id);
            format!("<#{id}>{}", if problem.is_empty() { String::new() } else { format!("\n⚠️ {problem}") })
        })
        .collect();
    let mut parts: Vec<String> = Vec::new();
    if !is_module_enabled(Some(settings), Some(ModuleKey::Autothreads)) {
        parts.push(OFF_NOTE.into());
    }
    if thread_lines.is_empty() && publish_lines.is_empty() {
        parts.push("Nothing is set up yet. Add a channel with `/autothread add`, or `/autothread publish add` for an announcement channel.".into());
    } else {
        let or_none = |s: String| if s.is_empty() { "None.".to_string() } else { s };
        parts.push(format!(
            "**🧵 Threads ({}/{})**\n{}",
            entries.len(),
            limits::AUTOTHREAD_CHANNELS,
            or_none(join_with_limit(&thread_lines, "\n", 1700, bot_util::text::default_more))
        ));
        parts.push(format!(
            "**📣 Publishing ({}/{})**\n{}",
            publish_ids.len(),
            limits::AUTOPUBLISH_CHANNELS,
            or_none(join_with_limit(&publish_lines, "\n", 1500, bot_util::text::default_more))
        ));
    }
    parts.join("\n\n")
}

async fn run_list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let text = list_text(&ctx.app, guild_id, &settings);
    finish(&ctx, guild_embed(Some(&settings), Some("🧵 Auto Threads"), Some(&text), None)).await
}
