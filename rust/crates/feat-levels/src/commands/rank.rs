//! Port of `src/commands/leveling/rank.js`: `/rank`, the rank card of a member (MEE6 /rank). Uses the member's server
//! name and avatar (R245), the leaderboard's tie rule for the position (R243) and the server's rank-card style.

use std::sync::Arc;

use bot_cards::{ProfileCardOptions, RankCardStyle, generate_profile_card, normalize_card_style};
use bot_core::adapters::{member_display_avatar_url, user_display_avatar_url};
use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Payload, cached_guild, is_interaction_gone, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_util::safe_fetch::ImageOptions;
use bot_util::text::format_number;
use serde_json::Value;
use serenity::builder::{CreateAttachment, CreateEmbed};

use crate::xp_service::{display_name, find_user_level, get_rank, level_from_xp, xp_into_level};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("rank", "Show your rank card or a member's (MEE6 /rank).").option(Opt::user("user", "Member to look up (default: you)"))
}

/// `/rank` (`module 'leveling'`, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("leveling", data(), Some(ModuleKey::Leveling), execute).permission_level(PermissionLevel::Member)
}

/// `safeFetchImage(url, { maxBytes: 2 MB, timeoutMs: 5000 }).catch(() => null)`: the avatar for a card.
pub async fn fetch_avatar(url: String) -> Option<Vec<u8>> {
    let opts = ImageOptions { max_bytes: 2 * 1024 * 1024, timeout_ms: 5000, ..ImageOptions::default() };
    bot_util::safe_fetch::safe_fetch_image(&url, &opts).await.ok()
}

/// What a rank card shows.
#[derive(Debug, Clone)]
pub struct CardData {
    pub avatar: Option<Vec<u8>>,
    pub username: String,
    pub display_name: String,
    pub level: i64,
    pub total_xp: i64,
    pub rank: i64,
    pub xp_current: f64,
    pub xp_needed: f64,
    pub style: RankCardStyle,
    pub background: Option<Arc<Vec<u8>>>,
}

/// `generateProfileCard(...)`, drawn off the async runtime (`spawn_blocking`).
pub async fn draw_card(d: CardData) -> anyhow::Result<Vec<u8>> {
    let png = tokio::task::spawn_blocking(move || {
        generate_profile_card(&ProfileCardOptions {
            avatar_buffer: d.avatar.as_deref(),
            username: &d.username,
            display_name: &d.display_name,
            level: d.level as f64,
            total_xp: d.total_xp as f64,
            rank: d.rank as f64,
            xp_current: Some(d.xp_current),
            xp_needed: Some(d.xp_needed),
            style: Some(&d.style),
            background_buffer: d.background.as_deref().map(|b| b.as_slice()),
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("card task failed: {e}"))?;
    png.map_err(|e| anyhow::anyhow!("{e}"))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/rank outside a guild"))?;
    let g = guild_id.to_string();
    let user = ctx.get_user("user").unwrap_or_else(|| ctx.user().clone());
    let member = ctx.get_member("user").or_else(|| (user.id == ctx.user().id).then(|| ctx.member().cloned()).flatten());
    let name = display_name(member.as_ref(), &user);

    if user.bot {
        safe_reply(&ctx.responder, Payload::embed(info_embed("No Rank", Some("Bots don't earn XP."), None))).await;
        return Ok(());
    }

    // Independent waits overlap: the avatar download starts at once, the row and settings load together, then the
    // position and background load together.
    let avatar_url = match member.as_ref() {
        Some(m) => member_display_avatar_url(guild_id, m),
        None => user_display_avatar_url(&user),
    };
    let avatar_task = tokio::spawn(fetch_avatar(avatar_url.clone()));
    let u = user.id.to_string();
    let (record, settings) = tokio::join!(find_user_level(ctx.app.db(), &g, &u), ctx.app.settings().get_settings(&g));
    let (record, settings) = (record?, settings?);
    let Some(record) = record.filter(|r| r.xp > 0) else {
        let text = format!("{name} hasn't earned any XP in this server yet.");
        safe_reply(&ctx.responder, Payload::embed(info_embed("No XP Yet", Some(&text), None))).await;
        return Ok(());
    };

    let xp = i64::from(record.xp);
    let level = level_from_xp(xp as f64);
    let xp_current = xp_into_level(xp as f64);
    let xp_needed = bot_config::xp_for_level(level) as f64;
    let style = normalize_card_style(settings.rank_card_style.as_ref().unwrap_or(&Value::Null));
    let (rank, avatar, background) = tokio::join!(
        get_rank(ctx.app.db(), &g, &u, xp),
        async { avatar_task.await.ok().flatten() },
        bot_cards::profile_card::load_card_background(style.background_url.as_deref()),
    );
    let rank = rank?;
    let card = draw_card(CardData {
        avatar,
        username: user.name.clone(),
        display_name: name.clone(),
        level,
        total_xp: xp,
        rank,
        xp_current,
        xp_needed,
        style,
        background,
    })
    .await;
    match card {
        Ok(png) => match ctx.edit_reply(Payload::new().file(CreateAttachment::bytes(png, "rank.png")).to_edit()).await {
            Ok(_) => return Ok(()),
            Err(e) if is_interaction_gone(&e) => return Ok(()),
            Err(e) => bot_util::log_warn!(format!("rank: could not send the card: {e}")),
        },
        Err(e) => bot_util::log_warn!(format!("rank: could not draw the card for {u} in {g}: {e}")),
    }

    // Fallback: an embed with the same numbers.
    let guild_name = cached_guild(&ctx.app, guild_id).map(|g| g.name).unwrap_or_default();
    let embed = fallback_embed(&settings, &name, level, rank, xp, xp_current, xp_needed, &record, &avatar_url, &guild_name);
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
    Ok(())
}

/// The embed `/rank` answers when the card cannot be drawn or sent.
#[allow(clippy::too_many_arguments)]
pub fn fallback_embed(
    settings: &bot_db::GuildSettings,
    name: &str,
    level: i64,
    rank: i64,
    xp: i64,
    xp_current: f64,
    xp_needed: f64,
    record: &bot_db::UserLevel,
    avatar_url: &str,
    guild_name: &str,
) -> CreateEmbed {
    guild_embed(Some(settings), Some(&format!("{name}'s Rank")), None, Some(guild_name))
        .field("Level", format_number(level as f64), true)
        .field("Rank", format!("#{}", format_number(rank as f64)), true)
        .field("Total XP", format_number(xp as f64), true)
        .field("Progress", format!("{} / {} XP", format_number(xp_current), format_number(xp_needed)), true)
        .field("Messages", format_number(f64::from(record.message_count)), true)
        .field("Voice", format!("{} min", format_number(f64::from(record.voice_minutes))), true)
        .thumbnail(avatar_url)
}
