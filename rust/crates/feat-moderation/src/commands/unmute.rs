//! Port of `src/commands/moderation/unmute.js`: `/unmute`. Removes the timeout and/or the mute role and closes stale
//! TIMEOUT / MUTE records. A relief action (no immunity check); a member who left only has their records closed.

use bot_core::interaction_helpers::safe_defer;
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::Permissions;

use super::common::{deliver, guild_of, reason_default, required_user, tag_of};
use crate::guild_ops::UserRef;
use crate::moderation_service::{execute_unmute, reply_mod_error};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("unmute", "Remove a member's timeout or mute role (MEE6 /unmute).")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(CommandOption::user("user", "Member to unmute").required(true))
        .option(reason_default())
}

/// `/unmute` (`module 'moderation'`, MODERATOR).
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
    let reason = ctx.get_string("reason");
    let res = match execute_unmute(&ctx.app, &g, &UserRef::of(&user), None, &UserRef::of(ctx.user()), reason.as_deref(), "command").await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };
    let inf = &res.infraction;
    let removed = if res.removed.is_empty() { "nothing (records only)".to_string() } else { res.removed.join(" and ") };
    let lines = vec![
        format!("**User:** <@{}> `{}`", user.id, tag_of(&user)),
        format!("**Removed:** {removed}"),
        format!("**Records closed:** {}", res.cleared),
        format!("**Reason:** {}", clamp_text(inf.reason.as_deref().unwrap_or(""), 1000)),
        format!("**Case:** #{}", inf.case_id),
    ];
    deliver(&ctx, settings.as_deref(), "Member Unmuted", lines, res.note.into_iter().collect()).await;
    Ok(())
}
