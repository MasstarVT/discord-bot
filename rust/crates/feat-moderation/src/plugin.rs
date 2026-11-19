//! Port of `src/plugins/moderation.js`: glue only. The temp-ban / timeout / mute expiry jobs, the ban import and
//! expiry reconciliation on `guildReady`, the mute re-apply on rejoin, and the external ban / unban recorders. The
//! work itself is in [`crate::moderation_service`].

use std::sync::Arc;

use bot_core::prelude::*;

use crate::moderation_service::{handle_external_unban, reapply_mute_on_join, reconcile_expiries, record_external_ban, run_expire_job, run_unban_job, sync_guild_bans};

/// `BANSYNC_TTL`: 6 hours.
pub const BANSYNC_TTL: u64 = 21_600;

/// `mod:bansync:{guildId}`: the ban import ran recently for this guild.
pub fn bansync_key(guild_id: &str) -> String {
    format!("mod:bansync:{guild_id}")
}

/// `banSyncQueue`: ban imports run one at a time across guilds.
#[derive(Default)]
pub struct BanSyncQueue(pub bot_core::sync::KeyedMutex);

/// Whether the ban import should run for this `guildReady` (and marks it): a join always runs; startup and
/// availability run once per 6 hours, failing OPEN when Redis is down.
pub async fn claim_ban_sync(app: &App, guild_id: &str, reason: GuildReadyReason) -> bool {
    let key = bansync_key(guild_id);
    if reason == GuildReadyReason::Join {
        app.cache().set(&key, &"1", BANSYNC_TTL).await;
        true
    } else {
        app.cache().set_nx(&key, &"1", BANSYNC_TTL).await != Some(false)
    }
}

/// The `guildReady` hook: ALWAYS reconciles the expiries (a stated duration must never be exceeded), then, while the
/// module is on, starts the ban import detached (serialised across guilds). Returns the import's task, if started.
pub async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> Option<tokio::task::JoinHandle<()>> {
    let g = ctx.data.guild_id.to_string();
    reconcile_expiries(&ctx.app, &g).await;
    let settings = ctx.get_settings().await;
    if !is_module_enabled(settings.as_deref(), Some(ModuleKey::Moderation)) {
        return None; // the ban import only runs while the module is on
    }
    if !claim_ban_sync(&ctx.app, &g, ctx.data.reason).await {
        return None;
    }
    let (app, gid) = (ctx.app.clone(), ctx.data.guild_id);
    let bot = crate::moderation_service::bot_user_id(&app);
    let queued = app.ext::<BanSyncQueue>().0.run("bansync", move || async move {
        sync_guild_bans(&app, gid, &bot).await;
    });
    Some(tokio::spawn(queued))
}

/// The plugin (`src/plugins/moderation.js`).
pub fn plugin() -> Plugin {
    Plugin::new("moderation", Some(ModuleKey::Moderation))
        .hook(
            Hook::<GuildReady>::new("moderation", |ctx| async move {
                on_guild_ready(ctx).await;
                Ok(())
            })
            .always()
            .timeout_ms(30_000),
        )
        .hook(
            // Gated by the plugin's module; the slot right after `invites` (mute evasion).
            Hook::<GuildMemberAdd>::new("moderation", |ctx| async move {
                let settings = ctx.get_settings().await;
                reapply_mute_on_join(&ctx.app, ctx.data.guild_id, ctx.data.member.user.id, settings.as_deref()).await;
                Ok(())
            })
            .filter(|ctx| !ctx.data.member.user.bot),
        )
        .hook(
            Hook::<GuildBanAdd>::new("moderation", |ctx| async move {
                record_external_ban(&ctx.app, ctx.data.guild_id, &ctx.data.user, ctx.data.reason.clone()).await;
                Ok(())
            })
            .filter(|ctx| ctx.app.discord().is_none_or(|d| d.bot_user_id() != ctx.data.user.id)),
        )
        .hook(
            // `always`: rows are resolved and the temp-ban job cancelled even when the module is off.
            Hook::<GuildBanRemove>::new("moderation", |ctx| async move {
                let settings = ctx.get_settings().await;
                handle_external_unban(&ctx.app, ctx.data.guild_id, &ctx.data.user, settings.as_deref()).await;
                Ok(())
            })
            .always()
            .filter(|_| true),
        )
        .job(JobType::ModUnban, job_handler(|job, ctx| async move { run_unban_job(job, ctx).await }))
        .job(JobType::ModExpire, job_handler(|job, ctx| async move { run_expire_job(job, ctx).await }))
}
