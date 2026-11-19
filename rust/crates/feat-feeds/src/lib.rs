//! The social feeds (RSS, YouTube, Reddit, Bluesky, podcasts) and the scheduled reminders.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/feedParsers.js` | [`feed_parsers`] (with [`xml`], the fast-xml-parser port, and [`js_date`], V8's `Date.parse`) |
//! | `src/services/feedService.js` | [`feed_service`] |
//! | `src/services/reminderService.js` | [`reminder_service`] |
//! | `src/plugins/feeds.js`, `src/plugins/reminders.js` | [`plugins::feeds`], [`plugins::reminders`] |
//! | `src/commands/admin/feed.js`, `src/commands/admin/reminder.js` | [`commands::feed`], [`commands::reminder`] |
//!
//! Outbound HTTP and Discord REST go through the swappable seams of [`net`] (the JS `__setFetchImpl` test seam).

pub mod commands;
pub mod feed_parsers;
pub mod feed_service;
pub mod net;
pub mod plugins;
pub mod reminder_service;
pub mod js_date;
pub mod xml;

/// Registers this crate's commands and plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::feed::command());
    reg.command(commands::reminder::command());
    reg.plugin(plugins::feeds::plugin());
    reg.plugin(plugins::reminders::plugin());
}
