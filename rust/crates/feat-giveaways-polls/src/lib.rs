//! feat-giveaways-polls: the Giveaways and Polls modules.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/giveawayService.js` | [`giveaway_service`] |
//! | `src/services/pollService.js` | [`poll_service`] |
//! | `src/plugins/giveaways.js` | [`plugins::giveaways`] |
//! | `src/plugins/polls.js` | [`plugins::polls`] |
//! | `src/commands/community/giveaway.js` | [`commands::giveaway`] |
//! | `src/commands/community/poll.js` | [`commands::poll`] |
//!
//! Shared by both services: [`discord_ops`] (the REST seam) and [`guild_view`] (cached-guild lookups).
//! Other crates (the dashboard tasks) call [`giveaway_service`] and [`poll_service`] directly.

pub mod commands {
    pub mod giveaway;
    pub mod poll;
}
pub mod plugins {
    pub mod giveaways;
    pub mod polls;
}
pub mod discord_ops;
pub mod giveaway_service;
pub mod guild_view;
pub mod poll_service;

/// Registers `/giveaway`, `/poll` and the `giveaways` and `polls` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::giveaway::command());
    reg.command(commands::poll::command());
    reg.plugin(plugins::giveaways::plugin());
    reg.plugin(plugins::polls::plugin());
}
