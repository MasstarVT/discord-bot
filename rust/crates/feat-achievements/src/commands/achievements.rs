//! Port of `src/commands/community/achievements.js`: `/achievements [member]`: badges, tiers and progress bars,
//! public like MEE6's.

use bot_core::command::CommandOption as Opt;
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Payload, safe_defer, safe_reply};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_util::text::{clamp_text, join_with_limit};
use serenity::all::Permissions;
use serenity::builder::CreateEmbed;

use crate::achievement_service::{EntryView, MEMBER_PAGE_SIZE, TIER_EMOJIS, badge_of, format_member_entry, load_member_view};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("achievements", "Show a member's achievements, badges and progress (MEE6 /achievements)")
        .option(Opt::user("member", "Whose achievements to show (default: you)"))
}

/// `/achievements` (`module 'achievements'`, gated, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Achievements), execute).permission_level(PermissionLevel::Member)
}

fn went_wrong() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("I could not load achievements right now. Please try again in a moment."), None)
}

/// The header line of the member view: `**u/t** tiers unlocked · **c/n** completed` and the badges.
pub fn header(entries: &[crate::achievement_service::MemberEntry]) -> String {
    let total: usize = entries.iter().map(|e| e.a.tiers.len()).sum();
    let unlocked: i64 = entries.iter().map(|e| e.display.tier).sum();
    let completed = entries.iter().filter(|e| e.display.tier >= e.a.tiers.len() as i64).count();
    let badges: Vec<String> = entries
        .iter()
        .filter(|e| e.display.tier >= 1)
        .map(|e| format!("{}{}", badge_of(e.a.emoji.as_deref(), &e.a.trigger), TIER_EMOJIS.get((e.display.tier - 1) as usize).copied().unwrap_or("undefined")))
        .collect();
    let mut h = format!("**{unlocked}/{total}** tiers unlocked · **{completed}/{}** completed", entries.len());
    if !badges.is_empty() {
        h.push_str(&format!("\n**Badges:** {}", join_with_limit(&badges, " ", 300, bot_util::text::default_more)));
    }
    h
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    if let Err(e) = run(&ctx).await {
        bot_util::log_error!("achievements failed", bot_core::error::log_arg(&e));
        safe_reply(&ctx.responder, Payload::embed(went_wrong())).await;
    }
    Ok(())
}

async fn run(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/achievements outside a guild"))?;
    let g = guild_id.to_string();
    let user = ctx.get_user("member").unwrap_or_else(|| ctx.user().clone());
    if user.bot {
        safe_reply(&ctx.responder, Payload::embed(error_embed("Not Available", Some("Bots don't earn achievements."), None))).await;
        return Ok(());
    }
    let member = ctx.get_member("member").or_else(|| if user.id == ctx.user().id { ctx.member().cloned() } else { None });
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let boosting = member.as_ref().and_then(|m| m.premium_since).is_some();
    let Some(entries) = load_member_view(&ctx.app, &g, &user.id.to_string(), boosting).await else {
        safe_reply(&ctx.responder, Payload::embed(went_wrong())).await;
        return Ok(());
    };
    if entries.is_empty() {
        // `memberPermissions.has(ManageGuild)`: discord.js counts Administrator too (checkAdmin defaults to true).
        let is_admin = ctx.interaction.member.as_ref().and_then(|m| m.permissions).is_some_and(|p| p.administrator() || p.contains(Permissions::MANAGE_GUILD));
        let extra = if is_admin { " Create one with `/achievement-config preset` or `/achievement-config create`." } else { "" };
        let embed = apply_guild_color(info_embed("No Achievements", Some(&format!("This server has no achievements yet.{extra}")), None), settings.as_deref());
        safe_reply(&ctx.responder, Payload::embed(embed)).await;
        return Ok(());
    }
    let display_name = match &member {
        Some(m) => bot_core::adapters::member_display_name(m),
        None => bot_core::adapters::user_display_name(&user),
    };
    let head = header(&entries);
    let now = ctx.app.clock().now_ms();
    // `(member ?? user).displayAvatarURL({ extension: 'png', size: 128 })`.
    let opts = || bot_core::adapters::ImageUrlOptions::new("png", Some(128));
    let thumbnail = match &member {
        Some(m) => bot_core::adapters::member_display_avatar_url_with(guild_id, m, opts()),
        None => bot_core::adapters::user_display_avatar_url_with(&user, opts()),
    };
    let title = format!("🏆 {}'s achievements", clamp_text(&display_name, 200));
    let pages: Vec<CreateEmbed> = entries
        .chunks(MEMBER_PAGE_SIZE)
        .map(|chunk| {
            let body: Vec<String> = chunk
                .iter()
                .map(|e| format_member_entry(&e.a, &EntryView { progress: e.display.progress, tier: e.display.tier, pending: e.display.pending, enabled: Some(e.enabled) }, now))
                .collect();
            guild_embed(settings.as_deref(), Some(&title), Some(&clamp_text(&format!("{head}\n\n{}", body.join("\n\n")), 4096)), None).thumbnail(thumbnail.clone())
        })
        .collect();
    paginate(ctx, &pages, PaginateOptions::default()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
