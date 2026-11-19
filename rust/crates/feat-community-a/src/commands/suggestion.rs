//! Port of `src/commands/community/suggestion.js`: `/suggestion submit | approve | deny | consider | implement |
//! delete | setup`.
//!
//! Members run `submit`; moderators review and delete; admins run `setup`. The command is not gated, so admins can set
//! the channel up before turning the module on; `submit` checks the module itself and refuses with the router's
//! wording while it is off. The rules live in [`crate::suggestion_service`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{PermTarget, Payload, cached_channel, check_channel_usable, is_interaction_gone, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_db::SuggestionStatus;
use serenity::all::ChannelId;
use serenity::builder::CreateEmbed;

use crate::io::snowflake;
use crate::suggestion_service::{
    Deleted, DmResult, NOTE_MAX, build_suggestion_modal, cooldown_embed, cooldown_left_ms, delete_suggestion, jump_url, review_suggestion, status_info,
    submit_from_interaction, submit_problem,
};

/// `REVIEWS`: subcommand, status, description.
pub const REVIEWS: [(&str, SuggestionStatus, &str); 4] = [
    ("approve", SuggestionStatus::Approved, "Approve a suggestion (closes voting)."),
    ("deny", SuggestionStatus::Denied, "Deny a suggestion (closes voting)."),
    ("consider", SuggestionStatus::Considered, "Mark a suggestion as under consideration (voting stays open)."),
    ("implement", SuggestionStatus::Implemented, "Mark a suggestion as implemented (closes voting)."),
];

fn number_option() -> Opt {
    Opt::integer("number", "The suggestion number").required(true).min_value(1.0).max_value(2_000_000_000.0)
}

/// The command's data.
pub fn data() -> SlashCommand {
    let mut builder = SlashCommand::new("suggestion", "Post a suggestion for the server to vote on, or review suggestions.").option(
        Opt::sub_command("submit", "Post a suggestion. Leave the text empty to write it in a form.")
            .option(Opt::string("text", "Your suggestion").max_length(bot_config::limits::SUGGESTION_LENGTH as u16))
            .option(Opt::boolean("anonymous", "Hide your name on the post (if the server allows it)")),
    );
    for (name, _, blurb) in REVIEWS {
        builder = builder.option(
            Opt::sub_command(name, blurb)
                .option(number_option())
                .option(Opt::string("note", "A note for the author and the post").max_length(NOTE_MAX as u16)),
        );
    }
    builder.option(Opt::sub_command("delete", "Delete a suggestion and its post.").option(number_option())).option(
        Opt::sub_command("setup", "Set the suggestion channel and options (no options: show them).")
            .option(Opt::channel("channel", "Where suggestions are posted").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
            .option(new_channel_option("channel", "channel"))
            .option(Opt::boolean("threads", "Open a discussion thread on each suggestion"))
            .option(Opt::boolean("anonymous", "Let members post anonymously"))
            .option(Opt::boolean("dm_on_update", "DM the author when staff review their suggestion"))
            .option(Opt::integer("cooldown", "Seconds between suggestions per member (0 = none)").min_value(0.0).max_value(86_400.0)),
    )
}

/// The command (`module 'suggestions'`, `gated = false`, MEMBER; reviews and delete MODERATOR; setup ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Suggestions), execute)
        .gated(false)
        .permission_level(PermissionLevel::Member)
        .subcommand_permission("approve", PermissionLevel::Moderator)
        .subcommand_permission("deny", PermissionLevel::Moderator)
        .subcommand_permission("consider", PermissionLevel::Moderator)
        .subcommand_permission("implement", PermissionLevel::Moderator)
        .subcommand_permission("delete", PermissionLevel::Moderator)
        .subcommand_permission("setup", PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn not_found(n: i64) -> CreateEmbed {
    error_embed("Suggestion Not Found", Some(&format!("This server has no suggestion **#{n}**.")), None)
}

async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    match sub.as_str() {
        "submit" => submit(ctx).await,
        "delete" => remove(ctx).await,
        "setup" => setup(ctx).await,
        s => match REVIEWS.iter().find(|r| r.0 == s) {
            Some(r) => review(ctx, r.1).await,
            None => Ok(()),
        },
    }
}

// ── submit ────────────────────────────────────────────────────────────────────

async fn submit(ctx: CommandCtx) -> BotResult {
    let text = ctx.get_string("text");
    let anonymous = ctx.get_boolean("anonymous").unwrap_or(false);
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();

    if text.is_none() {
        // The form must be the first response: only bounded lookups before it. Without settings in time the form
        // opens anyway and the submit re-checks everything.
        let settings = crate::commands::bumps::settings_in_budget_labeled(&ctx.app, &g, ctx.ack_budget_ms(), "suggestion settings").await;
        let mut problem = settings.as_ref().and_then(|s| submit_problem(s, anonymous));
        if problem.is_none()
            && let Some(s) = settings.clone()
        {
            let budget = ctx.ack_budget_ms();
            if budget > 0 {
                let (app, gid, uid) = (ctx.app.clone(), g.clone(), ctx.user().id.to_string());
                let left = bot_util::async_util::with_timeout(
                    async move { cooldown_left_ms(&app, &gid, &uid, &s).await.map_err(|e| e.to_string()) },
                    budget,
                    "suggestion cooldown",
                )
                .await
                .unwrap_or(0);
                if left > 0 {
                    problem = Some(cooldown_embed(left, ctx.app.clock().now_ms()));
                }
            }
        }
        if let Some(p) = problem {
            safe_reply(&ctx.responder, Payload::embed(p).ephemeral(true)).await;
            return Ok(());
        }
        if let Err(e) = ctx.show_modal(build_suggestion_modal(anonymous)).await
            && !is_interaction_gone(&e)
        {
            bot_util::log_error!("suggestion submit: showModal failed", bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
        }
        return Ok(());
    }

    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    submit_from_interaction(&ctx, text.as_deref(), anonymous, settings).await;
    Ok(())
}

// ── approve | deny | consider | implement ─────────────────────────────────────

/// The staff answer of a review (`lines` of the success embed).
pub fn review_lines(r: &crate::suggestion_service::Reviewed, module_on: bool) -> Vec<String> {
    let s = &r.suggestion;
    let mut lines = vec![format!("Author: <@{}>{}", s.author_id, if s.anonymous { " (anonymous on the post)" } else { "" })];
    if let Some(n) = s.review_note.as_deref().filter(|n| !n.is_empty()) {
        lines.push(format!("Note: {n}"));
    }
    lines.push(if r.edited {
        format!("The post was updated{}.", jump_url(s).map(|u| format!(" ([jump]({u}))")).unwrap_or_default())
    } else {
        "⚠️ I could not update the post: it was deleted or I cannot edit it. The status is saved.".into()
    });
    match r.dm {
        DmResult::Sent => lines.push("The author was sent a DM.".into()),
        DmResult::Failed => lines.push("⚠️ The author could not be reached by DM (their DMs are closed).".into()),
        DmResult::Off => {}
    }
    if !module_on {
        lines.push("⚠️ The Suggestions module is off. Turn it on with `/module enable suggestions`.".into());
    }
    lines
}

async fn review(ctx: CommandCtx, status: SuggestionStatus) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();
    let number = ctx.get_integer("number").ok_or_else(|| anyhow::anyhow!("Required option \"number\" not found."))?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let note = ctx.get_string("note");
    let out = match review_suggestion(&ctx.app, guild_id, number, status, note.as_deref(), &ctx.user().id.to_string(), Some(&settings)).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[suggestions] {g}: review of #{number} failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)).await;
        }
    };
    let Some(out) = out else { return finish(&ctx, not_found(number)).await };
    let info = status_info(out.suggestion.status);
    let lines = review_lines(&out, is_module_enabled(Some(&settings), Some(ModuleKey::Suggestions)));
    finish(&ctx, success_embed(&format!("Suggestion #{number}: {}", info.label), Some(&lines.join("\n")), None)).await
}

// ── delete ────────────────────────────────────────────────────────────────────

async fn remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let number = ctx.get_integer("number").ok_or_else(|| anyhow::anyhow!("Required option \"number\" not found."))?;
    let out = match delete_suggestion(&ctx.app, guild_id, number).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[suggestions] {guild_id}: delete of #{number} failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, error_embed("Could Not Delete", Some("The suggestion could not be deleted. Please try again in a moment."), None)).await;
        }
    };
    match out {
        Deleted::NotFound => finish(&ctx, not_found(number)).await,
        Deleted::Message(reason) => finish(&ctx, error_embed("Could Not Delete", Some(&format!("I couldn't delete the post: {reason} Nothing was changed.")), None)).await,
        Deleted::Ok(_) => {
            bot_util::log_info!(format!("[suggestions] {guild_id}: #{number} deleted by {}", ctx.user().id));
            finish(&ctx, success_embed("Suggestion Deleted", Some(&format!("Suggestion **#{number}** and its votes were deleted.")), None)).await
        }
    }
}

// ── setup ─────────────────────────────────────────────────────────────────────

fn yes_no(v: bool) -> &'static str {
    if v { "Yes" } else { "No" }
}

/// `configEmbed(settings, title)`'s description.
pub fn config_lines(settings: &bot_db::GuildSettings) -> String {
    let cooldown = settings.suggestion_cooldown_seconds;
    [
        format!(
            "Module: {}",
            if is_module_enabled(Some(settings), Some(ModuleKey::Suggestions)) { "✅ On" } else { "❌ Off, turn it on with `/module enable suggestions`" }
        ),
        format!("Channel: {}", settings.suggestion_channel_id.as_deref().filter(|c| !c.is_empty()).map(|c| format!("<#{c}>")).unwrap_or_else(|| "Not set".into())),
        format!("Discussion threads: {}", yes_no(settings.suggestion_threads)),
        format!("Anonymous suggestions: {}", yes_no(settings.suggestion_allow_anonymous)),
        format!("DM the author on review: {}", yes_no(settings.suggestion_dm_on_update)),
        format!("Cooldown: {}", if cooldown > 0 { format!("{cooldown} seconds") } else { "none".into() }),
    ]
    .join("\n")
}

fn config_embed(settings: &bot_db::GuildSettings, title: &str) -> CreateEmbed {
    guild_embed(Some(settings), Some(title), Some(&config_lines(settings)), None)
}

async fn setup(mut ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    if !resolve_new_options(&mut ctx, NewOptions { required: &[], private: &[] }).await {
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let mut update = SettingsPatch::new();
    if let Some(v) = ctx.get_boolean("threads") {
        update.insert("suggestionThreads", v);
    }
    if let Some(v) = ctx.get_boolean("anonymous") {
        update.insert("suggestionAllowAnonymous", v);
    }
    if let Some(v) = ctx.get_boolean("dm_on_update") {
        update.insert("suggestionDmOnUpdate", v);
    }
    if let Some(v) = ctx.get_integer("cooldown") {
        update.insert("suggestionCooldownSeconds", v);
    }
    let mut channel: Option<ChannelId> = None;
    if let Some(picked) = ctx.get_channel_id("channel") {
        let usable = check_channel_usable(&ctx.app, guild_id, Some(picked), &["ViewChannel", "SendMessages", "EmbedLinks"]);
        if !usable.ok {
            let reason = usable.reason.unwrap_or_default();
            return finish(&ctx, error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)).await;
        }
        update.insert("suggestionChannelId", picked.to_string());
        channel = Some(picked);
    }
    if update.is_empty() {
        return finish(&ctx, config_embed(&settings, "💡 Suggestions")).await;
    }
    let keys: Vec<String> = update.iter().map(|(k, _)| k.clone()).collect();
    let saved = match ctx.app.settings().update_settings(&g, update).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[suggestions] {g}: setup failed"), bot_core::error::log_arg(&anyhow::Error::from(e)));
            return finish(&ctx, error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)).await;
        }
    };
    bot_util::log_info!(format!("[suggestions] {g}: setup changed {} by {}", keys.join(", "), ctx.user().id));

    let mut embed = config_embed(&saved, "✅ Suggestions Updated");
    // The thread option needs a permission the post itself does not.
    let target = channel.or_else(|| saved.suggestion_channel_id.as_deref().and_then(snowflake).map(ChannelId::new).filter(|c| cached_channel(&ctx.app, guild_id, *c).is_some()));
    if saved.suggestion_threads
        && let Some(t) = target
    {
        let missing = missing_bot_permissions(&ctx.app, PermTarget::Channel(guild_id, t), &["CreatePublicThreads"]);
        if !missing.is_empty() {
            embed = embed.field("Threads", format!("⚠️ I need **{}** in <#{t}> to open threads.", permission_names(&missing).join(", ")), false);
        }
    }
    if saved.suggestion_channel_id.as_deref().is_none_or(str::is_empty) {
        embed = embed.field("Channel", "⚠️ Pick a channel with the `channel` option, or nobody can post.", false);
    }
    finish(&ctx, embed).await
}
