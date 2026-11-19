//! Port of `src/plugins/translation-fixes.js`: auto-translate as the last messageCreate hook (after automod and the
//! custom-command hooks, so a message automod deleted never reaches it). The body is
//! [`crate::translation_service::handle_auto_translate`].

use std::sync::Arc;

use bot_core::prelude::*;
use futures::FutureExt;

use crate::translation_service::{AutoCtx, AutoMessage, ReplyError, build_auto_reply_payload, handle_auto_translate, translation_paused};

/// Longer than AUTO_DEADLINE_MS (25 s) plus one provider request and the reply; the default 10 s is too short for 5
/// targets.
pub const HOOK_TIMEOUT_MS: u64 = 45_000;

/// The hook filter: sync and cheap (it runs before settings are loaded, for every guild message).
pub fn filter(app: &App, content: &str) -> bool {
    !content.is_empty() && !translation_paused(app)
}

/// `ctx.state.automodAction?.deleted`, through bot-core's shared accessor (it reads the typed `AutomodAction` and the
/// JSON `{ rule, deleted }` automod stores).
pub fn automod_deleted<E: HookEvent>(ctx: &HookCtx<E>) -> bool {
    ctx.state.automod_action().is_some_and(|a| a.deleted)
}

async fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let message = AutoMessage::of(&ctx.data.message);
    let guild_id = ctx.data.guild_id.to_string();
    let c = ctx.clone();
    let settings = async move { c.get_settings().await }.boxed();
    let auto = AutoCtx { guild_id: &guild_id, message: &message, automod_deleted: automod_deleted(&ctx), settings, check_permissions: true };
    let (app, raw) = (ctx.app.clone(), ctx.data.message.clone());
    handle_auto_translate(&ctx.app, auto, |embed| async move {
        let Some(http) = app.http() else { return Err(ReplyError { code: None, message: "Discord is not connected".into() }) };
        raw.channel_id.send_message(http.as_ref(), build_auto_reply_payload(embed, &raw)).await.map(|_| ()).map_err(|e| ReplyError::of(&e))
    })
    .await;
    Ok(())
}

/// The plugin (`src/plugins/translation-fixes.js`): the runner skips the hook unless `translationEnabled`.
pub fn plugin() -> Plugin {
    Plugin::new("translation-fixes", Some(ModuleKey::Translation)).hook(
        // HOOK_ORDER.messageCreate: last.
        Hook::<MessageCreate>::new("translation-fixes", on_message)
            .filter(|ctx| filter(&ctx.app, &ctx.data.message.content))
            .timeout_ms(HOOK_TIMEOUT_MS),
    )
}
