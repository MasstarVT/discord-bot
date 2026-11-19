//! dash-server: the web dashboard's HTTP server.
//!
//! | JS file | Rust module |
//! |---|---|
//! | `src/dashboard/server.js` | [`server`] (the router and every route), [`static_files`] (the static UI files), [`serve`] (listening, proxy trust, shutdown, the parked server) |
//! | `src/dashboard/discordApi.js` | [`discord_api`] |
//! | `src/dashboard/landing.js` | [`landing`] |
//!
//! The server is an axum [`axum::Router`] built by [`server::create_dashboard_router`] from the dashboard config
//! (dash-auth), the collections registry and the bridges (dash-collections), a Discord client ([`ServerDiscord`],
//! implemented over Discord's REST API by [`discord_api::DiscordApi`]) and an optional [`server::BotJoins`] source.
//! [`serve::serve`] runs it on the configured host and port. The front end in `src/dashboard/public/` is served
//! unchanged (embedded at build time, or from a directory).

pub mod collate;
pub mod discord_api;
pub mod landing;
pub mod logger;
pub mod serve;
pub mod server;
pub mod static_files;
pub mod util;

pub use discord_api::{BotGuild, CurrentUser, DiscordApi, DiscordApiOptions, OAuthGuild, ServerDiscord};
pub use logger::{BotLogger, DashLogger, RecordingLogger};
pub use serve::{create_parked_router, serve, serve_listener};
pub use server::{
    BotJoinRow, BotJoins, Clock, DashboardDeps, ServerOptions, create_dashboard_router, BODY_LIMIT_BYTES, PROPAGATION_SECONDS,
    RECORD_BODY_LIMIT_BYTES, SECURITY_HEADERS,
};
