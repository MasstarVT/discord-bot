//! Port of `src/commands/moderation/ban.js`: `/ban`, optionally for a set time. The Discord action runs first inside
//! [`execute_mod_action`]; nothing is recorded when it fails.

use bot_core::interaction_helpers::safe_defer;
use bot_core::prelude::*;
use bot_util::parse_duration::{format_duration_long, parse_duration};
use bot_util::text::clamp_text;
use serenity::all::Permissions;

use super::common::{deliver, edit_embed, guild_of, invalid_duration, proof_opt, reason_default, required_user, result_warnings, tag_of};
use crate::guild_ops::UserRef;
use crate::moderation_service::{ModActionInput, execute_mod_action, reply_mod_error};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("ban", "Ban a user, optionally for a set time (MEE6 /ban, /tempban).")
        .default_member_permissions(Permissions::BAN_MEMBERS)
        .option(CommandOption::user("user", "User to ban (members or users who left)").required(true))
        .option(reason_default())
        .option(CommandOption::string("duration", "Temporary ban length, e.g. 12h or 7d (max 1 year). Omit for permanent").max_length(32))
        .option(CommandOption::integer("delete_days", "Days of their messages to delete (0-7, default 0)").min_value(0).max_value(7))
        .option(proof_opt())
}

/// `/ban` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let g = guild_of(&ctx)?;
    let user = required_user(&ctx, "user")?;
    let reason = ctx.get_string("reason");
    let duration_str = ctx.get_string("duration");
    let delete_days = ctx.get_integer("delete_days").unwrap_or(0);
    let proof = ctx.get_string("proof");

    // A duration that was typed but cannot be read must never silently become a permanent ban.
    let mut duration_ms = None;
    if let Some(raw) = &duration_str {
        match parse_duration(raw).filter(|d| *d != 0) {
            Some(d) => duration_ms = Some(d),
            None => return edit_embed(&ctx, invalid_duration(raw)).await,
        }
    }

    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let action = if duration_ms.is_some() { "TEMPBAN" } else { "BAN" };
    let input = ModActionInput {
        target_user: Some(user.clone()),
        reason,
        duration_ms: duration_ms.map(|d| d as f64),
        delete_message_seconds: Some(delete_days * 86_400),
        proof_url: proof,
        ..ModActionInput::new(g, user.id.to_string(), UserRef::of(ctx.user()), action)
    };
    let res = match execute_mod_action(&ctx.app, input).await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };
    let inf = &res.infraction;
    let temporary = res.action == "TEMPBAN";
    let mut lines = vec![
        format!("**User:** <@{}> `{}`", user.id, tag_of(&user)),
        format!("**Reason:** {}", clamp_text(inf.reason.as_deref().unwrap_or(""), 1000)),
        format!("**Duration:** {}", duration_ms.map(|d| format_duration_long(d as f64)).unwrap_or_else(|| "Permanent".into())),
    ];
    if temporary && let Some(at) = inf.expires_at {
        lines.push(format!("**Ends:** {}", bot_util::time::discord_timestamp(&at, "R")));
    }
    if delete_days > 0 {
        lines.push(format!("**Messages deleted:** {delete_days} day(s)"));
    }
    lines.push(format!("**Case:** #{}", inf.case_id));
    let title = if temporary { "User Temporarily Banned" } else { "User Banned" };
    deliver(&ctx, settings.as_deref(), title, lines, result_warnings(&res)).await;
    Ok(())
}
