//! Port of `src/plugins/levels.js`: the levels plugin declaration. Message XP, voice XP and the role cleanup, the
//! voice sweep job and the `xp` rewards provider; the logic is in [`crate::xp_service`].

use std::sync::Arc;

use bot_core::prelude::*;

use crate::xp_service::{
    XpRewardProvider, handle_message_xp, handle_role_delete, handle_voice_state_xp, run_voice_sweep, seed_voice_sessions, voice_change_matters,
};

/// The `voiceStateUpdate` filter: mute, camera and stream toggles change nobody's voice XP, so they are skipped
/// before any settings or Redis work.
pub fn voice_filter(ctx: &HookCtx<VoiceStateUpdate>) -> bool {
    voice_change_matters(ctx.data.change.as_str(), ctx.data.old_state.as_ref(), Some(&ctx.data.new_state))
}

/// The plugin (`src/plugins/levels.js`).
pub fn plugin() -> Plugin {
    Plugin::new("levels", Some(ModuleKey::Leveling))
        // The core already dropped bots, webhooks, DMs and system messages; the filter only needs a resolved member.
        .hook(Hook::<MessageCreate>::new("levels", handle_message_xp).filter(|ctx| ctx.data.member.is_some()))
        .hook(Hook::<VoiceStateUpdate>::new("levels", handle_voice_state_xp).filter(voice_filter))
        // Role cleanup runs even while the module is off, so a deleted role never lingers in rewards, no-XP or boost
        // lists.
        .hook(Hook::<RoleDelete>::new("levels", handle_role_delete).always())
        // Starts voice sessions for members who are already in a channel at startup or when the guild arrives.
        .hook(Hook::<GuildReady>::new("levels", seed_voice_sessions).timeout_ms(30_000))
        .job(JobType::LevelsVoiceSweep, job_handler(|_job: Job, ctx: JobCtx| async move { run_voice_sweep(&ctx.app, &ctx.guild_id).await }))
        .provides(RewardKind::Xp, Arc::new(XpRewardProvider))
}
