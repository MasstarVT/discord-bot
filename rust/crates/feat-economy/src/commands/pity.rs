//! Port of `src/commands/economy/pity.js`: `/pity`, [`PITY_AMOUNT`] coins when the balance is below
//! [`PITY_THRESHOLD`], once per 12 hours, per server. Refused while a casino game is running (the escrowed bet would
//! make the balance look low).

use bot_core::prelude::*;
use bot_util::time::discord_timestamp;

use super::{edit_embed, guild_of, opt, settings_of};
use crate::economy_service::{
    PITY_AMOUNT, PITY_THRESHOLD, PityClaim, claim_pity, format_coins, reply_fail_embed, try_lock_casino, unlock_casino,
};

pub fn data() -> SlashCommand {
    SlashCommand::new("pity", format!("Get {PITY_AMOUNT} coins when your balance is below {PITY_THRESHOLD} (12-hour cooldown)."))
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

/// Releases the casino lock however the command ends (`finally { unlockCasino(...) }`).
struct CasinoGuard {
    app: App,
    guild_id: String,
    user_id: String,
}

impl Drop for CasinoGuard {
    fn drop(&mut self) {
        unlock_casino(&self.app, &self.guild_id, &self.user_id);
    }
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let guild_id = guild_of(&ctx)?;
    let (g, u) = (guild_id.to_string(), ctx.user().id.to_string());
    // The casino lock is TAKEN (not just read): a /blackjack sent together with /pity could otherwise escrow the whole
    // balance between the check and the claim, and pity would pay out against that bet.
    if !try_lock_casino(&ctx.app, &g, &u) {
        ctx.reply_embed(warn_embed("Game In Progress", Some("Finish your casino game first."), None), true).await?;
        return Ok(());
    }
    let _guard = CasinoGuard { app: ctx.app.clone(), guild_id: g.clone(), user_id: u.clone() };
    ctx.defer_reply(false).await?;
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    match claim_pity(&ctx.app, &g, &u, s).await? {
        PityClaim::Claimed { balance, .. } => {
            let description = format!("You received {} to get back in the game!", format_coins(PITY_AMOUNT, s, true));
            edit_embed(&ctx, success_embed("💸 Pity Claimed", Some(&description), None).field("Balance", format_coins(balance, s, true), true)).await
        }
        PityClaim::TooRich { balance } => {
            let text = format!(
                "Your balance is {}. Pity is only for balances below {}.",
                format_coins(balance, s, true),
                format_coins(PITY_THRESHOLD, s, true)
            );
            reply_fail_embed(&ctx.responder, info_embed("Pity Not Available", Some(&text), None)).await;
            Ok(())
        }
        PityClaim::NotYet { next_at } => {
            let text = format!("You can claim pity again {}.", discord_timestamp(&next_at, "R"));
            reply_fail_embed(&ctx.responder, warn_embed("Pity On Cooldown", Some(&text), None)).await;
            Ok(())
        }
    }
}
