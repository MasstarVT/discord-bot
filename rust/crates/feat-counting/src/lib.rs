//! feat-counting: the Counting module (`/counting` and the counting game). The worked example of a feature crate
//! (`rust/docs/FRAMEWORK.md`).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/countingService.js` | [`counting_service`] |
//! | `src/plugins/counting.js` | [`plugin`] |
//! | `src/commands/admin/counting.js` | [`commands::counting`] |
//!
//! Other crates (the dashboard, other features) call [`counting_service`] directly.

pub mod commands {
    pub mod counting;
}
pub mod counting_service;
pub mod plugin;

/// Registers `/counting` and the `counting` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::counting::command());
    reg.plugin(plugin::plugin());
}
