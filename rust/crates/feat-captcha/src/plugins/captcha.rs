//! Port of `src/plugins/captcha.js`: the join gate, the leave cleanup, the "role given by someone else" hook, the
//! Verify button, the two forms and the `captcha.timeout` job. The bodies are in [`crate::captcha_service`].

use bot_core::prelude::*;

use crate::captcha_service::{
    handle_captcha_button, handle_captcha_modal, is_bulk_target, is_role_newly_added, on_member_join, on_member_leave, on_member_update,
    run_timeout_job,
};

/// The join gate's filter: unknown settings pass; known ones need the gate on and a role.
pub fn join_filter(app: &App, guild_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.captcha_enabled && s.captcha_role_id.as_deref().is_some_and(|r| !r.is_empty()),
    }
}

/// The leave cleanup's filter (it runs while the module is off: rows created earlier).
pub fn leave_filter(app: &App, guild_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.captcha_enabled || s.captcha_role_id.as_deref().is_some_and(|r| !r.is_empty()),
    }
}

/// The update hook's filter.
pub fn update_filter(app: &App, data: &GuildMemberUpdate) -> bool {
    let g = data.guild_id.to_string();
    if let Some(s) = app.settings().peek_settings(&g) {
        let rid = s.captcha_role_id.as_deref().filter(|r| !r.is_empty());
        if !s.captcha_enabled || rid.is_none() {
            return false;
        }
        let old = data.old_member.as_ref().map(|m| m.roles.as_slice());
        if !is_role_newly_added(old, &data.member.roles, data.old_known, rid) {
            return false;
        }
    }
    !is_bulk_target(app, &g, &data.member.user.id.to_string())
}

/// The plugin (`id: 'captcha'`, `module: 'captcha'`).
pub fn plugin() -> Plugin {
    Plugin::new("captcha", Some(ModuleKey::Captcha))
        // Join gate. Runs after invites/logging and BEFORE welcome (HOOK_ORDER), so ctx.state.captchaPending reaches welcome.
        .hook(Hook::<GuildMemberAdd>::new("captcha", on_member_join).filter(|ctx| join_filter(&ctx.app, &ctx.data.guild_id.to_string())))
        // Cleanup must run even while the module is off (rows created earlier).
        .hook(Hook::<GuildMemberRemove>::new("captcha", on_member_leave).always().filter(|ctx| leave_filter(&ctx.app, &ctx.data.guild_id.to_string())))
        // The verified role given by someone else verifies a pending member.
        .hook(Hook::<GuildMemberUpdate>::new("captcha", on_member_update).filter(|ctx| update_filter(&ctx.app, &ctx.data)))
        // captcha:start | captcha:answer: gated by the module through the component wrapper.
        .button("captcha", handle_captcha_button)
        // captcha-modal:answer (checks the module itself) | captcha-modal:panel (admin form; must work while the module is off).
        .modal("captcha-modal", handle_captcha_modal)
        .component_module(ModuleRef::Ungated)
        .job(JobType::CaptchaTimeout, job_handler(run_timeout_job))
}
