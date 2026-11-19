//! Port of `src/plugins/stat-channels.js`: the statistics-channels plugin declaration. Declaration only: every body
//! lives in [`crate::stats_service`].

use std::sync::Arc;

use bot_core::prelude::*;
use chrono::{TimeZone, Utc};

use crate::stats_service as stats;

async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    stats::on_guild_ready(&ctx.app, &ctx.data.guild_id.to_string()).await;
    Ok(())
}

async fn on_member_add(ctx: Arc<HookCtx<GuildMemberAdd>>) -> BotResult {
    stats::on_member_change(&ctx.app, &ctx.data.guild_id.to_string()).await;
    Ok(())
}

async fn on_member_remove(ctx: Arc<HookCtx<GuildMemberRemove>>) -> BotResult {
    stats::on_member_change(&ctx.app, &ctx.data.guild_id.to_string()).await;
    Ok(())
}

async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let settings = if u8::from(ctx.data.channel.kind) == 4 { ctx.get_settings().await } else { None };
    stats::on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), u8::from(ctx.data.channel.kind), &ctx.data.channel.id.to_string(), settings)
        .await;
    Ok(())
}

/// The `stats.refresh` job: the handler reads the module toggle itself (while the module is off it only reschedules,
/// so turning it back on needs no restart).
pub async fn refresh_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    Ok(match stats::refresh_job_handler(&ctx.app, &job.guild_id).await {
        None => JobOutcome::Done,
        Some(ms) => JobOutcome::Reschedule { next_run_at: Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now), payload: None },
    })
}

/// The plugin (`src/plugins/stat-channels.js`).
pub fn plugin() -> Plugin {
    Plugin::new("stat-channels", Some(ModuleKey::Stats))
        // Database only: primes the memo and keeps the recurring job alive.
        .hook(Hook::<GuildReady>::new("stat-channels", on_guild_ready).always())
        // Gated by the `stats` module through the runner; the sync filters keep guilds without member counters cheap.
        .hook(Hook::<GuildMemberAdd>::new("stat-channels", on_member_add).filter(|ctx| stats::member_event_filter(&ctx.app, &ctx.data.guild_id.to_string())))
        .hook(
            Hook::<GuildMemberRemove>::new("stat-channels", on_member_remove)
                .filter(|ctx| stats::member_event_filter(&ctx.app, &ctx.data.guild_id.to_string())),
        )
        // Cleanup must happen even while the module is off.
        .hook(Hook::<ChannelDelete>::new("stat-channels", on_channel_delete).always().filter(|ctx| {
            stats::channel_delete_filter(&ctx.app, &ctx.data.guild_id.to_string(), u8::from(ctx.data.channel.kind), &ctx.data.channel.id.to_string())
        }))
        .job(JobType::StatsRefresh, job_handler(refresh_job))
}
