//! Port of `src/commands/moderation/kick.js`: `/kick`. The Discord action runs first inside [`execute_mod_action`];
//! nothing is recorded when it fails.

use bot_core::interaction_helpers::safe_defer;
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::Permissions;

use super::common::{deliver, guild_of, proof_opt, reason_default, required_user, result_warnings, tag_of};
use crate::guild_ops::UserRef;
use crate::moderation_service::{ModActionInput, execute_mod_action, reply_mod_error};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("kick", "Kick a member from the server (MEE6 /kick).")
        .default_member_permissions(Permissions::KICK_MEMBERS)
        .option(CommandOption::user("user", "Member to kick").required(true))
        .option(reason_default())
        .option(proof_opt())
}

/// `/kick` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let g = guild_of(&ctx)?;
    let user = required_user(&ctx, "user")?;
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let input = ModActionInput {
        target_user: Some(user.clone()),
        reason: ctx.get_string("reason"),
        proof_url: ctx.get_string("proof"),
        ..ModActionInput::new(g, user.id.to_string(), UserRef::of(ctx.user()), "KICK")
    };
    let res = match execute_mod_action(&ctx.app, input).await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };
    let inf = &res.infraction;
    let lines = vec![
        format!("**User:** <@{}> `{}`", user.id, tag_of(&user)),
        format!("**Reason:** {}", clamp_text(inf.reason.as_deref().unwrap_or(""), 1000)),
        format!("**Case:** #{}", inf.case_id),
    ];
    deliver(&ctx, settings.as_deref(), "Member Kicked", lines, result_warnings(&res)).await;
    Ok(())
}
