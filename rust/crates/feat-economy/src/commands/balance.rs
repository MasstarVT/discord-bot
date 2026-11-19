//! Port of `src/commands/economy/balance.js`: `/balance [user]` (MEE6 /coins), a member's coins on THIS server.
//! Looking at yourself creates the account (lazy global copy); looking at someone else never creates rows.

use bot_core::embed_builder::guild_embed;
use bot_core::prelude::*;

use super::{BOTS_CANT, edit_embed, guild_of, opt, plural, private_error, settings_of, target_label};
use crate::economy_service::{AccountPeek, active_streak, fmt_num, format_coins, get_account, get_rank, peek_account};

pub fn data() -> SlashCommand {
    SlashCommand::new("balance", "Show your or a member's coins on this server (MEE6 /coins).")
        .option(CommandOption::user("user", "Member to check (default: you)"))
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let target = ctx.get_user("user").unwrap_or_else(|| ctx.user().clone());
    if target.bot {
        return private_error(&ctx, "Not Allowed", BOTS_CANT).await;
    }
    ctx.defer_reply(false).await?;

    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let is_self = target.id == ctx.user().id;
    let uid = target.id.to_string();
    let acc = if is_self { AccountPeek::from(&get_account(&ctx.app, &g, &uid, s).await?) } else { peek_account(&ctx.app, &g, &uid, s).await? };
    let name = target_label(&ctx, "user", &target, is_self);

    let footer = if is_self { Some("Earn more with /daily and /work.") } else { None };
    let mut embed = guild_embed(s, Some(&format!("💰 {name}'s balance")), Some(&format_coins(acc.balance, s, true)), footer);
    if acc.exists && acc.balance > 0 {
        embed = embed.field("Rank", format!("#{}", fmt_num(get_rank(&ctx.app, &g, acc.balance).await?)), true);
    }
    let streak = active_streak(acc.last_daily, acc.daily_streak, ctx.app.clock().now());
    if streak > 0 {
        embed = embed.field("Daily streak", format!("🔥 {streak} {}", plural(streak, "day", "days")), true);
    }
    edit_embed(&ctx, embed).await
}
