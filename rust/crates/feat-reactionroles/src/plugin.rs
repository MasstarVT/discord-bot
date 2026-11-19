//! Port of `src/plugins/reaction-roles.js`: the plugin declaration. The hook bodies, filters and the persistent
//! component handlers live in [`crate::reaction_role_service`].
//!
//! Gating:
//! - The reaction hooks are gated by the module through the runner: while Reaction Roles is off, reactions do nothing
//!   (roles are unchanged and the reactions stay).
//! - `messageDelete` and `roleDelete` are `always`: while the module is off, a deleted panel message is still flagged
//!   and a deleted role is still pruned from its panels.
//! - The `rr` and `rr-select` handlers are wrapped with the module gate by the registry, so members get the
//!   "module is turned off" notice and the handlers never run.
//!
//! The reaction hooks return immediately and work in the background, so starboard, achievements and automations
//! (which run after this plugin in the same dispatch) are never held up. `guildReady` (gated) adds a timezone
//! template's new roles to the panels posted from its older version.

use bot_core::prelude::*;

use crate::reaction_role_service as rr;
use crate::role_template_setup::upgrade_timezone_panels;

/// The plugin (`src/plugins/reaction-roles.js`).
pub fn plugin() -> Plugin {
    Plugin::new("reaction-roles", Some(ModuleKey::Reactionroles))
        .hook(
            Hook::<GuildReady>::new("reaction-roles", |ctx| async move {
                upgrade_timezone_panels(&ctx.app, &ctx.data.guild_id.to_string()).await;
                Ok(())
            })
            .timeout_ms(60_000),
        )
        .hook(Hook::<MessageReactionAdd>::new("reaction-roles", rr::on_reaction_add).filter(|ctx| {
            rr::reaction_hook_filter(&ctx.app, Some(&ctx.data.0.guild_id.to_string()), Some(&ctx.data.0.message_id.to_string()))
        }))
        .hook(Hook::<MessageReactionRemove>::new("reaction-roles", rr::on_reaction_remove).filter(|ctx| {
            rr::reaction_hook_filter(&ctx.app, Some(&ctx.data.0.guild_id.to_string()), Some(&ctx.data.0.message_id.to_string()))
        }))
        .hook(Hook::<MessageReactionRemoveAll>::new("reaction-roles", rr::on_reaction_remove_all).filter(|ctx| {
            rr::reaction_hook_filter(&ctx.app, Some(&ctx.data.guild_id.to_string()), Some(&ctx.data.message_id.to_string()))
        }))
        .hook(Hook::<MessageReactionRemoveEmoji>::new("reaction-roles", rr::on_reaction_remove_emoji).filter(|ctx| {
            rr::reaction_hook_filter(&ctx.app, Some(&ctx.data.guild_id.to_string()), Some(&ctx.data.message_id.to_string()))
        }))
        .hook(
            Hook::<MessageDelete>::new("reaction-roles", rr::on_message_delete)
                .filter(|ctx| rr::message_delete_filter(&ctx.app, Some(&ctx.data.guild_id.to_string()), Some(&ctx.data.message_id.to_string())))
                .always(),
        )
        .hook(Hook::<RoleDelete>::new("reaction-roles", rr::on_role_delete).always())
        .button("rr", rr::handle_role_button)
        .select("rr-select", rr::handle_role_select)
}
