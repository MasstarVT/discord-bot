//! feat-search: the `search`, `liverole` and `afk` modules (see rust/docs/groups.json).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/searchService.js` | [`search_service`] |
//! | `src/services/afkService.js` | [`afk_service`] |
//! | `src/plugins/liverole.js` | [`plugins::liverole`] |
//! | `src/plugins/afk.js` | [`plugins::afk`] |
//! | `src/commands/search/search.js` | [`commands::search`] |
//! | `src/commands/admin/liverole.js` | [`commands::liverole`] |
//! | `src/commands/community/afk.js` | [`commands::afk`] |
//!
//! [`ops`] is the Discord side (cache reads and REST writes) the AFK and live-role code goes through.

pub mod afk_service;
pub mod commands;
pub mod ops;
pub mod plugins;
pub mod search_service;

/// Registers this crate's commands and plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::search::command());
    reg.command(commands::liverole::command());
    reg.command(commands::afk::command());
    reg.plugin(plugins::liverole::plugin());
    reg.plugin(plugins::afk::plugin());
}
