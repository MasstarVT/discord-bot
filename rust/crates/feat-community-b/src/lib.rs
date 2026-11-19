//! feat-community-b: Modmail, Server Boosts and Auto Threads.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/modmailService.js` | [`modmail_service`] |
//! | `src/services/boostService.js` | [`boost_service`] |
//! | `src/services/autoThreadService.js` | [`auto_thread_service`] |
//! | `src/plugins/modmail.js` | [`plugins::modmail`] |
//! | `src/plugins/boosts.js` | [`plugins::boosts`] |
//! | `src/plugins/auto-threads.js` | [`plugins::auto_threads`] |
//! | `src/commands/admin/modmail.js` | [`commands::modmail`] |
//! | `src/commands/community/boost.js` | [`commands::boost`] |
//! | `src/commands/admin/autothread.js` | [`commands::autothread`] |
//!
//! Other crates (the dashboard, other features) call the service modules directly.

pub mod auto_thread_service;
pub mod boost_service;
pub mod commands;
pub mod modmail_service;
pub mod plugins;

/// Registers `/modmail`, `/boost`, `/autothread` and the `modmail`, `boosts` and `auto-threads` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::modmail::command());
    reg.command(commands::boost::command());
    reg.command(commands::autothread::command());
    reg.plugin(plugins::modmail::plugin());
    reg.plugin(plugins::boosts::plugin());
    reg.plugin(plugins::auto_threads::plugin());
}
