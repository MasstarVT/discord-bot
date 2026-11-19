//! Port of `src/commands/moderation/unban.js`: `/unban`. A relief action (no immunity check): the service validates
//! the id, lifts the ban, closes the ban records, cancels a pending expiry job and records the UNBAN.

use bot_core::interaction_helpers::safe_defer;
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::Permissions;

use super::common::{deliver, guild_of, reason_default};
use crate::guild_ops::UserRef;
use crate::moderation_service::{execute_unban, reply_mod_error};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("unban", "Lift a ban by user ID (MEE6 /unban).")
        .default_member_permissions(Permissions::BAN_MEMBERS)
        .option(CommandOption::string("user_id", "ID or mention of the banned user").required(true).min_length(17).max_length(32))
        .option(reason_default())
}

/// `/unban` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

/// `.replace(/[<@!>\s]/g, '')`.
pub fn clean_user_id(raw: &str) -> String {
    raw.chars().filter(|c| !matches!(c, '<' | '@' | '!' | '>') && !bot_util::js::is_js_space(*c)).collect()
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let g = guild_of(&ctx)?;
    let raw = ctx.get_string("user_id").ok_or_else(|| anyhow::anyhow!("Required option \"user_id\" not found."))?;
    let user_id = clean_user_id(&raw);
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let reason = ctx.get_string("reason");
    let res = match execute_unban(&ctx.app, &g, &user_id, &UserRef::of(ctx.user()), reason.as_deref(), "command").await {
        Ok(r) => r,
        Err(e) => {
            reply_mod_error(&ctx, &e).await;
            return Ok(());
        }
    };
    let tag = clamp_text(&res.user.as_ref().map(|u| u.label().to_string()).unwrap_or_else(|| user_id.clone()).replace('`', "'"), 100);
    let mut lines = vec![
        format!("**User:** <@{user_id}> `{tag}`"),
        format!("**Reason:** {}", clamp_text(res.infraction.reason.as_deref().unwrap_or(""), 1000)),
        format!("**Case:** #{}", res.infraction.case_id),
    ];
    if res.cleared > 1 {
        lines.push(format!("**Ban records closed:** {}", res.cleared));
    }
    deliver(&ctx, settings.as_deref(), "User Unbanned", lines, Vec::new()).await;
    Ok(())
}
