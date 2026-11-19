//! feat-moderation: the Moderation module (cases, warnings, timeouts, mutes, kicks, bans, escalation, the case log,
//! `/purge` and `/slowmode`).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/moderationService.js` | [`moderation_service`] (Discord calls in [`guild_ops`]) |
//! | `src/plugins/moderation.js` | [`plugin`] |
//! | `src/commands/moderation/<name>.js` | [`commands`]`::<name>` |
//!
//! Other crates (automod, captcha, anti-raid, the dashboard) call [`moderation_service`] directly.

pub mod commands {
    pub mod ban;
    pub mod case;
    pub mod common;
    pub mod kick;
    pub mod modconfig;
    pub mod purge;
    pub mod slowmode;
    pub mod softban;
    pub mod timeout;
    pub mod unban;
    pub mod unmute;
    pub mod warn;
}
pub mod guild_ops;
pub mod moderation_service;
pub mod plugin;

/// The command names this crate registers, in file order.
pub const COMMANDS: [&str; 11] = ["ban", "case", "kick", "modconfig", "purge", "slowmode", "softban", "timeout", "unban", "unmute", "warn"];

/// Registers the moderation commands and the `moderation` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::ban::command());
    reg.command(commands::case::command());
    reg.command(commands::kick::command());
    reg.command(commands::modconfig::command());
    reg.command(commands::purge::command());
    reg.command(commands::slowmode::command());
    reg.command(commands::softban::command());
    reg.command(commands::timeout::command());
    reg.command(commands::unban::command());
    reg.command(commands::unmute::command());
    reg.command(commands::warn::command());
    reg.plugin(plugin::plugin());
}
