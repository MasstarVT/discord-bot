//! Port of `src/plugins/reminders.js`: the `channelDelete` cleanup, the `guildReady` self-healing pass, the persistent
//! `reminder-modal` form handler and the `reminder.fire` job handler. The logic lives in
//! [`crate::reminder_service`].

use std::sync::Arc;

use bot_core::custom_embed::{normalize_template, parse_template_modal, validate_template};
use bot_core::interaction_helpers::{Payload, cached_channel, cached_guild, check_channel_usable, reply_error, safe_reply};
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::ChannelId;

use crate::reminder_service::{
    self as svc, REMINDER_CHANNEL_TYPES, ReminderUpdate, Schedule, compute_next_run, default_template, is_empty_template, reminder_warnings, resolve_time_zone,
    summary_lines, sync_reminder_job,
};

/// `handleReminderModal(interaction)`: the submit of the message editor (`reminder-modal:<reminderId>`, the
/// template `tpl-*` fields). Registered without a module (the command works while the module is off), so the
/// permission level is checked here.
pub async fn handle_reminder_modal(ctx: ModalCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let reminder_id = params.first().cloned().filter(|p| !p.is_empty());
    let guild_cached = ctx.guild_id().is_some_and(|g| cached_guild(&ctx.app, g).is_some());
    let (Some(gid), Some(reminder_id), true) = (ctx.guild_id(), reminder_id, guild_cached) else {
        reply_error(&ctx.responder, "Form Expired", Some("This form has expired. Please run the command again."), true).await;
        return Ok(());
    };
    let g = gid.to_string();

    // Everything before the acknowledgement is bounded.
    let mut settings = ctx.app.settings().peek_settings(&g);
    if settings.is_none() {
        let (app, g2) = (ctx.app.clone(), g.clone());
        let fut = async move { Ok::<_, std::convert::Infallible>(app.settings().get_settings_safe(&g2).await) };
        settings = bot_util::async_util::with_timeout(fut, ctx.ack_budget_ms(), "reminder modal settings").await.ok().flatten();
    }

    // The router checks levels for commands, not for components.
    if ctx.permission_level().await < PermissionLevel::Admin {
        reply_error(&ctx.responder, "Insufficient Permissions", Some("You need the Admin level to edit reminders."), true).await;
        return Ok(());
    }

    let (pool, g2, id2) = (ctx.app.db().clone(), g.clone(), reminder_id.clone());
    let found = bot_util::async_util::with_timeout(async move { svc::get_reminder(&pool, &g2, &id2).await }, ctx.ack_budget_ms(), "reminder modal lookup").await;
    let r = match found {
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            reply_error(&ctx.responder, "Temporarily Unavailable", Some("That took too long. Please try again."), true).await;
            return Ok(());
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => return Err(e),
        Ok(None) => {
            reply_error(&ctx.responder, "Reminder Not Found", Some("This reminder no longer exists."), true).await;
            return Ok(());
        }
        Ok(Some(r)) => r,
    };

    // An unreadable colour is the one parse error nothing later would report (the parser keeps the old colour).
    let (template, errors) = parse_template_modal(|id| ctx.value(id), &normalize_template(&r.template), true);
    if let Some(colour) = errors.iter().find(|e| e.starts_with("Colour must")) {
        reply_error(&ctx.responder, "Invalid Message", Some(&format!("{colour}\nNothing was saved.")), true).await;
        return Ok(());
    }
    let tpl = if is_empty_template(&template) { default_template() } else { normalize_template(&template) };
    let v = validate_template(&tpl, true);
    if !v.ok {
        let text = format!("{}\nNothing was saved.", bot_util::text::join_with_limit(&v.errors, "\n", 1000, bot_util::text::default_more));
        reply_error(&ctx.responder, "Invalid Message", Some(&text), true).await;
        return Ok(());
    }
    if tpl["embed"].is_object()
        && let Some(cid) = r.channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
        && cached_channel(&ctx.app, gid, cid).is_some()
        && !check_channel_usable(&ctx.app, gid, Some(cid), &["EmbedLinks"]).ok
    {
        let text = format!("I need **Embed Links** in <#{}> to post an embed. Nothing was saved.", r.channel_id);
        reply_error(&ctx.responder, "Missing Permissions", Some(&text), true).await;
        return Ok(());
    }

    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    if settings.is_none() {
        settings = ctx.app.settings().get_settings_safe(&g).await;
    }
    // The form never changes `enabled` or the schedule.
    let updated = svc::update_reminder(&ctx.app, &r.id, &ReminderUpdate { template: Some(tpl.clone()), ..Default::default() }).await?;
    let tz = resolve_time_zone(settings.as_ref().map(|s| s.timezone.as_str()));
    let mut next = None;
    if updated.enabled {
        next = match sync_reminder_job(&ctx.app, &updated, settings.as_deref(), ctx.app.clock().now()).await {
            Ok(n) => n,
            Err(e) => {
                bot_util::log_error!(format!("reminder modal: scheduling {} failed", updated.id), bot_core::error::log_arg(&e));
                compute_next_run(&Schedule::from(&updated), &tz, ctx.app.clock().now())
            }
        };
    }
    let warnings = reminder_warnings(&ctx.app, &updated, &tpl);
    let lines = summary_lines(&updated, settings.as_deref(), next, &warnings);
    safe_reply(&ctx.responder, Payload::embed(success_embed("Reminder Saved", Some(&clamp_text(&lines.join("\n"), 4096)), None))).await;
    Ok(())
}

/// `channelDelete` hook body: pauses the reminders of the deleted channel.
pub async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    svc::disable_reminders_for_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await?;
    Ok(())
}

/// `guildReady` hook body: an enabled reminder without a live job gets it back.
pub async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    svc::reconcile_guild_reminders(&ctx.app, &ctx.data.guild_id.to_string()).await
}

/// The plugin.
pub fn plugin() -> Plugin {
    Plugin::new("reminders", Some(ModuleKey::Reminders))
        // Cleanup must run even while the module is off.
        .hook(
            Hook::<ChannelDelete>::new("reminders", on_channel_delete).always().filter(|ctx| REMINDER_CHANNEL_TYPES.contains(&u8::from(ctx.data.channel.kind))),
        )
        .hook(Hook::<GuildReady>::new("reminders", on_guild_ready).always().timeout_ms(30_000))
        // `module: null` so the editor works while the module is off (the command is `gated = false`).
        .modal("reminder-modal", handle_reminder_modal)
        .component_module(ModuleRef::Ungated)
        .job(JobType::ReminderFire, job_handler(|job, ctx: JobCtx| async move { svc::fire_reminder(job, ctx.app, ctx.attempt).await }))
}
