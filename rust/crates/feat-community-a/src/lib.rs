//! feat-community-a: Suggestions, Confessions and Bump Reminders.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/suggestionService.js` | [`suggestion_service`] |
//! | `src/services/confessionService.js` | [`confession_service`] |
//! | `src/services/bumpService.js` | [`bump_service`] |
//! | `src/plugins/suggestions.js` | [`plugins::suggestions`] |
//! | `src/plugins/confessions.js` | [`plugins::confessions`] |
//! | `src/plugins/bumps.js` | [`plugins::bumps`] |
//! | `src/commands/community/suggestion.js` | [`commands::suggestion`] |
//! | `src/commands/community/confession.js` | [`commands::confession`] |
//! | `src/commands/community/bumps.js` | [`commands::bumps`] |
//!
//! Other crates (the dashboard, other features) call the services directly. Discord REST calls go through
//! [`io::CommunityIo`] so tests can stand in for Discord.

pub mod bump_service;
pub mod confession_service;
pub mod io;
pub mod suggestion_service;

pub mod commands {
    pub mod bumps;
    pub mod confession;
    pub mod suggestion;
}

pub mod plugins {
    pub mod bumps;
    pub mod confessions;
    pub mod suggestions;
}

/// Registers `/suggestion`, `/confession`, `/bumps` and the `suggestions`, `confessions` and `bumps` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::suggestion::command());
    reg.command(commands::confession::command());
    reg.command(commands::bumps::command());
    reg.plugin(plugins::suggestions::plugin());
    reg.plugin(plugins::confessions::plugin());
    reg.plugin(plugins::bumps::plugin());
}
