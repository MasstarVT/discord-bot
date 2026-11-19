//! feat-tempvoice: Temporary Channels (join-to-create voice hubs, `/voice`, `/voice-hub`) and Voice Roles
//! (`/voicerole`).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/tempVoiceService.js` | [`temp_voice_service`] |
//! | `src/services/voiceRoleService.js` | [`voice_role_service`] |
//! | `src/plugins/temp-voice.js` | [`plugins::temp_voice`] |
//! | `src/plugins/voice-roles.js` | [`plugins::voice_roles`] |
//! | `src/commands/admin/voice-hub.js` | [`commands::voice_hub`] |
//! | `src/commands/admin/voicerole.js` | [`commands::voicerole`] |
//! | `src/commands/community/voice.js` | [`commands::voice`] |
//!
//! Every Discord REST call of both services goes through [`discord_port`] (the JS `discordCall`). Other crates (events,
//! role persistence, the dashboard) call the services directly.

pub mod commands {
    pub mod voice;
    pub mod voice_hub;
    pub mod voicerole;
}
pub mod discord_port;
pub mod plugins {
    pub mod temp_voice;
    pub mod voice_roles;
}
pub mod temp_voice_service;
pub mod voice_role_service;

/// Registers `/voice`, `/voice-hub`, `/voicerole` and the `temp-voice` and `voice-roles` plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::voice_hub::command());
    reg.command(commands::voicerole::command());
    reg.command(commands::voice::command());
    reg.plugin(plugins::temp_voice::plugin());
    reg.plugin(plugins::voice_roles::plugin());
}
