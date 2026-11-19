//! Port of `src/commands/economy/work.js`: `/work` (MEE6 /work), one atomic shift per `workCooldownMinutes` (default
//! 60) that pays workMin..workMax. A qualifying coin boost applies once.

use bot_core::prelude::*;
use bot_util::time::discord_timestamp;

use super::daily::boost_line;
use super::{edit_embed, guild_of, member_role_ids, opt, settings_of};
use crate::economy_service::{WorkClaim, claim_work, format_coins, reply_fail_embed};

pub fn data() -> SlashCommand {
    SlashCommand::new("work", "Work a shift to earn coins (MEE6 /work).")
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    let guild_id = guild_of(&ctx)?;
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let r = claim_work(&ctx.app, &guild_id.to_string(), &ctx.user().id.to_string(), s, &member_role_ids(&ctx)).await?;

    let (amount, balance, capped, next_at, line, boost) = match r {
        WorkClaim::NotYet { next_at } => {
            let text = format!("You can work again {}.", discord_timestamp(&next_at, "R"));
            reply_fail_embed(&ctx.responder, warn_embed("Still Tired", Some(&text), None)).await;
            return Ok(());
        }
        WorkClaim::Claimed { amount, balance, capped, next_at, line, boost } => (amount, balance, capped, next_at, line, boost),
    };
    let description = format!("{line} and earned {}!{}", format_coins(amount, s, true), boost_line(boost.as_ref()));
    let embed = success_embed("Shift Complete", Some(&description), None)
        .field("Balance", format_coins(balance, s, true) + if capped { " (server maximum)" } else { "" }, true)
        .field("Next shift", discord_timestamp(&next_at, "R"), true);
    edit_embed(&ctx, embed).await
}
