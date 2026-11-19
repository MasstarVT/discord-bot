//! Port of `src/commands/utility/timezone.js`: `/timezone set | show | clear`, a member's own timezone, saved once
//! for every server. /timestamp reads times in it, ahead of the server's timezone roles and the server timezone.

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Choice, Payload, cached_guild, reply_error, respond_autocomplete, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_util::timezones::{ResolvedTimezone, SearchOptions, describe_local_time_ms, normalize_timezone_roles, parse_zone_input, search_time_zones, zone_input_error, zone_label_ms, zone_source_text};
use serenity::builder::CreateEmbed;

use crate::user_timezone_service::{clear_user_timezone, get_user_timezone, member_role_ids, peek_user_timezone, resolve_member_timezone, set_user_timezone};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("timezone", "Save your timezone so /timestamp reads your times right in every server.")
        .option(
            Opt::sub_command("set", "Save your timezone. It beats timezone roles and the server timezone.").option(
                Opt::string("zone", "Region/City (America/Chicago) or an abbreviation like CST. Type an offset like -6 to search.")
                    .required(true)
                    .max_length(64)
                    .autocomplete(true),
            ),
        )
        .option(Opt::sub_command("show", "Show which timezone /timestamp uses for you, and why."))
        .option(Opt::sub_command("clear", "Forget your saved timezone; your timezone role or the server timezone is used again."))
}

/// The command (core, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), None, execute).permission_level(PermissionLevel::Member).autocomplete(|ctx| async move { autocomplete(ctx).await })
}

async fn reply(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
}

/// "It's **15:12** on Friday, 2026-10-16 there now."
pub fn local_now(zone: &str, now_ms: f64) -> String {
    let lt = describe_local_time_ms(zone, now_ms);
    format!("It's **{}** on {}, {} there now.", lt.time, lt.weekday, lt.date)
}

/// What /timestamp would use for this member, after a change to their own zone.
async fn resolve_for(ctx: &CommandCtx, personal: Option<&str>) -> (Option<Settings>, ResolvedTimezone) {
    let settings = match ctx.guild_id() {
        Some(g) => ctx.app.settings().get_settings_safe(&g.to_string()).await,
        None => None,
    };
    let guild = ctx.guild_id().and_then(|g| cached_guild(&ctx.app, g));
    let resolved = resolve_member_timezone(&member_role_ids(ctx.member()), guild.as_ref(), settings.as_deref(), personal);
    (settings, resolved)
}

fn now_ms(ctx: &CommandCtx) -> f64 {
    ctx.app.clock().now_ms() as f64
}

async fn run_set(ctx: CommandCtx) -> BotResult {
    let raw = ctx.get_string("zone").ok_or_else(|| anyhow::anyhow!("Required option \"zone\" not found."))?;
    let r = match parse_zone_input(&raw) {
        Ok(r) => r,
        Err(e) => {
            let (title, text) = zone_input_error(&raw, e.code.as_str(), e.options.as_deref().unwrap_or(&[]));
            reply_error(&ctx.responder, &title, Some(&text), true).await;
            return Ok(());
        }
    };
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let user_id = ctx.user().id.to_string();
    if let Err(e) = set_user_timezone(&ctx.app, &user_id, &r.zone).await {
        bot_util::log_error!("[timezone] save failed", e.to_string(), format!("userId: {user_id}"));
        reply(&ctx, error_embed("Could Not Save", Some("Your timezone could not be saved. Please try again in a moment."), None)).await;
        return Ok(());
    }
    bot_util::log_info!(format!("[timezone] {user_id} → {}", r.zone));
    let now = now_ms(&ctx);
    let mut lines = vec![
        format!("Your timezone is now **{}**.", zone_label_ms(&r.zone, now)),
        local_now(&r.zone, now),
        String::new(),
        "/timestamp reads your times in it in every server, ahead of timezone roles and the server timezone.".to_string(),
    ];
    if let Some(abbr) = &r.abbreviation {
        lines.push(format!("{abbr} is read as {} time, so it follows daylight saving.", r.zone));
    }
    reply(&ctx, success_embed("Timezone Saved", Some(&lines.join("\n")), None)).await;
    Ok(())
}

async fn run_show(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let personal = get_user_timezone(&ctx.app, &ctx.user().id.to_string(), false).await;
    let (settings, resolved) = resolve_for(&ctx, personal.clone().flatten().as_deref()).await;
    let null = serde_json::Value::Null;
    let links = normalize_timezone_roles(settings.as_ref().and_then(|s| s.timezone_roles.as_ref()).unwrap_or(&null));
    let link = links.iter().find(|l| Some(&l.role_id) == resolved.role_id.as_ref());
    let saved = match &personal {
        None => "could not be loaded right now".to_string(),
        Some(Some(z)) => format!("`{z}`"),
        Some(None) => "none (`/timezone set`)".to_string(),
    };
    let role = match link {
        Some(l) => format!("<@&{}> → `{}`{}", l.role_id, l.zone, if resolved.role_count > 1 { " (your highest of several)" } else { "" }),
        None => "none".to_string(),
    };
    let now = now_ms(&ctx);
    let lines = [
        format!(
            "/timestamp reads your times in **{}**, from {}.",
            zone_label_ms(&resolved.zone, now),
            zone_source_text(resolved.source.as_str(), resolved.role_id.as_deref(), false)
        ),
        local_now(&resolved.zone, now),
        String::new(),
        format!("• Saved timezone: {saved}"),
        format!("• Timezone role: {role}"),
        format!("• Server timezone: `{}`", settings.as_ref().map(|s| s.timezone.clone()).unwrap_or_else(|| "UTC".into())),
    ];
    reply(&ctx, info_embed("Your Timezone", Some(&lines.join("\n")), None)).await;
    Ok(())
}

async fn run_clear(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let user_id = ctx.user().id.to_string();
    let cleared = match clear_user_timezone(&ctx.app, &user_id).await {
        Ok(c) => c,
        Err(e) => {
            bot_util::log_error!("[timezone] clear failed", e.to_string(), format!("userId: {user_id}"));
            reply(&ctx, error_embed("Could Not Clear", Some("Your timezone could not be cleared. Please try again in a moment."), None)).await;
            return Ok(());
        }
    };
    let (_, resolved) = resolve_for(&ctx, None).await;
    let now = format!(
        "/timestamp uses **{}**, from {}.",
        zone_label_ms(&resolved.zone, now_ms(&ctx)),
        zone_source_text(resolved.source.as_str(), resolved.role_id.as_deref(), false)
    );
    let embed = if cleared {
        success_embed("Timezone Cleared", Some(&format!("Your saved timezone is gone. {now}")), None)
    } else {
        info_embed("No Saved Timezone", Some(&format!("You had no saved timezone. {now}")), None)
    };
    reply(&ctx, embed).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("set") => run_set(ctx).await,
        Some("show") => run_show(ctx).await,
        Some("clear") => run_clear(ctx).await,
        _ => Ok(()),
    }
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let mut choices: Vec<Choice> = Vec::new();
    if let Some((name, value)) = ctx.focused()
        && name == "zone"
    {
        let current = peek_user_timezone(&ctx.app, &ctx.interaction.user.id.to_string()).flatten();
        let opts = SearchOptions { current: current.as_deref(), now_ms: ctx.app.clock().now_ms() as f64, limit: 25, abbreviations: true };
        choices = search_time_zones(&value, &opts).into_iter().map(|c| Choice::new(c.name, c.value)).collect();
    }
    respond_autocomplete(&ctx.responder, &choices).await; // clamps to 25 / 100 chars and never fails
    Ok(())
}
