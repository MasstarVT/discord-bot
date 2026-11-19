//! Port of `src/plugins/boosts.js`: the Server Boosts plugin declaration. Every body lives in
//! [`crate::boost_service`].

use bot_core::prelude::*;
use bot_config::{JobType, ModuleKey};

use crate::boost_service as boosts;

/// The plugin (`src/plugins/boosts.js`).
pub fn plugin() -> Plugin {
    Plugin::new("boosts", Some(ModuleKey::Boosts))
        // Gated by the module. The sync filter lets only a change of the boost state of a cached member through.
        .hook(
            Hook::<GuildMemberUpdate>::new("boosts", boosts::on_member_update)
                .filter(|ctx| boosts::member_update_filter(ctx.data.old_known, ctx.data.old_member.as_ref(), &ctx.data.member)),
        )
        .hook(Hook::<GuildMemberRemove>::new("boosts", boosts::on_member_remove).filter(|ctx| boosts::member_remove_filter(ctx.data.user.bot)))
        // Database only: a guild with booster roles gets its sweep job back after a restart or a re-invite.
        .hook(Hook::<GuildReady>::new("boosts", boosts::on_guild_ready))
        // Cleanup runs even while the module is off.
        .hook(
            Hook::<ChannelDelete>::new("boosts", boosts::on_channel_delete)
                .always()
                .filter(|ctx| boosts::channel_delete_filter(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string())),
        )
        .hook(Hook::<RoleDelete>::new("boosts", boosts::on_role_delete).always())
        // The handler cleans up whether or not the module is on, and ends itself when no booster roles are left.
        .job(JobType::BoostsSweep, job_handler(boosts::sweep_job_handler))
}
