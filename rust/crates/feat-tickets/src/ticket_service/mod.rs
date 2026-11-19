//! Port of `src/services/ticketService.js` (pkg-tickets): panels with buttons or a dropdown, private ticket channels,
//! claim / close / reopen / delete, categories, per-user open-ticket limits (race-safe under an advisory lock) and
//! HTML transcripts. Split by concern; everything is re-exported here, so other crates (the dashboard) call
//! `feat_tickets::ticket_service::<fn>`.

pub mod components;
pub mod handlers;
pub mod db;
pub mod io;
pub mod lifecycle;
pub mod messages;
pub mod open;
pub mod panels;
pub mod rules;
pub mod transcript;

pub use components::*;
pub use handlers::*;
pub use db::*;
pub use io::*;
pub use lifecycle::*;
pub use messages::*;
pub use open::*;
pub use panels::*;
pub use rules::*;
pub use transcript::*;
