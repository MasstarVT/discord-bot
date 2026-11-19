//! Port of `src/commands/economy/daily.js`: `/daily` (MEE6 /daily), one atomic claim per 24 hours with a streak bonus
//! (day 2 +10 %, day 3 +20 %, day 4 +40 %, day 5 +80 %, day 6 x2.5, day 7+ x4). The streak resets after 48 hours. A
//! qualifying coin boost applies once.

use bot_core::prelude::*;
use bot_util::time::discord_timestamp;

use super::{edit_embed, guild_of, member_role_ids, opt, plural, settings_of};
use crate::economy_service::{DailyClaim, claim_daily, format_coins, reply_fail_embed, streak_multiplier_label};

pub fn data() -> SlashCommand {
    SlashCommand::new("daily", "Claim your daily coins; claim on consecutive days for a streak bonus (MEE6 /daily).")
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

/// `\n🚀 Boost **name** +N%` (empty without a boost).
pub(crate) fn boost_line(boost: Option<&crate::economy_service::BoostInfo>) -> String {
    boost
        .map(|b| format!("\n🚀 Boost **{}** +{}%", bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(&b.name), 40), b.percent))
        .unwrap_or_default()
}

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    let guild_id = guild_of(&ctx)?;
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let r = claim_daily(&ctx.app, &guild_id.to_string(), &ctx.user().id.to_string(), s, &member_role_ids(&ctx)).await?;

    let (amount, streak, balance, capped, next_at, boost) = match r {
        DailyClaim::NotYet { next_at, streak } => {
            let days = if streak != 0 { format!("\nCurrent streak: 🔥 {streak} {}.", plural(streak, "day", "days")) } else { String::new() };
            let text = format!(
                "You can claim your next daily reward {} ({}).{days}",
                discord_timestamp(&next_at, "R"),
                discord_timestamp(&next_at, "t")
            );
            reply_fail_embed(&ctx.responder, warn_embed("Already Claimed", Some(&text), None)).await;
            return Ok(());
        }
        DailyClaim::Claimed { amount, streak, balance, capped, next_at, boost, .. } => (amount, streak, balance, capped, next_at, boost),
    };

    let footer = if streak < 7 {
        format!("Tomorrow: {} · claim within 48 hours to keep your streak.", streak_multiplier_label(streak + 1))
    } else {
        "Maximum streak bonus (×4) · claim within 48 hours to keep it.".to_string()
    };
    let description = format!("You received {}!{}", format_coins(amount, s, true), boost_line(boost.as_ref()));
    let embed = success_embed("Daily Reward Claimed", Some(&description), Some(&footer))
        .field("Streak", format!("🔥 Day {streak} ({})", streak_multiplier_label(streak)), true)
        .field("Balance", format_coins(balance, s, true) + if capped { " (server maximum)" } else { "" }, true)
        .field("Next claim", discord_timestamp(&next_at, "R"), true);
    edit_embed(&ctx, embed).await
}
