//! Port of `src/plugins/invites.js`: the Invite Tracker plugin. It owns the join/leave bookkeeping
//! (`GuildMemberCache`) and publishes the result to later `guildMemberAdd` hooks:
//!
//! * `ctx.state` key `"invite"`: `Option<InviteInfo>` (`None` = unknown, the JS `null`). Absent = the invites module
//!   recorded nothing (bots).
//! * `ctx.state` key `"invitePromise"`: [`InvitePending`], settling with the attribution, or `None` after
//!   [`JOIN_HOOK_WAIT_MS`]. Only the join log waits for it.

use std::sync::Arc;

use bot_core::prelude::*;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

use crate::invite_tracker::{IsEnabled, JOIN_HOOK_WAIT_MS, invite_event_to_state, record_invite_create, record_invite_delete, refresh_snapshot, track_join, track_leave};
use crate::log_events::InviteInfo;

/// The pending attribution of one join (`ctx.state.invitePromise`). Cloning shares it; it never fails.
#[derive(Clone)]
pub struct InvitePending(pub Shared<BoxFuture<'static, Option<InviteInfo>>>);

/// The `guildMemberAdd` hook. It returns at once: `track_join` is queued (synchronously, so the guild queue keeps
/// the gateway order) and the attribution arrives through `"invitePromise"` / `"invite"`.
pub fn on_member_add(ctx: Arc<HookCtx<GuildMemberAdd>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    ctx.state.insert("invite", None::<InviteInfo>);
    if !ctx.data.member.user.bot {
        let c = ctx.clone();
        let is_enabled: IsEnabled = Box::new(move || {
            async move {
                let s = c.get_settings().await;
                is_module_enabled(s.as_deref(), Some(ModuleKey::Invites))
            }
            .boxed()
        });
        let joined = ctx.data.member.joined_at.as_ref().map(|t| t.timestamp_millis());
        // MUST be the first call: it enqueues synchronously.
        let p = track_join(&ctx.app, ctx.data.guild_id, ctx.data.member.user.id, joined, Some(is_enabled));
        let waited: BoxFuture<'static, Option<InviteInfo>> =
            async move { tokio::time::timeout(std::time::Duration::from_millis(JOIN_HOOK_WAIT_MS), p).await.ok().flatten() }.boxed();
        let shared = waited.shared();
        ctx.state.insert("invitePromise", InvitePending(shared.clone()));
        let c = ctx.clone();
        tokio::spawn(async move {
            let invite = shared.await;
            c.state.insert("invite", invite);
        });
    }
    async { Ok(()) }
}

/// The `guildMemberRemove` hook: sets `leftAt` (fire and forget; queued synchronously behind the joins).
pub fn on_member_remove(ctx: Arc<HookCtx<GuildMemberRemove>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    drop(track_leave(&ctx.app, ctx.data.guild_id, ctx.data.user.id));
    async { Ok(()) }
}

/// The plugin (`src/plugins/invites.js`).
pub fn plugin() -> Plugin {
    Plugin::new("invites", Some(ModuleKey::Invites))
        // 1st in guildReady: warm a TRUSTED snapshot before the other per-guild startup work.
        .hook(
            Hook::<GuildReady>::new("invites", |ctx| async move {
                refresh_snapshot(&ctx.app, ctx.data.guild_id).await;
                Ok(())
            })
            .timeout_ms(15_000),
        )
        // 1st in guildMemberAdd; always, so join rows stay correct while the module is off.
        .hook(Hook::<GuildMemberAdd>::new("invites", on_member_add).always())
        // 1st in guildMemberRemove; always: leftAt must be recorded even while the module is off.
        .hook(Hook::<GuildMemberRemove>::new("invites", on_member_remove).always().filter(|ctx| !ctx.data.user.bot))
        // Incremental snapshot edits (no REST). Gated by the module.
        // discord.js emits neither invite event while the invite's channel is uncached.
        .hook(Hook::<InviteCreate>::new("invites", |ctx| async move {
            if !crate::plugins::logging::invite_channel_cached(&ctx.app, ctx.data.guild_id, ctx.data.invite.channel_id) {
                return Ok(());
            }
            let state = invite_event_to_state(&ctx.data.invite);
            record_invite_create(&ctx.app, ctx.data.guild_id, state).await;
            Ok(())
        }))
        .hook(Hook::<InviteDelete>::new("invites", |ctx| async move {
            if !crate::plugins::logging::invite_channel_cached(&ctx.app, ctx.data.guild_id, ctx.data.invite.channel_id) {
                return Ok(());
            }
            record_invite_delete(&ctx.app, ctx.data.guild_id, ctx.data.invite.code.clone()).await;
            Ok(())
        }))
}
