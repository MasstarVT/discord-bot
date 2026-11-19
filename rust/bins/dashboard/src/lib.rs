//! The web dashboard binary as a library, so the tests can drive each part.
//!
//! | JS | Rust |
//! |---|---|
//! | `dashboard.js` (configuration, parking, listen, signals, shutdown) | [`index`] |
//! | `src/dashboard/recordsBridge.js` | [`records_bridge`] |
//! | `src/dashboard/settingsBridge.js` | [`settings_bridge`] |
//! | `src/dashboard/tasks.js` | [`tasks`] |
//! | `deps.botJoins` (`src/services/botJoinService.js`) | [`bot_joins`] |
//!
//! The HTTP server itself (`src/dashboard/server.js`, `discordApi.js`) is dash-server; the stores are dash-records-a
//! and dash-records-b; the bot side of the tasks is feat-dashboard-tasks.

pub mod bot_joins;
pub mod index;
pub mod records_bridge;
pub mod settings_bridge;
pub mod tasks;
