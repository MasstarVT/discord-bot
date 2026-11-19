//! feat-automod: the Auto-Moderation and Anti-Raid modules.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/automodService.js` | [`automod_service`] |
//! | `src/services/antiRaidService.js` | [`anti_raid_service`] |
//! | `src/plugins/automod.js` | [`plugins::automod`] |
//! | `src/plugins/antiraid.js` | [`plugins::antiraid`] |
//! | `src/commands/admin/automod.js` | [`commands::automod`] |
//! | `src/commands/admin/antiraid.js` | [`commands::antiraid`] |
//!
//! Other crates (the dashboard, other features) call the services directly. Automod writes the shared
//! `ctx.state.automodAction` ([`bot_core::hooks::AutomodAction`]) that translation and automations read.

pub mod anti_raid_service;
pub mod automod_service;
pub mod commands {
    pub mod antiraid;
    pub mod automod;
}
mod js_re;
pub mod plugins {
    pub mod antiraid;
    pub mod automod;
}

/// The commands this crate registers.
pub const COMMANDS: [&str; 2] = ["automod", "antiraid"];

/// Registers `/automod`, `/antiraid` and the `automod` and `antiraid` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::automod::command());
    reg.command(commands::antiraid::command());
    reg.plugin(plugins::automod::plugin());
    reg.plugin(plugins::antiraid::plugin());
}
