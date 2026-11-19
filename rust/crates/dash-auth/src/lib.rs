//! dash-auth: the dashboard's configuration, authentication and the shared pieces of its public pages.
//!
//! | JS file | Rust module |
//! |---|---|
//! | `src/dashboard/config.js` | [`config`] |
//! | `src/dashboard/auth.js` | [`auth`] |
//! | `src/dashboard/siteParts.js` | [`site_parts`] |
//! | `src/dashboard/docs.js` | [`docs`] |
//! | `src/dashboard/changelog.js` | [`changelog`] |
//! | `src/dashboard/commandList.js` | [`command_list`] |
//! | the pure rate-limit helpers of `src/dashboard/server.js` | [`rate_limit`] |
//!
//! Everything here works over plain values (header strings, cookie strings, URLs, config structs) so the axum
//! handlers of `dash-server` can call it; nothing starts a server.

pub mod auth;
pub mod changelog;
pub mod command_list;
pub mod config;
pub mod docs;
pub mod js_ip;
pub mod rate_limit;
pub mod site_parts;

pub use config::{load_dashboard_config, DashboardConfig, LoadedConfig};
