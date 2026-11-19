//! Port of `src/commands/moderation/warn.js`: `/warn`. Records an active WARN (the service runs escalation) and
//! reports the active total and any escalation that fired.

use bot_core::interaction_helpers::safe_defer;
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::Permissions;

use super::common::{deliver, guild_of, proof_opt, reason_opt, required_user, result_warnings, tag_of};
use crate::guild_ops::UserRef;
use crate::moderation_service::{ModActionInput, count_active_warns, execute_mod_action, reply_mod_error, tier_action_label};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("warn", "Warn a member; warnings can trigger automatic actions (MEE6 /warn).")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(CommandOption::user("user", "Member to warn").required(true))
        .option(reason_opt("Reason for the warning").required(true))
        .option(proof_opt())
}

/// `/warn` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let g = guild_of(&ctx)?;
    let user = required_user(&ctx, "user")?;
    let reason = ctx.get_string("reason").ok_or_else(|| anyhow::anyhow!("Required option \"reason\" not found."))?;
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let input = ModActionInput {
        target_user: Some(user.clone()),
        reason: Some(reason),
        proof_url: ctx.get_string("proof"),
        ..ModActionInput::new(g.clone(), user.id.to_string(), UserRef::of(ctx.user()), "WARN")
    };
    let res = match execute_mod_action(&ctx.app, input).await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };

    // The count follows the action, so it includes this warning (and ignores pardoned ones).
    let active = match count_active_warns(&ctx.app, &g, &user.id.to_string()).await {
        Ok(n) => Some(n),
        Err(e) => {
            bot_util::log_warn!(format!("/warn: could not count active warnings: {e}"));
            None
        }
    };

    let inf = &res.infraction;
    let mut lines =
        vec![format!("**User:** <@{}> `{}`", user.id, tag_of(&user)), format!("**Reason:** {}", clamp_text(inf.reason.as_deref().unwrap_or(""), 1000))];
    if let Some(n) = active {
        lines.push(format!("**Active warnings:** {n}"));
    }
    lines.push(format!("**Case:** #{}", inf.case_id));

    let mut warnings = result_warnings(&res);
    if let Some(e) = &res.escalation {
        let label = tier_action_label(&e.action).unwrap_or(&e.action);
        if e.ok {
            lines.push(format!("**Escalation T{}:** {label} applied (case #{})", e.tier_id, e.case_id.unwrap_or(0)));
        } else {
            warnings.push(format!(
                "Escalation T{} ({label}) could not be applied: {}",
                e.tier_id,
                clamp_text(e.error.as_deref().unwrap_or("unknown error"), 300)
            ));
        }
    }
    deliver(&ctx, settings.as_deref(), "Warning Issued", lines, warnings).await;
    Ok(())
}
