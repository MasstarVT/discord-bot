//! Port of `src/plugins/welcome.js`: the welcome plugin's declaration. The bodies are in
//! [`crate::welcome_service`].

use bot_core::prelude::*;

use crate::welcome_service::{Kind, handle_setup_modal_submit, on_member_join, on_member_leave, on_member_update, on_role_delete, run_autorole_job, update_completion};

/// The `guildMemberRemove` filter: not the bot itself leaving.
pub fn leave_filter(app: &App, user_id: serenity::all::UserId) -> bool {
    app.discord().is_none_or(|d| d.bot_user_id() != user_id)
}

/// The `guildMemberUpdate` filter: a screening completion, and (when memoised) a guild with autoroles.
pub fn update_filter(app: &App, data: &GuildMemberUpdate) -> bool {
    update_completion(app, data).is_some()
        && app.settings().peek_settings(&data.guild_id.to_string()).map(|s| s.autorole_ids.len()).unwrap_or(1) > 0
}

/// The `roleDelete` filter: no memo, or the role is an autorole.
pub fn role_delete_filter(app: &App, guild_id: &str, role_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.autorole_ids.iter().any(|r| r == role_id),
    }
}

/// The plugin (`src/plugins/welcome.js`).
pub fn plugin() -> Plugin {
    Plugin::new("welcome", Some(ModuleKey::Welcome))
        // Autoroles run whatever welcomeEnabled says; message/DM sending check the flag themselves.
        .hook(Hook::<GuildMemberAdd>::new("welcome", on_member_join).ungated().timeout_ms(15_000))
        // Cancels pending autorole jobs always; posts the goodbye only when welcomeEnabled && goodbyeEnabled.
        .hook(Hook::<GuildMemberRemove>::new("welcome", on_member_leave).ungated().filter(|ctx| leave_filter(&ctx.app, ctx.data.user.id)))
        // Membership-screening completion (cached transition, or uncached member: R108-R110).
        .hook(Hook::<GuildMemberUpdate>::new("welcome", on_member_update).ungated().filter(|ctx| update_filter(&ctx.app, &ctx.data)))
        // Prune deleted roles from autoroleIds (autoroles are not module-gated, so neither is pruning).
        .hook(
            Hook::<RoleDelete>::new("welcome", on_role_delete)
                .ungated()
                .filter(|ctx| role_delete_filter(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string())),
        )
        // module: null: the setup forms must work while the module is off (/welcome is gated = false).
        .modal("welcome-setup", |ctx| handle_setup_modal_submit(Kind::Welcome, ctx))
        .component_module(ModuleRef::Ungated)
        .modal("welcome-dm-setup", |ctx| handle_setup_modal_submit(Kind::Dm, ctx))
        .component_module(ModuleRef::Ungated)
        .modal("goodbye-setup", |ctx| handle_setup_modal_submit(Kind::Goodbye, ctx))
        .component_module(ModuleRef::Ungated)
        .job(JobType::WelcomeAutorole, job_handler(run_autorole_job))
}
