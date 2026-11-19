//! Port of `src/plugins/applications.js`: the persistent buttons (Apply, Continue, Accept, Deny) and modals (question
//! pages, deny reason), and the cleanup hooks. Declaration only: the logic is in [`crate::application_service`].

use bot_core::prelude::*;

use crate::application_service::{handle_button, handle_modal, on_channel_delete, on_message_delete, on_role_delete};

/// `isOwnOrUnknown(ctx)`: panels and review posts are bot messages; an uncached message has no author and is looked
/// up by id.
pub fn is_own_or_unknown(app: &App, data: &MessageDelete) -> bool {
    if data.partial || data.message.is_none() {
        return true;
    }
    let bot = app.discord().map(|d| d.bot_user_id());
    data.message.as_ref().is_some_and(|m| Some(m.author.id) == bot)
}

/// The plugin (`src/plugins/applications.js`).
pub fn plugin() -> Plugin {
    Plugin::new("applications", Some(ModuleKey::Applications))
        // All three are `always`: forgetting a deleted panel, review post, channel or role is cleanup and must happen
        // while the module is off too.
        .hook(Hook::<MessageDelete>::new("applications", on_message_delete).always().filter(|ctx| is_own_or_unknown(&ctx.app, &ctx.data)))
        .hook(Hook::<ChannelDelete>::new("applications", on_channel_delete).always().filter(|ctx| bot_core::permissions::is_text_based(&ctx.data.channel)))
        .hook(Hook::<RoleDelete>::new("applications", on_role_delete).always())
        // Gated by the module wrapper: while the module is off the buttons and forms answer with the module notice.
        .button("application", handle_button)
        .modal("application-modal", handle_modal)
}
