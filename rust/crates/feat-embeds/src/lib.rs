//! feat-embeds: the Embed Messages module (`/embed`, saved embeds, the Welcome Channel presets).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/embedService.js` | [`embed_service`] |
//! | `src/plugins/embeds.js` | [`plugin`] |
//! | `src/commands/admin/embed.js` | [`commands::embed`] |
//!
//! Other crates (feat-captcha, feat-automations, the dashboard) call [`embed_service`] directly: above all
//! [`embed_service::get_saved_embed_template`], [`embed_service::search_saved_embeds`] and
//! [`embed_service::normalize_embed_name`].

pub mod commands {
    pub mod embed;
}
pub mod embed_service;
pub mod plugin;

/// Registers `/embed` and the `embeds` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::embed::command());
    reg.plugin(plugin::plugin());
}
