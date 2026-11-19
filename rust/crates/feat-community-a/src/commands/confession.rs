//! Port of `src/commands/community/confession.js`: `/confession submit | panel | delete | block | unblock | reveal |
//! setup`.
//!
//! Members run `submit` (text option, or an empty one for a form); moderators run delete | block | unblock; admins run
//! `reveal`, `panel` and `setup`. The command is not gated, so admins can set the channels up before turning the
//! module on; `submit` checks the module itself. EVERY reply is ephemeral: nothing public may hint who used the
//! command. The rules live in [`crate::confession_service`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{CHANNEL_POST_NEEDS, Payload, cached_channel, cached_guild, check_channel_usable, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_core::privacy_guards::everyone_can_view;
use serde_json::Value;
use serenity::all::ChannelId;
use serenity::builder::CreateEmbed;

use crate::confession_service::{
    AttachmentInfo, BlockResult, Deleted, Revealed, build_panel_payload, delete_confession, describe_author, err_tag, open_confession_form, reveal_confession,
    set_blocked, submit_from_interaction,
};
use crate::io::io;

const TEXT_CHANNELS: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn number_option() -> Opt {
    Opt::integer("number", "The confession number").required(true).min_value(1.0).max_value(2_000_000_000.0)
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("confession", "Post an anonymous confession, or manage confessions.")
        .option(
            Opt::sub_command("submit", "Post a confession. Leave the text empty to write it in a form.")
                .option(Opt::string("text", "Your confession").max_length(bot_config::limits::CONFESSION_LENGTH as u16))
                .option(Opt::attachment("image", "An image to show with it (if the server allows it)")),
        )
        .option(
            Opt::sub_command("panel", "Post a message with a Confess button.")
                .option(Opt::channel("channel", "Where to post it (default: here)").channel_types(&TEXT_CHANNELS)),
        )
        .option(Opt::sub_command("delete", "Delete a confession and its post.").option(number_option()))
        .option(Opt::sub_command("block", "Stop the author of a confession from confessing. You will not see who.").option(number_option()))
        .option(Opt::sub_command("unblock", "Let the author of a confession confess again.").option(number_option()))
        .option(Opt::sub_command("reveal", "Show who wrote a confession. Written to the moderation log.").option(number_option()))
        .option(
            Opt::sub_command("setup", "Set the confession and review channels (no options: show them).")
                .option(Opt::channel("channel", "Where confessions are posted").channel_types(&TEXT_CHANNELS))
                .option(Opt::channel("review_channel", "Where moderators approve them first (keep it private)").channel_types(&TEXT_CHANNELS))
                .option(Opt::boolean("no_review", "Stop reviewing: post confessions at once")),
        )
}

/// The command (`module 'confessions'`, `gated = false`, MEMBER; staff levels per subcommand).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Confessions), execute)
        .gated(false)
        .permission_level(PermissionLevel::Member)
        .subcommand_permission("panel", PermissionLevel::Admin)
        .subcommand_permission("delete", PermissionLevel::Moderator)
        .subcommand_permission("block", PermissionLevel::Moderator)
        .subcommand_permission("unblock", PermissionLevel::Moderator)
        .subcommand_permission("reveal", PermissionLevel::Admin)
        .subcommand_permission("setup", PermissionLevel::Admin)
}

fn not_found(n: i64) -> CreateEmbed {
    error_embed("Confession Not Found", Some(&format!("This server has no confession **#{n}**.")), None)
}

fn try_again() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn number(ctx: &CommandCtx) -> anyhow::Result<i64> {
    ctx.get_integer("number").ok_or_else(|| anyhow::anyhow!("Required option \"number\" not found."))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    if sub == "submit" {
        return submit(ctx).await;
    }
    // Everything else is staff: answered ephemerally.
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    match sub.as_str() {
        "delete" => remove(ctx).await,
        "block" => block(ctx, true).await,
        "unblock" => block(ctx, false).await,
        "reveal" => reveal(ctx).await,
        "panel" => panel(ctx).await,
        "setup" => setup(ctx).await,
        _ => Ok(()),
    }
}

// ── submit ────────────────────────────────────────────────────────────────────

async fn submit(ctx: CommandCtx) -> BotResult {
    let text = ctx.get_string("text");
    let attachment = ctx.get_attachment("image");
    if text.is_none() {
        // The form has no image field: an image needs the text option next to it.
        if attachment.is_some() {
            let e = error_embed("Add The Text", Some("Type your confession in the `text` option when you attach an image. The form cannot take images."), None);
            safe_reply(&ctx.responder, Payload::embed(e).ephemeral(true)).await;
            return Ok(());
        }
        open_confession_form(&ctx).await;
        return Ok(());
    }
    // Ephemeral: the invocation must not show in the channel.
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    submit_from_interaction(&ctx, text.as_deref(), attachment.as_ref().map(AttachmentInfo::from), settings).await;
    Ok(())
}

// ── delete | block | unblock | reveal ─────────────────────────────────────────

async fn remove(ctx: CommandCtx) -> BotResult {
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let n = number(&ctx)?;
    let settings = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    let out = match delete_confession(&ctx.app, guild_id, n, settings.as_deref()).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {guild_id}: delete of #{n} failed ({})", err_tag(&e)));
            return finish(&ctx, error_embed("Could Not Delete", Some("The confession could not be deleted. Please try again in a moment."), None)).await;
        }
    };
    let embed = match out {
        Deleted::NotFound => not_found(n),
        Deleted::Already => error_embed("Already Deleted", Some(&format!("Confession **#{n}** was already deleted.")), None),
        Deleted::Changed => error_embed("Try Again", Some(&format!("Confession **#{n}** was just changed by someone else. Run the command again.")), None),
        Deleted::Message(reason) => error_embed("Could Not Delete", Some(&format!("I couldn't delete the post: {reason} Nothing was changed.")), None),
        Deleted::Ok(_) => {
            bot_util::log_info!(format!("[confessions] {guild_id}: #{n} deleted by {}", ctx.user().id));
            success_embed("Confession Deleted", Some(&format!("Confession **#{n}** was deleted.")), None)
        }
    };
    finish(&ctx, embed).await
}

async fn block(ctx: CommandCtx, blocked: bool) -> BotResult {
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let n = number(&ctx)?;
    let out = match set_blocked(&ctx.app, &g, n, blocked).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {g}: {} of #{n} failed ({})", if blocked { "block" } else { "unblock" }, err_tag(&e)));
            return finish(&ctx, try_again()).await;
        }
    };
    match out {
        BlockResult::NotFound => finish(&ctx, not_found(n)).await,
        BlockResult::Full => {
            let max = bot_config::limits::CONFESSION_BLOCKED_USERS;
            finish(&ctx, error_embed("Blocklist Full", Some(&format!("Up to **{max}** members can be blocked. Unblock someone first.")), None)).await
        }
        BlockResult::Ok => {
            bot_util::log_info!(format!("[confessions] {g}: #{n} {} by {}", if blocked { "blocked" } else { "unblocked" }, ctx.user().id));
            // The same words whatever the state was before, and never a name.
            let embed = if blocked {
                success_embed("Author Blocked", Some(&format!("The author of confession **#{n}** can no longer post confessions. You do not see who they are.")), None)
            } else {
                success_embed("Author Unblocked", Some(&format!("The author of confession **#{n}** can post confessions again.")), None)
            };
            finish(&ctx, embed).await
        }
    }
}

/// The reveal reply's lines.
pub fn reveal_lines(number: i64, author: &crate::confession_service::AuthorView) -> Vec<String> {
    let mut lines = vec![
        format!("Confession **#{number}** was written by <@{}>{}.", author.id, author.name.as_deref().map(|n| format!(" (**{n}**)")).unwrap_or_default()),
        format!("User ID: `{}`", author.id),
    ];
    if !author.in_server {
        lines.push("They are no longer in this server.".into());
    }
    lines.push("This lookup was written to the moderation log.".into());
    lines
}

async fn reveal(ctx: CommandCtx) -> BotResult {
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let n = number(&ctx)?;
    let out = match reveal_confession(&ctx.app, guild_id, n, &ctx.user().id.to_string()).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {guild_id}: reveal of #{n} failed ({})", err_tag(&e)));
            return finish(&ctx, error_embed("Could Not Reveal", Some("Something went wrong, so nothing was revealed. Please try again in a moment."), None)).await;
        }
    };
    let c = match out {
        Revealed::NotFound => return finish(&ctx, not_found(n)).await,
        Revealed::NoLog(reason) => {
            let why = format!("{reason} Set one with `/modconfig logchannel` (and let me post there) first.");
            return finish(&ctx, error_embed("Not Revealed", Some(&format!("Every reveal is written to the moderation log first, so nothing was revealed. {why}")), None)).await;
        }
        Revealed::LogFailed(reason) => {
            let why = format!("I couldn't write the log entry: {reason}");
            return finish(&ctx, error_embed("Not Revealed", Some(&format!("Every reveal is written to the moderation log first, so nothing was revealed. {why}")), None)).await;
        }
        Revealed::Ok(c) => c,
    };
    let author = describe_author(&ctx.app, guild_id, &c.author_id).await;
    finish(&ctx, success_embed("Author Revealed", Some(&reveal_lines(n, &author).join("\n")), None)).await
}

// ── panel ─────────────────────────────────────────────────────────────────────

async fn panel(ctx: CommandCtx) -> BotResult {
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let channel = ctx.get_channel_id("channel").unwrap_or_else(|| ctx.channel_id());
    let usable = check_channel_usable(&ctx.app, guild_id, Some(channel), &CHANNEL_POST_NEEDS);
    if !usable.ok {
        let reason = usable.reason.unwrap_or_default();
        return finish(&ctx, error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)).await;
    }
    if let Err(e) = io(&ctx.app).send(channel, build_panel_payload()).await {
        bot_util::log_warn!(format!("[confessions] {guild_id}: could not post the panel: {}", e.message));
        return finish(&ctx, error_embed("Could Not Post", Some(&format!("I couldn't post the panel in <#{channel}>: {}.", e.message)), None)).await;
    }
    let settings = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    let mut notes = vec![format!("The Confess button is in <#{channel}>.")];
    if settings.as_ref().is_none_or(|s| s.confession_channel_id.as_deref().is_none_or(str::is_empty)) {
        notes.push("⚠️ No confession channel is set yet. Pick one with `/confession setup`, or the button will refuse.".into());
    }
    if settings.as_ref().is_some_and(|s| !is_module_enabled(Some(s), Some(ModuleKey::Confessions))) {
        notes.push("⚠️ The Confessions module is off. Turn it on with `/module enable confessions`.".into());
    }
    finish(&ctx, success_embed("Panel Posted", Some(&notes.join("\n")), None)).await
}

// ── setup ─────────────────────────────────────────────────────────────────────

/// `configEmbed(settings, title)`'s description.
pub fn config_lines(settings: &bot_db::GuildSettings) -> String {
    let cd = settings.confession_cooldown_seconds;
    [
        format!(
            "Module: {}",
            if is_module_enabled(Some(settings), Some(ModuleKey::Confessions)) { "✅ On" } else { "❌ Off, turn it on with `/module enable confessions`" }
        ),
        format!("Channel: {}", settings.confession_channel_id.as_deref().filter(|c| !c.is_empty()).map(|c| format!("<#{c}>")).unwrap_or_else(|| "Not set".into())),
        format!(
            "Review channel: {}",
            settings.confession_review_channel_id.as_deref().filter(|c| !c.is_empty()).map(|c| format!("<#{c}>")).unwrap_or_else(|| "None, confessions are posted at once".into())
        ),
        format!("Images: {}", if settings.confession_allow_images { "Allowed" } else { "Not allowed" }),
        format!("Links: {}", if settings.confession_block_links { "Refused" } else { "Allowed" }),
        format!("Cooldown: {}", if cd > 0 { format!("{cd} seconds") } else { "none".into() }),
        "Blocked members: not listed (so nobody can tell who they are or whether two confessions share an author)".into(),
    ]
    .join("\n")
}

fn config_embed(settings: &bot_db::GuildSettings, title: &str) -> CreateEmbed {
    guild_embed(Some(settings), Some(title), Some(&config_lines(settings)), None)
}

async fn setup(ctx: CommandCtx) -> BotResult {
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)).await;
    };
    let picked_channel = ctx.get_channel_id("channel");
    let picked_review = ctx.get_channel_id("review_channel");
    let no_review = ctx.get_boolean("no_review").unwrap_or(false);
    if picked_review.is_some() && no_review {
        return finish(&ctx, error_embed("Pick One", Some("Choose a `review_channel`, or set `no_review`, not both."), None)).await;
    }
    let mut update = SettingsPatch::new();
    let mut resolved_review: Option<ChannelId> = None;
    for (picked, field, is_review) in [(picked_channel, "confessionChannelId", false), (picked_review, "confessionReviewChannelId", true)] {
        let Some(id) = picked else { continue };
        let usable = check_channel_usable(&ctx.app, guild_id, Some(id), &["ViewChannel", "SendMessages", "EmbedLinks", "AttachFiles"]);
        if !usable.ok {
            let reason = usable.reason.unwrap_or_default();
            return finish(&ctx, error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)).await;
        }
        update.insert(field, id.to_string());
        if is_review {
            resolved_review = Some(id);
        }
    }
    if no_review {
        update.insert("confessionReviewChannelId", Value::Null);
    }
    if update.is_empty() {
        return finish(&ctx, config_embed(&settings, "🤫 Confessions")).await;
    }
    let channel_id = picked_channel.map(|c| c.to_string()).or_else(|| settings.confession_channel_id.clone());
    let review_id = if no_review { None } else { picked_review.map(|c| c.to_string()).or_else(|| settings.confession_review_channel_id.clone()) };
    if let (Some(c), Some(r)) = (&channel_id, &review_id)
        && c == r
    {
        return finish(&ctx, error_embed("Same Channel", Some("The review channel has to be a different channel from the one confessions are posted in."), None)).await;
    }
    let keys: Vec<String> = update.iter().map(|(k, _)| k.clone()).collect();
    let saved = match ctx.app.settings().update_settings(&g, update).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[confessions] {g}: setup failed ({})", err_tag(&anyhow::Error::from(e))));
            return finish(&ctx, try_again()).await;
        }
    };
    bot_util::log_info!(format!("[confessions] {g}: setup changed {} by {}", keys.join(", "), ctx.user().id));

    let mut embed = config_embed(&saved, "✅ Confessions Updated");
    let mut warnings = Vec::new();
    if let Some(r) = resolved_review {
        let visible = cached_guild(&ctx.app, guild_id).and_then(|guild| everyone_can_view(&guild, cached_channel(&ctx.app, guild_id, r).as_ref()));
        if visible != Some(false) {
            warnings.push(format!("⚠️ <#{r}> can be seen by @everyone, so members would read confessions before they are approved. Make it private."));
        }
    }
    let has_channel = saved.confession_channel_id.as_deref().is_some_and(|c| !c.is_empty());
    if saved.confession_review_channel_id.as_deref().is_some_and(|c| !c.is_empty()) && !has_channel {
        warnings.push("⚠️ Pick the confession channel too, or approved confessions have nowhere to go.".into());
    }
    if !has_channel {
        warnings.push("⚠️ Pick a channel with the `channel` option, or nobody can confess.".into());
    }
    if !warnings.is_empty() {
        embed = embed.field("Check", warnings.join("\n"), false);
    }
    finish(&ctx, embed).await
}
