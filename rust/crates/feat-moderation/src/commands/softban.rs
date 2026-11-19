//! Port of `src/commands/moderation/softban.js`: `/softban` (ban and instantly unban to delete recent messages). An
//! already-banned user is refused (ALREADY) so the standing ban is never lifted.

use bot_core::interaction_helpers::safe_defer;
use bot_core::prelude::*;
use bot_db::InfractionType;
use bot_util::text::clamp_text;
use serenity::all::Permissions;

use super::common::{deliver, guild_of, proof_opt, reason_default, required_user, result_warnings, tag_of};
use crate::guild_ops::UserRef;
use crate::moderation_service::{ModActionInput, execute_mod_action, reply_mod_error};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("softban", "Ban and instantly unban a user to delete their recent messages.")
        .default_member_permissions(Permissions::BAN_MEMBERS)
        .option(CommandOption::user("user", "User to softban").required(true))
        .option(reason_default())
        .option(CommandOption::integer("delete_days", "Days of their messages to delete (1-7, default 7)").min_value(1).max_value(7))
        .option(proof_opt())
}

/// `/softban` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let g = guild_of(&ctx)?;
    let user = required_user(&ctx, "user")?;
    let delete_days = ctx.get_integer("delete_days").unwrap_or(7);
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let input = ModActionInput {
        target_user: Some(user.clone()),
        reason: ctx.get_string("reason"),
        delete_message_seconds: Some(delete_days * 86_400),
        proof_url: ctx.get_string("proof"),
        ..ModActionInput::new(g, user.id.to_string(), UserRef::of(ctx.user()), "SOFTBAN")
    };
    let res = match execute_mod_action(&ctx.app, input).await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };
    let inf = &res.infraction;
    // A softban whose automatic unban failed is stored as an active BAN (the service adds the warning).
    let stuck = inf.r#type == InfractionType::Ban;
    let lines = vec![
        format!("**User:** <@{}> `{}`", user.id, tag_of(&user)),
        format!("**Reason:** {}", clamp_text(inf.reason.as_deref().unwrap_or(""), 1000)),
        format!("**Messages deleted:** {delete_days} day(s)"),
        format!("**Case:** #{}", inf.case_id),
    ];
    let title = if stuck { "User Banned (Softban Incomplete)" } else { "User Softbanned" };
    deliver(&ctx, settings.as_deref(), title, lines, result_warnings(&res)).await;
    Ok(())
}
