//! feat-events: the Events module (`/event`, RSVP buttons, the edit panel, the event jobs) and Role Persistence
//! (`/rolepersist`, the leave / rejoin hooks).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/eventService.js` | [`event_service`] |
//! | `src/services/rolePersistService.js` | [`role_persist_service`] |
//! | `src/utils/calendar.js` | [`calendar`] (re-exports `bot_util::calendar`) |
//! | `src/plugins/events.js` | [`plugins::events`] |
//! | `src/plugins/role-persist.js` | [`plugins::role_persist`] |
//! | `src/commands/community/event.js` | [`commands::event`] |
//! | `src/commands/admin/rolepersist.js` | [`commands::rolepersist`] |
//!
//! Other crates (the dashboard, other features) call the service modules directly.

pub mod calendar;
pub mod commands {
    pub mod event;
    pub mod rolepersist;
}
pub mod event_service;
pub mod plugins {
    pub mod events;
    pub mod role_persist;
}
pub mod role_persist_service;

/// Registers `/event`, `/rolepersist` and the `events` and `role-persist` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::event::command());
    reg.command(commands::rolepersist::command());
    reg.plugin(plugins::events::plugin());
    reg.plugin(plugins::role_persist::plugin());
}
