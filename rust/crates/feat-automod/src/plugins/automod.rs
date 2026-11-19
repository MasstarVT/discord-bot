//! Port of `src/plugins/automod.js`: glue only. Two hooks run the rules: messageCreate (first in `HOOK_ORDER`, so a
//! deleted message never reaches logging, custom commands, XP or translation) and messageUpdate (after logging, so
//! the edit is logged before automod acts on it). The runner skips both hooks while `automodEnabled` is false.

use std::sync::Arc;

use bot_core::prelude::*;

use crate::automod_service::{AutomodInput, run_automod};

/// `hasMessage(ctx)`: a guild message by a user that is not a bot.
pub fn has_message(author_bot: bool) -> bool {
    !author_bot
}

async fn on_create(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let input = AutomodInput::from_create(&ctx.app, &ctx.data);
    run_automod(&ctx.app, ctx.as_ref(), settings.as_deref(), &input, false).await;
    Ok(())
}

async fn on_update(ctx: Arc<HookCtx<MessageUpdate>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let input = AutomodInput::from_update(&ctx.app, &ctx.data);
    run_automod(&ctx.app, ctx.as_ref(), settings.as_deref(), &input, true).await;
    Ok(())
}

/// The plugin (`src/plugins/automod.js`).
pub fn plugin() -> Plugin {
    Plugin::new("automod", Some(ModuleKey::Automod))
        .hook(Hook::<MessageCreate>::new("automod", on_create).filter(|ctx| has_message(ctx.data.message.author.bot)).timeout_ms(15_000))
        .hook(Hook::<MessageUpdate>::new("automod", on_update).filter(|ctx| has_message(ctx.data.message.author.bot)).timeout_ms(15_000))
}
