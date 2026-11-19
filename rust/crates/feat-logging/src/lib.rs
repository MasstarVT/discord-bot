//! feat-logging: audit logs and the invite tracker.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/loggingService.js` | [`logging_service`] |
//! | `src/services/logEvents.js` | [`log_events`] (+ [`adapters`]: serenity models as the builders' views) |
//! | `src/services/inviteTracker.js` | [`invite_tracker`] |
//! | `src/plugins/logging.js` | [`plugins::logging`] |
//! | `src/plugins/invites.js` | [`plugins::invites`] |
//! | `src/commands/admin/logging.js` | [`commands::logging`] |
//! | `src/commands/utility/invites.js` | [`commands::invites`] |

pub mod adapters;
pub mod invite_tracker;
pub mod log_events;
pub mod logging_service;

pub mod plugins {
    pub mod invites;
    pub mod logging;
}

pub mod commands {
    pub mod invites;
    pub mod logging;
}

pub use log_events::InviteInfo;
pub use plugins::invites::InvitePending;

/// Registers this crate's commands and plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::logging::command());
    reg.command(commands::invites::command());
    reg.plugin(plugins::logging::plugin());
    reg.plugin(plugins::invites::plugin());
    // bot.js shutdown: flushLogQueue() (order 20, 4000 ms). Registered at install, not on the first post.
    reg.on_boot("log queue flush", |app| {
        logging_service::ensure_shutdown_flush(app);
        Ok(())
    });
}
