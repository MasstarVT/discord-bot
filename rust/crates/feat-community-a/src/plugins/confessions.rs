//! Port of `src/plugins/confessions.js`: the cleanup hooks (a deleted post, review message or channel), the
//! persistent buttons (the panel's Confess button and the review message's Approve / Reject) and the confession form.
//! Declaration only: the rules live in [`crate::confession_service`].

use bot_core::prelude::*;

use crate::confession_service::{handle_button, handle_confession_modal, on_channel_delete, on_message_delete};
use crate::plugins::suggestions::{is_own_or_unknown, is_relevant_channel};

/// The plugin (`id: 'confessions'`, `module: 'confessions'`).
pub fn plugin() -> Plugin {
    Plugin::new("confessions", Some(ModuleKey::Confessions))
        // `always`: forgetting a dead message id or an unset channel is cleanup and must happen while the module is off.
        .hook(
            Hook::<MessageDelete>::new("confessions", |ctx| async move {
                on_message_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await;
                Ok(())
            })
            .always()
            .filter(|ctx| is_own_or_unknown(&ctx.app, &ctx.data)),
        )
        .hook(
            Hook::<ChannelDelete>::new("confessions", |ctx| async move {
                on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await;
                Ok(())
            })
            .always()
            .filter(|ctx| is_relevant_channel(ctx.data.channel.kind)),
        )
        // Gated by the foundation wrapper (plugin.module = 'confessions'): while the module is off the buttons and the
        // form answer "The Confessions module is turned off on this server." and nothing is recorded.
        .button("confession", handle_button)
        .modal("confession-modal", handle_confession_modal)
}
