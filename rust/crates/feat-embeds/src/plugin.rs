//! Port of `src/plugins/embeds.js`: the persistent `embed-modal` form handler and the `messageDelete` hook that turns
//! an embed whose published message was deleted back into a draft. Thin wrappers over [`crate::embed_service`].

use std::sync::Arc;

use bot_core::prelude::*;

use crate::embed_service::{handle_embed_modal_submit, is_embed_delete_candidate, on_embed_message_delete};

/// The `messageDelete` hook body (`onEmbedMessageDelete`).
pub async fn on_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    on_embed_message_delete(ctx.app.db(), &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await;
    Ok(())
}

/// The plugin (`src/plugins/embeds.js`).
pub fn plugin() -> Plugin {
    Plugin::new("embeds", Some(ModuleKey::Embeds))
        .hook(
            Hook::<MessageDelete>::new("embeds", on_delete)
                .filter(|ctx| is_embed_delete_candidate(&ctx.app, &ctx.data)) // sync, cheap
                .always() // clear stale messageIds even while the module is off
                .timeout_ms(5_000),
        )
        // Gated by the plugin's module: a form submitted while the module is off is refused.
        .modal("embed-modal", handle_embed_modal_submit)
}
