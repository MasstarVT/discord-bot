//! Port of `src/plugins/feeds.js`: the `guildReady` job repair, the `channelDelete` cleanup and the `feed.poll` job
//! handler. The logic lives in [`crate::feed_service`]; the command is [`crate::commands::feed`]. Feeds use no
//! customIds.

use std::sync::Arc;

use bot_core::prelude::*;

use crate::feed_service::{FEED_CHANNEL_TYPES, disable_feeds_for_channel, ensure_guild_feed_jobs, poll_feed_job};

/// `channelDelete` hook body: disables (keeps) the feeds posting in the deleted channel.
pub async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let mod_log = settings.as_ref().and_then(|s| s.mod_log_channel_id.clone());
    disable_feeds_for_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string(), mod_log.as_deref()).await?;
    Ok(())
}

/// `guildReady` hook body: every enabled feed gets a live poll job.
pub async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    ensure_guild_feed_jobs(&ctx.app, &ctx.data.guild_id.to_string()).await
}

/// The plugin.
pub fn plugin() -> Plugin {
    Plugin::new("feeds", Some(ModuleKey::Feeds))
        // Jobs must exist even while the module is off; the handler skips the work.
        .hook(Hook::<GuildReady>::new("feeds", on_guild_ready).always().timeout_ms(30_000))
        .hook(Hook::<ChannelDelete>::new("feeds", on_channel_delete).always().filter(|ctx| FEED_CHANNEL_TYPES.contains(&u8::from(ctx.data.channel.kind))))
        .job(JobType::FeedPoll, job_handler(|job, ctx: JobCtx| async move { poll_feed_job(job, ctx.app).await }))
}
