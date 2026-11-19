//! feat-tickets: the Ticketing module (`/ticket-panel`, `/ticket`, the panel components, the channelDelete cleanup and
//! the `ticket.managersync` job).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/ticketService.js` | [`ticket_service`] (split by concern, re-exported flat) |
//! | `src/plugins/tickets.js` | [`plugin`] |
//! | `src/commands/admin/ticket-panel.js` | [`commands::ticket_panel`] |
//! | `src/commands/community/ticket.js` | [`commands::ticket`] |
//!
//! Other crates (the dashboard) call [`ticket_service`] directly.

pub mod commands {
    pub mod ticket;
    pub mod ticket_panel;
}
pub mod plugin;
pub mod ticket_service;

/// Registers `/ticket-panel`, `/ticket` and the `tickets` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::ticket_panel::command());
    reg.command(commands::ticket::command());
    reg.plugin(plugin::plugin());
}
