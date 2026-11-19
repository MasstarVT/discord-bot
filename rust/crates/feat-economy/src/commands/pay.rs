//! Port of `src/commands/economy/pay.js`: `/pay <member> <amount>` (ours; MEE6 /give-coins between members). Moves
//! coins from you to another member in one transaction; coins are never created or destroyed, so a recipient at the
//! server maximum refuses the transfer.

use bot_config::COIN_MAX;
use bot_core::prelude::*;

use super::{BOTS_CANT, edit_embed, guild_of, member_name, opt, private_error, settings_of};
use crate::economy_service::{Transfer, economy_config, format_coins, reply_fail, transfer};

pub fn data() -> SlashCommand {
    SlashCommand::new("pay", "Give some of your coins to another member (MEE6 /give-coins).")
        .option(CommandOption::user("member", "Who receives the coins").required(true))
        .option(CommandOption::integer("amount", "How many coins to give").required(true).min_value(1).max_value(COIN_MAX))
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let target = ctx.get_user("member").ok_or_else(|| anyhow::anyhow!("Required option \"member\" not found."))?;
    let amount = ctx.get_integer("amount").ok_or_else(|| anyhow::anyhow!("Required option \"amount\" not found."))?;
    // Checks that need no database run before the deferral and answer privately.
    if target.id == ctx.user().id {
        return private_error(&ctx, "Not Allowed", "You can't pay yourself.").await;
    }
    if target.bot {
        return private_error(&ctx, "Not Allowed", BOTS_CANT).await;
    }
    if ctx.get_member("member").is_none() {
        return private_error(&ctx, "Not A Member", "That user is not a member of this server.").await;
    }

    ctx.defer_reply(false).await?;
    let guild_id = guild_of(&ctx)?;
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let cfg = economy_config(s);
    let r = transfer(&ctx.app, &guild_id.to_string(), &ctx.user().id.to_string(), &target.id.to_string(), amount, s).await?;
    let (from_balance, to_balance) = match r {
        Transfer::Refused { code: crate::economy_service::EconomyCode::Insufficient, balance } => {
            reply_fail(&ctx.responder, "Insufficient Funds", &format!("You only have {}.", format_coins(balance, s, true))).await;
            return Ok(());
        }
        Transfer::Refused { .. } => {
            let who = bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(&target.name), 100);
            let text = format!("{who} can't hold that many {} (the maximum is {}).", cfg.currency_name, format_coins(cfg.max_balance, s, true));
            reply_fail(&ctx.responder, "Balance Limit", &text).await;
            return Ok(());
        }
        Transfer::Done { from_balance, to_balance } => (from_balance, to_balance),
    };
    let giver_raw = ctx.member().map(member_name).unwrap_or_else(|| ctx.user().name.clone());
    let giver = bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(&giver_raw), 100);
    let description = format!("{giver} gave {} to <@{}>.", format_coins(amount, s, true), target.id);
    let embed = success_embed("Coins Sent", Some(&description), None)
        .field("Your balance", format_coins(from_balance, s, true), true)
        .field("Their balance", format_coins(to_balance, s, true), true);
    edit_embed(&ctx, embed).await
}
