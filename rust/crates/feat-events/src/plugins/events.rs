//! Port of `src/plugins/events.js`: the RSVP and Add to calendar buttons, the edit panel and its forms, the
//! `event.remind` / `event.end` / `event.voice` / `event.cleanup` jobs and the hooks that keep events consistent when
//! their post, channel, role or Discord event vanishes, or that clean up an event's channels when the last member
//! leaves. Declaration only: the logic lives in [`crate::event_service`].

use std::sync::Arc;

use bot_core::prelude::*;
use serenity::all::ScheduledEventStatus;

use crate::event_service as svc;

/// `isOwnOrUnknown(ctx)`: event posts are bot-authored; an uncached message has no author and is looked up by id.
pub fn is_own_or_unknown(ctx: &HookCtx<MessageDelete>) -> bool {
    match &ctx.data.message {
        None => true,
        Some(m) => ctx.app.discord().map(|d| d.bot_user_id()).is_some_and(|bot| m.author.id == bot),
    }
}

/// `isCancelled(ctx)`.
pub fn is_cancelled(ctx: &HookCtx<GuildScheduledEventUpdate>) -> bool {
    ctx.data.scheduled_event.status == ScheduledEventStatus::Canceled
}

/// `leftWaitingVoice(ctx)` (sync, no database): someone left a voice channel that is waiting to be cleaned up.
pub fn left_waiting_voice(ctx: &HookCtx<VoiceStateUpdate>) -> bool {
    ctx.data.change != VoiceChange::State
        && ctx.data.old_state.as_ref().and_then(|s| s.channel_id).is_some_and(|c| svc::is_waiting_voice(&ctx.app, &c.to_string()))
}

async fn on_message_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    svc::on_message_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await
}

async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    svc::on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await
}

async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> BotResult {
    svc::on_role_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string()).await
}

async fn on_scheduled_update(ctx: Arc<HookCtx<GuildScheduledEventUpdate>>) -> BotResult {
    svc::on_discord_event_cancelled(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.scheduled_event.id.to_string()).await
}

async fn on_scheduled_delete(ctx: Arc<HookCtx<GuildScheduledEventDelete>>) -> BotResult {
    svc::on_discord_event_cancelled(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.scheduled_event.id.to_string()).await
}

async fn on_voice(ctx: Arc<HookCtx<VoiceStateUpdate>>) -> BotResult {
    let Some(old) = ctx.data.old_state.as_ref().and_then(|s| s.channel_id) else { return Ok(()) };
    svc::on_waiting_voice_left(&ctx.app, ctx.data.guild_id, old).await
}

/// The plugin (`src/plugins/events.js`).
pub fn plugin() -> Plugin {
    Plugin::new("events", Some(ModuleKey::Events))
        // `always`: cleanup must run even while the module is off, so a dead post or channel never keeps an event open.
        .hook(Hook::<MessageDelete>::new("events", on_message_delete).always().filter(is_own_or_unknown))
        .hook(Hook::<ChannelDelete>::new("events", on_channel_delete).always().timeout_ms(30_000))
        .hook(Hook::<RoleDelete>::new("events", on_role_delete).always())
        // Cancelling the Discord event in the server's Events tab deletes it (or sets it to Canceled).
        .hook(Hook::<GuildScheduledEventUpdate>::new("events", on_scheduled_update).always().timeout_ms(30_000).filter(is_cancelled))
        .hook(Hook::<GuildScheduledEventDelete>::new("events", on_scheduled_delete).always().timeout_ms(30_000))
        .hook(Hook::<VoiceStateUpdate>::new("events", on_voice).always().filter(left_waiting_voice))
        // Module gating is applied by the registry (plugin module 'events').
        .button("event", svc::handle_rsvp_button)
        .button("event-edit", svc::handle_edit_button)
        .modal("event-modal", svc::handle_edit_modal)
        .job(JobType::EventRemind, job_handler(svc::handle_remind_job))
        .job(JobType::EventEnd, job_handler(svc::handle_end_job))
        .job(JobType::EventVoice, job_handler(svc::handle_voice_job))
        .job(JobType::EventCleanup, job_handler(svc::handle_cleanup_job))
}
