//! feat-reactionroles: the Reaction Roles module (`/reactionrole`, role panels with buttons, a dropdown or emoji
//! reactions, and the role templates).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/reactionRoleService.js` | [`reaction_role_service`] |
//! | `src/services/roleTemplateSetup.js` | [`role_template_setup`] |
//! | `src/utils/roleTemplates.js` | [`role_templates`] |
//! | `src/plugins/reaction-roles.js` | [`plugin`] |
//! | `src/commands/admin/reactionrole.js` | [`commands::reactionrole`] |
//!
//! Other crates (the dashboard tasks) call [`reaction_role_service`] and [`role_template_setup`] directly.

pub mod commands {
    pub mod reactionrole;
}
pub mod plugin;
pub mod reaction_role_service;
pub mod role_template_setup;
pub mod role_templates;

/// Registers `/reactionrole` and the `reaction-roles` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::reactionrole::command());
    reg.plugin(plugin::plugin());
}
