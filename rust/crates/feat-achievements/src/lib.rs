//! feat-achievements: the Achievements and Birthdays modules.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/achievementService.js` | [`achievement_service`] |
//! | `src/services/birthdayService.js` | [`birthday_service`] |
//! | `src/plugins/achievements.js` | [`plugins::achievements`] |
//! | `src/plugins/birthdays.js` | [`plugins::birthdays`] |
//! | `src/commands/admin/achievement-config.js` | [`commands::achievement_config`] |
//! | `src/commands/community/achievements.js` | [`commands::achievements`] |
//! | `src/commands/community/birthday.js` | [`commands::birthday`] |
//!
//! Other crates (the dashboard, other features) call the two services directly.

pub mod achievement_service;
pub mod birthday_service;
pub mod commands {
    pub mod achievement_config;
    pub mod achievements;
    pub mod birthday;
}
pub mod plugins {
    pub mod achievements;
    pub mod birthdays;
}

/// Registers `/achievement-config`, `/achievements`, `/birthday` and the `achievements` and `birthdays` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::achievement_config::command());
    reg.command(commands::achievements::command());
    reg.command(commands::birthday::command());
    reg.plugin(plugins::achievements::plugin());
    reg.plugin(plugins::birthdays::plugin());
    // bot.js shutdown: `flushMessageCounts({ final: true })` (order 10, 3 s), registered at boot so it exists even
    // before the first message.
    reg.on_boot("achievement count flush", |app| {
        achievement_service::ensure_shutdown_flush(app);
        Ok(())
    });
}
