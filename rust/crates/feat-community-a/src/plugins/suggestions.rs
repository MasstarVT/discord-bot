//! Port of `src/plugins/suggestions.js`: the cleanup hooks (a deleted post or suggestion channel), the persistent vote
//! buttons and the submit form. Declaration only: the rules live in [`crate::suggestion_service`].

use bot_core::prelude::*;
use serenity::all::ChannelType;

use crate::suggestion_service::{handle_suggestion_modal, handle_vote_button, on_channel_delete, on_message_delete};

/// `isOwnOrUnknown(ctx)`: suggestion posts are bot-authored; a partial (uncached) message has no author and is looked
/// up by id.
pub fn is_own_or_unknown(app: &App, data: &MessageDelete) -> bool {
    if data.partial {
        return true;
    }
    let bot = crate::io::bot_user_id(app);
    data.message.as_ref().is_some_and(|m| bot.is_some_and(|b| m.author.id == b))
}

/// The suggestion channel (and so every post) is a text or announcement channel; other deletions never reach the
/// database.
pub fn is_relevant_channel(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Text | ChannelType::News)
}

/// The plugin (`id: 'suggestions'`, `module: 'suggestions'`).
pub fn plugin() -> Plugin {
    Plugin::new("suggestions", Some(ModuleKey::Suggestions))
        // `always`: clearing a dead message id or an unset channel is cleanup and must happen while the module is off.
        .hook(
            Hook::<MessageDelete>::new("suggestions", |ctx| async move {
                on_message_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await;
                Ok(())
            })
            .always()
            .filter(|ctx| is_own_or_unknown(&ctx.app, &ctx.data)),
        )
        .hook(
            Hook::<ChannelDelete>::new("suggestions", |ctx| async move {
                on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await;
                Ok(())
            })
            .always()
            .filter(|ctx| is_relevant_channel(ctx.data.channel.kind)),
        )
        // Gated by the foundation wrapper (plugin.module = 'suggestions'): while the module is off the buttons and the
        // form answer "The Suggestions module is turned off on this server."
        .button("suggestion", handle_vote_button)
        .modal("suggestion-modal", handle_suggestion_modal)
}
