//! Port of `src/plugins/custom-commands.js`: prefix invocation (`!name args`) and the trigger (auto-responder) hook.
//!
//! `ctx.state.customCommand` ([`CustomCommandState`]) is set by the first hook whenever the message invoked an
//! existing enabled command (whether or not it then ran); the trigger hook skips such messages, so a message gets at
//! most one answer from this package. Only the first hook calls `ctx.stop()`, and only after deleting the usage.

use std::sync::Arc;

use bot_core::prelude::*;

use crate::tag_io::DiscordIo;
use crate::tag_service::{CUSTOM_COMMAND_STATE, handle_prefix_invocation, handle_triggers, has_trigger_content, is_possible_invocation};

pub use crate::tag_service::CustomCommandState;

/// The `custom-commands` hook body (`handlePrefixInvocation(ctx)`).
pub async fn on_prefix(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let io = DiscordIo::new(&ctx.app, ctx.data.guild_id, None);
    handle_prefix_invocation(&ctx, &io, true).await;
    Ok(())
}

/// The `custom-commands:triggers` hook body (`handleTriggers(ctx)`).
pub async fn on_triggers(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    // Most guilds with the module on have no trigger matching most messages: skip the guild snapshot when the memo
    // already says there are none.
    if crate::tag_service::memoised_trigger_count(&ctx.app, &ctx.data.guild_id.to_string()) == Some(0) {
        return Ok(());
    }
    let io = DiscordIo::new(&ctx.app, ctx.data.guild_id, None);
    handle_triggers(&ctx, &io, true).await;
    Ok(())
}

/// The plugin (`src/plugins/custom-commands.js`): both hooks are skipped by the runner unless
/// `customCommandsEnabled` is true.
pub fn plugin() -> Plugin {
    Plugin::new("custom-commands", Some(ModuleKey::Customcommands))
        .hook(
            // HOOK_ORDER.messageCreate: calls ctx.stop() only after deleting the usage.
            Hook::<MessageCreate>::new("custom-commands", on_prefix)
                .filter(|ctx| is_possible_invocation(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message.content))
                .timeout_ms(15_000), // several REST calls (reply, roles, send, delete)
        )
        .hook(
            // Never calls ctx.stop().
            Hook::<MessageCreate>::new("custom-commands:triggers", on_triggers).filter(|ctx| {
                has_trigger_content(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message.content, ctx.state.contains(CUSTOM_COMMAND_STATE))
            }),
        )
}
