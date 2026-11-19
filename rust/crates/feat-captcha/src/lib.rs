//! feat-captcha: captcha verification and sticky messages.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/captchaService.js` | [`captcha_service`] |
//! | `src/services/stickyService.js` | [`sticky_service`] |
//! | `src/plugins/captcha.js` | [`plugins::captcha`] |
//! | `src/plugins/sticky.js` | [`plugins::sticky`] |
//! | `src/commands/admin/captcha.js` | [`commands::captcha`] |
//! | `src/commands/admin/sticky.js` | [`commands::sticky`] |

pub mod captcha_service;
pub mod commands;
pub mod plugins;
pub mod sticky_service;

/// Registers this crate's commands and plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::captcha::command());
    reg.command(commands::sticky::command());
    reg.plugin(plugins::captcha::plugin());
    reg.plugin(plugins::sticky::plugin());
}
