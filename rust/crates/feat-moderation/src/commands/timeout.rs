//! Port of `src/commands/moderation/timeout.js`: `/timeout`. A native timeout up to 28 days; the service switches to
//! the mute role beyond that (when one is set).

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
    SlashCommand::new("timeout", "Time out a member; over 28 days uses the mute role (MEE6 /mute, /tempmute).")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(CommandOption::user("user", "Member to time out").required(true))
        .option(CommandOption::string("duration", "Length, e.g. 10m, 2h, 7d (over 28d needs a mute role)").required(true).max_length(32))
        .option(reason_default())
        .option(proof_opt())
}

/// `/timeout` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let g = guild_of(&ctx)?;
    let user = required_user(&ctx, "user")?;
    let raw = ctx.get_string("duration").ok_or_else(|| anyhow::anyhow!("Required option \"duration\" not found."))?;
    let Some(duration_ms) = parse_duration(&raw).filter(|d| *d != 0) else {
        return edit_embed(&ctx, invalid_duration(&raw)).await;
    };

    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let input = ModActionInput {
        target_user: Some(user.clone()),
        reason: ctx.get_string("reason"),
        duration_ms: Some(duration_ms as f64),
        proof_url: ctx.get_string("proof"),
        ..ModActionInput::new(g, user.id.to_string(), UserRef::of(ctx.user()), "TIMEOUT")
    };
    let res = match execute_mod_action(&ctx.app, input).await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };
    let inf = &res.infraction;
    let mut lines = vec![format!("**User:** <@{}> `{}`", user.id, tag_of(&user)), format!("**Duration:** {}", format_duration_long(duration_ms as f64))];
    if let Some(at) = inf.expires_at {
        lines.push(format!("**Ends:** {}", bot_util::time::discord_timestamp(&at, "R")));
    }
    lines.push(format!("**Reason:** {}", clamp_text(inf.reason.as_deref().unwrap_or(""), 1000)));
    lines.push(format!("**Case:** #{}", inf.case_id));
    let title = if res.action == "MUTE" { "Member Muted" } else { "Member Timed Out" };
    deliver(&ctx, settings.as_deref(), title, lines, result_warnings(&res)).await;
    Ok(())
}
