//! feat-translation: Auto-Translate (`/autotranslate`, `/translate`, the `translation-fixes` hook) and Auto
//! Timestamps (`/autotime`, `/timestamp`, `/timezone`, the `autotime` hook).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/translationService.js` | [`translation_service`] |
//! | `src/services/translateStatsService.js` | [`translate_stats_service`] |
//! | `src/services/userTimezoneService.js` | [`user_timezone_service`] |
//! | `src/plugins/translation-fixes.js` | [`plugins::translation_fixes`] |
//! | `src/plugins/autotime.js` | [`plugins::autotime`] |
//! | `src/commands/admin/autotranslate.js` | [`commands::autotranslate`] |
//! | `src/commands/admin/autotime.js` | [`commands::autotime`] |
//! | `src/commands/utility/translate.js` | [`commands::translate`] |
//! | `src/commands/utility/timestamp.js` | [`commands::timestamp`] |
//! | `src/commands/utility/timezone.js` | [`commands::timezone`] |
//!
//! Other crates (feat-events, the dashboard) call the services directly.

mod intl_data;
pub mod translate_stats_service;
pub mod translation_service;
pub mod user_timezone_service;

pub mod commands {
    pub mod autotime;
    pub mod autotranslate;
    pub mod timestamp;
    pub mod timezone;
    pub mod translate;
}

pub mod plugins {
    pub mod autotime;
    pub mod translation_fixes;
}

/// Registers `/autotranslate`, `/autotime`, `/translate`, `/timestamp`, `/timezone` and the `translation-fixes` and
/// `autotime` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::autotranslate::command());
    reg.command(commands::autotime::command());
    reg.command(commands::translate::command());
    reg.command(commands::timestamp::command());
    reg.command(commands::timezone::command());
    reg.plugin(plugins::translation_fixes::plugin());
    reg.plugin(plugins::autotime::plugin());
}
