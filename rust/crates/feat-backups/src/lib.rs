//! feat-backups: the Server Backups module (`/backup`, the `backups` plugin, the `backup.auto` and `backup.restore`
//! jobs) and the core `/emoji` command.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/utils/guildSnapshot.js` | [`guild_snapshot`] (the engine) and [`live_guild`] (its Discord side) |
//! | `src/services/backupService.js` | [`backup_service`] |
//! | `src/plugins/backups.js` | [`plugin`] |
//! | `src/commands/admin/backup.js` | [`commands::backup`] |
//! | `src/commands/admin/emoji.js` | [`commands::emoji`] |

pub mod commands {
    pub mod backup;
    pub mod emoji;
}
pub mod backup_service;
pub mod guild_snapshot;
pub mod live_guild;
pub mod plugin;

/// Registers `/backup`, `/emoji` and the `backups` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::backup::command());
    reg.command(commands::emoji::command());
    reg.plugin(plugin::plugin());
}
