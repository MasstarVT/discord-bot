//! feat-automations: the Automations module (When -> If -> Do rules, `/automation`).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/automationService.js` | [`automation_service`] |
//! | `src/plugins/automations.js` | [`plugin`] |
//! | `src/commands/admin/automation.js` | [`commands::automation`] |
//!
//! Other crates (the dashboard) call [`automation_service`] directly.

pub mod commands {
    pub mod automation;
}
pub mod automation_service;
pub mod plugin;

/// Registers `/automation` and the `automations` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::automation::command());
    reg.plugin(plugin::plugin());
}
