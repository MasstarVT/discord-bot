//! Port of `src/commands/leveling/leaderboard.js`: `/leaderboard`, the server XP leaderboard (MEE6 /levels). One query
//! for the top 200 (xp desc, userId asc: the same tie rule as /rank), at most two member lookups of 100 for names,
//! pages built in memory (R092, R093).

use std::collections::HashMap;

use bot_core::adapters::member_display_name;
use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Payload, cached_guild, safe_defer, safe_reply};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_util::text::{clamp_text, format_number};
use serenity::all::{GuildId, Member};
use serenity::builder::CreateEmbed;

use crate::xp_service::{LEADERBOARD_MAX_ROWS, fetch_members, find_user_level, get_rank, level_from_xp, ranked_count, ranked_rows};

/// `PAGE_SIZE`.
pub const PAGE_SIZE: usize = 10;
const MEMBER_CHUNK: usize = 100;
const MAX_MEMBER_CHUNKS: usize = 2;

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("leaderboard", "Show the server XP leaderboard (MEE6 /levels).")
        .option(Opt::integer("page", "Page number").min_value(1).max_value(20))
}

/// `/leaderboard` (`module 'leveling'`, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("leveling", data(), Some(ModuleKey::Leveling), execute).permission_level(PermissionLevel::Member)
}

/// `guild.iconURL()` (discord.js defaults: WebP, or GIF for an animated icon; no size).
pub fn guild_icon_url(guild_id: GuildId, icon: Option<&serenity::all::ImageHash>) -> Option<String> {
    icon.map(|h| {
        let h = h.to_string();
        let ext = if h.starts_with("a_") { "gif" } else { "webp" };
        format!("https://cdn.discordapp.com/icons/{guild_id}/{h}.{ext}")
    })
}

/// One leaderboard line.
pub fn line(rank: usize, user_id: &str, display_name: Option<&str>, xp: i64) -> String {
    let name = match display_name {
        Some(n) => format!("**{}**", bot_core::markdown::escape_markdown(&clamp_text(n, 32))),
        None => format!("<@{user_id}>"),
    };
    let medal = match rank {
        1 => "🥇".to_string(),
        2 => "🥈".to_string(),
        3 => "🥉".to_string(),
        n => format!("**#{n}**"),
    };
    format!("{medal} {name} — Level **{}** · {} XP", level_from_xp(xp as f64), format_number(xp as f64))
}

/// The footer (the paginator appends " • Page X/N" itself, R244).
pub fn footer(shown: usize, total: i64, my_rank: Option<i64>) -> String {
    let mine = my_rank.map(|r| format!(" · You: #{}", format_number(r as f64))).unwrap_or_default();
    format!("Top {shown} of {} ranked members{mine}", format_number(total as f64))
}

/// `Math.min(Math.max((page ?? 1) - 1, 0), pages.length - 1)`.
pub fn start_page(page: Option<i64>, pages: usize) -> usize {
    let wanted = (page.unwrap_or(1) - 1).max(0) as usize;
    wanted.min(pages.saturating_sub(1))
}

/// Names without per-row lookups for cached members; the uncached ones of the shown rows (at most two chunks of 100,
/// sequential, 5 s each). Never fails.
async fn fetch_missing_names(app: &App, guild_id: GuildId, rows: &[(String, i64)]) -> HashMap<String, Member> {
    let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let mut out = HashMap::new();
    for chunk in ids.chunks(MEMBER_CHUNK).take(MAX_MEMBER_CHUNKS) {
        out.extend(fetch_members(app, guild_id, chunk, 5000).await);
    }
    out
}

/// The invoker's place when they are outside the top rows; `None` without XP.
async fn own_rank(app: &App, guild_id: &str, user_id: &str) -> Option<i64> {
    let own = find_user_level(app.db(), guild_id, user_id).await.ok().flatten()?;
    if own.xp <= 0 {
        return None;
    }
    get_rank(app.db(), guild_id, user_id, i64::from(own.xp)).await.ok()
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/leaderboard outside a guild"))?;
    let g = guild_id.to_string();
    // The three reads are independent, so they run together instead of one after another.
    let (settings, total, rows) = tokio::join!(
        ctx.app.settings().get_settings(&g),
        ranked_count(ctx.app.db(), &g),
        ranked_rows(ctx.app.db(), &g, Some(0), None, LEADERBOARD_MAX_ROWS),
    );
    let (settings, total, rows) = (settings?, total?, rows?);
    if total == 0 {
        safe_reply(&ctx.responder, Payload::embed(info_embed("Leaderboard", Some("No one has earned XP in this server yet!"), None))).await;
        return Ok(());
    }

    // The invoker's own place (when outside the top rows) is looked up while the member names are fetched.
    let me = ctx.user().id.to_string();
    let my_index = rows.iter().position(|r| r.0 == me);
    let (names, my_rank) = tokio::join!(fetch_missing_names(&ctx.app, guild_id, &rows), async {
        match my_index {
            Some(i) => Some(i as i64 + 1),
            None => own_rank(&ctx.app, &g, &me).await,
        }
    });

    let lines: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, (user_id, xp))| line(i + 1, user_id, names.get(user_id).map(member_display_name).as_deref(), *xp))
        .collect();
    let foot = footer(rows.len(), total, my_rank);
    let guild = cached_guild(&ctx.app, guild_id);
    let guild_name = guild.as_ref().map(|g| g.name.clone()).unwrap_or_default();
    let icon = guild.as_ref().and_then(|g| guild_icon_url(guild_id, g.icon.as_ref()));
    let title = clamp_text(&format!("🏆 {guild_name} Leaderboard"), 256);
    let pages: Vec<CreateEmbed> = lines
        .chunks(PAGE_SIZE)
        .map(|chunk| {
            let e = guild_embed(Some(&settings), Some(&title), Some(&chunk.join("\n")), Some(&foot));
            match &icon {
                Some(url) => e.thumbnail(url),
                None => e,
            }
        })
        .collect();
    let start = start_page(ctx.get_integer("page"), pages.len());
    paginate(&ctx, &pages, PaginateOptions { start_page: start as f64, ..PaginateOptions::default() }).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
