//! Port of `src/commands/economy/richest.js`: `/richest` (MEE6 /richest), the top 100 balances of this server, 10 per
//! page, plus the caller's own rank. Viewing never creates rows and members are mentioned, not fetched.

use bot_core::embed_builder::guild_embed;
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;

use super::{edit_embed, guild_of, opt, settings_of};
use crate::economy_service::{RICHEST_LIMIT, TopAccount, economy_config, fmt_num, format_coins, get_rank, peek_account, top_accounts};

const PER_PAGE: usize = 10;
const MEDALS: [&str; 3] = ["🥇", "🥈", "🥉"];
const TITLE: &str = "🏆 Richest Members";

pub fn data() -> SlashCommand {
    SlashCommand::new("richest", "Show the richest members of this server (MEE6 /richest).")
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

/// The list lines: medals for the top three, then `**#N**`.
pub fn lines(rows: &[TopAccount], settings: Option<&bot_db::GuildSettings>) -> Vec<String> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let place = if i < 3 { MEDALS[i].to_string() } else { format!("**#{}**", i + 1) };
            format!("{place} <@{}> — {}", row.user_id, format_coins(row.balance, settings, true))
        })
        .collect()
}

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let rows = top_accounts(&ctx.app, &g, RICHEST_LIMIT).await?;

    if rows.is_empty() {
        let text = format!("Nobody has any {} yet. Try /daily!", economy_config(s).currency_name);
        return edit_embed(&ctx, guild_embed(s, Some(TITLE), Some(&text), None)).await;
    }

    let lines = lines(&rows, s);
    let me = ctx.user().id.to_string();
    let acc = peek_account(&ctx.app, &g, &me, s).await?;
    // Listed members get their list position: getRank shares a rank between tied balances, so it could disagree with
    // the list.
    let footer = if acc.exists && acc.balance > 0 {
        let rank = match rows.iter().position(|r| r.user_id == me) {
            Some(i) => i as i64 + 1,
            None => get_rank(&ctx.app, &g, acc.balance).await?,
        };
        Some(format!("Your rank: #{} · {}", fmt_num(rank), format_coins(acc.balance, s, false)))
    } else {
        None
    };
    let pages: Vec<_> = lines.chunks(PER_PAGE).map(|chunk| guild_embed(s, Some(TITLE), Some(&chunk.join("\n")), footer.as_deref())).collect();
    paginate(&ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}
