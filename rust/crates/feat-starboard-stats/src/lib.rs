//! feat-starboard-stats: the Starboards module (`/starboard`, reposting popular messages) and the Statistics Channels
//! module (`/stats`, counter channels).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/starboardService.js` | [`starboard_service`] (Discord through [`starboard_io`]) |
//! | `src/services/statsService.js` | [`stats_service`] (Discord through [`stats_io`]) |
//! | `src/plugins/starboard.js` | [`plugins::starboard`] |
//! | `src/plugins/stat-channels.js` | [`plugins::stat_channels`] |
//! | `src/commands/admin/starboard.js` | [`commands::starboard`] |
//! | `src/commands/admin/stats.js` | [`commands::stats`] |
//!
//! Other crates (the dashboard, other features) call the two services directly.

pub mod commands {
    pub mod starboard;
    pub mod stats;
}
pub mod plugins {
    pub mod starboard;
    pub mod stat_channels;
}
pub mod discord_io;
pub mod starboard_io;
pub mod starboard_service;
pub mod stats_io;
pub mod stats_service;

/// Registers `/starboard`, `/stats` and the `starboard` and `stat-channels` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::starboard::command());
    reg.command(commands::stats::command());
    reg.plugin(plugins::starboard::plugin());
    reg.plugin(plugins::stat_channels::plugin());
}
