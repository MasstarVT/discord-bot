//! feat-levels: the Levels module (MEE6-style XP, level-up messages, role rewards, voice XP, the rank card).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/xpService.js` | [`xp_service`] |
//! | `src/plugins/levels.js` | [`plugin`] |
//! | `src/commands/admin/levelconfig.js` | [`commands::levelconfig`] |
//! | `src/commands/leveling/rank.js` | [`commands::rank`] |
//! | `src/commands/leveling/leaderboard.js` | [`commands::leaderboard`] |
//!
//! Other crates (the dashboard tasks, giveaways, achievements) call [`xp_service`] directly.

pub mod commands {
    pub mod leaderboard;
    pub mod levelconfig;
    pub mod rank;
}
pub mod plugin;
pub mod xp_service;

/// Registers `/levelconfig`, `/rank`, `/leaderboard` and the `levels` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::levelconfig::command());
    reg.command(commands::rank::command());
    reg.command(commands::leaderboard::command());
    reg.plugin(plugin::plugin());
}
