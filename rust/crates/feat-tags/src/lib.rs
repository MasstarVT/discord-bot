//! feat-tags: custom commands (`/tag`, the MEE6 `!name` commands) and triggers (auto-responders).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/tagService.js` | [`tag_service`] (plus [`tag_io`], the Discord side of its executor) |
//! | `src/plugins/custom-commands.js` | [`plugin`] |
//! | `src/commands/utility/tag.js` | [`commands::tag`] |
//!
//! Other crates (feat-automations, the dashboard) call [`tag_service`] directly.

pub mod commands;
pub mod plugin;
pub mod tag_io;
pub mod tag_service;

/// Registers this crate's commands and plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::tag::command());
    reg.plugin(plugin::plugin());
}
