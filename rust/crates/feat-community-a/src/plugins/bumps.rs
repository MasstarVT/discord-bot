//! Port of `src/plugins/bumps.js`: the botMessage hook that sees a bump site's "bump done" answer, the cleanup hooks
//! (a deleted reminder channel or role) and the `bump.remind` job. Declaration only: the rules live in
//! [`crate::bump_service`].

use bot_core::prelude::*;

use crate::bump_service::{channel_delete_filter, handle_remind_job, is_bump_site_message, on_bot_message, on_channel_delete, on_role_delete, role_delete_filter};

/// The plugin (`id: 'bumps'`, `module: 'bumps'`).
pub fn plugin() -> Plugin {
    Plugin::new("bumps", Some(ModuleKey::Bumps))
        // Every bot message in every server reaches botMessage, so the sync filter only lets a known bump site's bot
        // through (before any settings lookup). Gated by the plugin's module: nothing is recorded while it is off.
        .hook(Hook::<BotMessage>::new("bumps", on_bot_message).filter(|ctx| is_bump_site_message(&ctx.data.message.author.id.to_string())))
        // `always`: unsetting a dead channel or role is cleanup and must happen while the module is off as well.
        .hook(
            Hook::<ChannelDelete>::new("bumps", |ctx| async move {
                on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await;
                Ok(())
            })
            .always()
            .filter(|ctx| channel_delete_filter(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string())),
        )
        .hook(
            Hook::<RoleDelete>::new("bumps", |ctx| async move {
                on_role_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string()).await;
                Ok(())
            })
            .always()
            .filter(|ctx| role_delete_filter(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string())),
        )
        // Not module-gated by the scheduler: the handler checks the module itself, so a job due while the module is
        // off just ends.
        .job(JobType::BumpRemind, job_handler(handle_remind_job))
}
