//! Port of `src/plugins/temp-voice.js`: the temporary voice channels plugin declaration. Declaration only: every body
//! lives in [`crate::temp_voice_service`].

use std::sync::Arc;

use bot_core::prelude::*;

use crate::temp_voice_service as tv;

fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    tv::on_guild_ready(&ctx.app, ctx.data.guild_id, None);
    async { Ok(()) }
}

fn on_voice_state_update(ctx: Arc<HookCtx<VoiceStateUpdate>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    let old = ctx.data.old_state.as_ref().and_then(|s| s.channel_id);
    let new = ctx.data.new_state.channel_id;
    // Queued synchronously (gateway order); the work runs detached.
    tv::on_voice_state_update(&ctx.app, ctx.data.guild_id, ctx.data.member.clone(), old, new, None);
    async { Ok(()) }
}

fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    tv::on_channel_delete(&ctx.app, ctx.data.guild_id, ctx.data.channel.id);
    async { Ok(()) }
}

/// The `temp-voice` plugin.
pub fn plugin() -> Plugin {
    Plugin::new("temp-voice", Some(ModuleKey::Tempvoice))
        // All three hooks are `always`: emptied channels, deleted channels and reconciliation keep draining while the
        // module is off. onVoiceStateUpdate checks the module itself before it creates a channel from a hub.
        .hook(Hook::<GuildReady>::new("temp-voice", on_guild_ready).always())
        .hook(Hook::<VoiceStateUpdate>::new("temp-voice", on_voice_state_update).always().filter(|ctx| {
            tv::voice_filter(
                &ctx.app,
                ctx.data.guild_id,
                ctx.data.change == VoiceChange::State,
                ctx.data.old_state.as_ref().and_then(|s| s.channel_id),
                ctx.data.new_state.channel_id,
            )
        }))
        .hook(
            Hook::<ChannelDelete>::new("temp-voice", on_channel_delete)
                .always()
                .filter(|ctx| tv::channel_delete_filter(&ctx.app, ctx.data.guild_id, ctx.data.channel.id)),
        )
        // Gated by the module wrapper: while the module is off a panel click answers that the module is off. These
        // prefixes stay registered forever (persistent); a stale channel id answers "Channel Gone".
        .button("voice", tv::handle_button)
        .select("voice-select", tv::handle_select)
        .modal("voice-modal", tv::handle_modal)
        // Never checks the module toggle: keep-alive deletion must run while the module is off as well.
        .job(JobType::TempvoiceCleanup, job_handler(tv::handle_cleanup_job))
}
