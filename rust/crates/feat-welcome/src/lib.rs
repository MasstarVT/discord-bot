//! feat-welcome: the Welcome & Goodbye module (`/welcome`, the welcome plugin) and the Applications module
//! (`/application`, the applications plugin).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/welcomeService.js` | [`welcome_service`] |
//! | `src/services/applicationService.js` | [`application_service`] |
//! | `src/plugins/welcome.js` | [`plugins::welcome`] |
//! | `src/plugins/applications.js` | [`plugins::applications`] |
//! | `src/commands/admin/welcome.js` | [`commands::welcome`] |
//! | `src/commands/community/application.js` | [`commands::application`] |
//!
//! Other crates (feat-captcha, feat-events, the dashboard) call [`welcome_service`] and [`application_service`]
//! directly. Discord REST calls of both services go through [`io`], which tests replace with a fake.

pub mod application_service;
pub mod commands {
    pub mod application;
    pub mod welcome;
}
pub mod io;
pub mod plugins {
    pub mod applications;
    pub mod welcome;
}
pub mod welcome_service;

/// Registers `/welcome`, `/application` and the `welcome` and `applications` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::welcome::command());
    reg.command(commands::application::command());
    reg.plugin(plugins::welcome::plugin());
    reg.plugin(plugins::applications::plugin());
}
