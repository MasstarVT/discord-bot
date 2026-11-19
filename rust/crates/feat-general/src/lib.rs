//! feat-general: the core commands every server has (`/help`, `/info`, `/owner`, `/module`, `/settings`) and the
//! `bot-joins` plugin (who added the bot to each server).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/botJoinService.js` | [`bot_join_service`] |
//! | `src/plugins/bot-joins.js` | [`plugins::bot_joins`] |
//! | `src/commands/utility/help.js` | [`commands::help`] |
//! | `src/commands/utility/info.js` | [`commands::info`] |
//! | `src/commands/utility/owner.js` | [`commands::owner`] |
//! | `src/commands/admin/module.js` | [`commands::module`] |
//! | `src/commands/admin/settings.js` | [`commands::settings`] |
//!
//! Other crates (the dashboard) call [`bot_join_service`] and the pure helpers of the command modules directly.

pub mod commands {
    pub mod help;
    pub mod info;
    pub mod module;
    pub mod owner;
    pub mod settings;
}
pub mod bot_join_service;
pub mod plugins {
    pub mod bot_joins;
}

/// Registers `/help`, `/info`, `/owner`, `/module`, `/settings` and the `bot-joins` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::module::command());
    reg.command(commands::settings::command());
    reg.command(commands::help::command());
    reg.command(commands::info::command());
    reg.command(commands::owner::command());
    reg.plugin(plugins::bot_joins::plugin());
}
