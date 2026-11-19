//! Port of `src/commands/admin/levelconfig.js`: `/levelconfig`, the Levels settings: XP range, cooldown, multiplier,
//! level-up announcements, voice XP, the rank card, role rewards, XP boost roles and no-XP exemptions. Writes go
//! through the settings service only. Dispatch is group-first (R002).
//!
//! The `xp` group (MEE6 /give-xp and /remove-xp) manages member XP: every write is an atomic SQL statement and goes
//! through `handle_level_change`, so reward roles and the `levelUp` hook stay in sync (R196, R299, R300). Admin
//! changes never post a level-up announcement; the reply shows the level change instead. It answers privately.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bot_cards::normalize_card_style;
use bot_cards::profile_card::{BackgroundCheck, clear_card_background, load_card_background, validate_card_background};
use bot_core::adapters::{member_display_avatar_url, member_display_name, render_context, user_display_avatar_url};
use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::custom_embed::{format_color, is_http_url, parse_color_str};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, Payload, await_confirm, cached_channel, cached_guild, check_assignable_role, check_channel_usable, respond_autocomplete,
    safe_defer, safe_reply,
};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::{GuildSettings, InsertBuilder, LEVEL_ROLE_INFO, LevelRole};
use bot_util::js::number_to_string;
use bot_util::placeholder_parser::render_placeholders_detailed;
use bot_util::text::{clamp_text, default_more, format_number, join_with_limit};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GenericId, GuildId, Member, Permissions, RoleId, UserId};
use serenity::builder::{CreateAttachment, CreateEmbed};

use crate::commands::rank::{CardData, draw_card, fetch_avatar};
use crate::xp_service::{
    self as svc, ANNOUNCE_MODES, BULK_SYNC_MAX_MEMBERS, BoostRole, BulkLevelUp, BulkReport, DEFAULT_LEVELUP_MESSAGE, LevelChange, LevelChangeOutcome,
    ROLE_GIVE_MAX_MEMBERS, RoleSync, SyncOptions, XpChange, XpDelta, announce_mode_label, apply_xp_delta, boost_roles_json, end_bulk, fetch_member,
    find_user_level, get_rank, give_xp_to_members, handle_level_change, level_from_xp, level_roles, level_up_vars, normalize_boost_roles,
    ranked_rows, report_bulk_result, reset_xp, reward_pairs, run_bulk_role_sync, set_xp, subtract_xp, sync_level_roles, total_xp_for_level,
    try_start_bulk, xp_into_level,
};

const XP_MAX: i64 = bot_config::XP_MAX as i64;

/// `NOXP_CHANNEL_TYPES`.
pub const NOXP_CHANNEL_TYPES: [u8; 7] = [
    channel_type::GUILD_TEXT,
    channel_type::GUILD_ANNOUNCEMENT,
    channel_type::GUILD_VOICE,
    channel_type::GUILD_STAGE_VOICE,
    channel_type::GUILD_FORUM,
    channel_type::GUILD_MEDIA,
    channel_type::GUILD_CATEGORY,
];

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    let mut announce_mode = Opt::string("mode", "Where to announce").required(true);
    for mode in ANNOUNCE_MODES {
        announce_mode = announce_mode.choice(announce_mode_label(mode).unwrap_or(mode), mode);
    }
    SlashCommand::new("levelconfig", "Levels settings: XP, level-up messages, role rewards, rank card (MEE6 Levels).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("enable", "Turn the Levels module on."))
        .option(Opt::sub_command("disable", "Turn the Levels module off. XP data is kept."))
        .option(Opt::sub_command("status", "Show the current Levels configuration."))
        .option(
            Opt::sub_command("range", "Set the random XP per message (MEE6 default 15–25).")
                .option(Opt::integer("min", "Lowest XP per message").required(true).min_value(1).max_value(1000))
                .option(Opt::integer("max", "Highest XP per message").required(true).min_value(1).max_value(1000)),
        )
        .option(
            Opt::sub_command("cooldown", "Set the seconds between XP-earning messages (MEE6: 60).")
                .option(Opt::integer("seconds", "Cooldown in seconds").required(true).min_value(5).max_value(3600)),
        )
        .option(
            Opt::sub_command("multiplier", "Set the server-wide XP multiplier.")
                .option(Opt::number("value", "Multiplier, e.g. 2 = double XP").required(true).min_value(0.1).max_value(10)),
        )
        .option(
            Opt::sub_command("announce", "Choose where level-up messages are sent.")
                .option(announce_mode)
                .option(
                    Opt::channel("channel", "Channel for the Custom channel mode")
                        .channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]),
                )
                .option(new_channel_option("channel", "channel"))
                .option(Opt::boolean("embed", "Send as an embed instead of plain text")),
        )
        .option(
            Opt::sub_command("message", "Set the level-up message. Leave text empty to restore the default.")
                .option(Opt::string("text", "Supports {player} {level} {rank} {xp} {user.name} {server}").max_length(1000)),
        )
        .option(
            Opt::sub_command("voicexp", "Turn voice XP on or off and set the XP per minute.")
                .option(Opt::boolean("enabled", "Earn XP in voice channels").required(true))
                .option(Opt::integer("rate", "XP per minute").min_value(1).max_value(50)),
        )
        .option(
            Opt::sub_command("card", "Customise the /rank card colours and background.")
                .option(Opt::string("color", "Background colour, e.g. #2b2d31").max_length(16))
                .option(Opt::string("accent", "Accent colour (ring, level, bar)").max_length(16))
                .option(Opt::string("text_color", "Main text colour").max_length(16))
                .option(Opt::string("background_url", "Background image URL (https), or \"none\"").max_length(500))
                .option(Opt::number("overlay", "Darkening over the image, 0–0.9").min_value(0).max_value(0.9))
                .option(Opt::boolean("reset", "Restore the default card")),
        )
        .option(
            Opt::sub_command("demotion-sync", "Remove reward roles when a member drops below their level.")
                .option(Opt::boolean("enabled", "Remove roles on demotion").required(true)),
        )
        .option(
            Opt::sub_command_group("noxp", "Roles and channels that earn no XP.")
                .option(
                    Opt::sub_command("role", "Add or remove a no-XP role.")
                        .option(Opt::role("role", "Role").required(true))
                        .option(Opt::boolean("remove", "Remove it from the list")),
                )
                .option(
                    Opt::sub_command("channel", "Add or remove a no-XP channel or category.")
                        .option(Opt::channel("channel", "Channel or category").required(true).channel_types(&NOXP_CHANNEL_TYPES))
                        .option(Opt::boolean("remove", "Remove it from the list")),
                ),
        )
        .option(
            Opt::sub_command_group("role", "Level role rewards.")
                .option(
                    Opt::sub_command("add", "Give a role when members reach a level.")
                        .option(Opt::integer("level", "Level").required(true).min_value(1).max_value(999))
                        .option(Opt::role("role", "Reward role"))
                        .option(new_role_option("role"))
                        .option(Opt::boolean("sync", "Also give it to members already at this level (default: yes)")),
                )
                .option(
                    Opt::sub_command("remove", "Remove the reward for a level.")
                        .option(Opt::integer("level", "Level").required(true).min_value(1).autocomplete(true)),
                )
                .option(Opt::sub_command("list", "List the role rewards."))
                .option(
                    Opt::sub_command("stack", "Keep lower rewards (stack) or only the highest (replace).")
                        .option(Opt::boolean("enabled", "true = stack, false = replace").required(true)),
                )
                .option(
                    Opt::sub_command("sync", "Re-sync reward roles for one member or everyone ranked.").option(Opt::user("user", "Only this member")),
                ),
        )
        .option(
            Opt::sub_command_group("boost", "XP boost roles (the highest multiplier applies).")
                .option(
                    Opt::sub_command("add", "Add or update an XP boost role.")
                        .option(Opt::role("role", "Role").required(true))
                        .option(Opt::number("multiplier", "Boost, e.g. 1.5 = +50%").required(true).min_value(1.1).max_value(5)),
                )
                .option(Opt::sub_command("remove", "Remove an XP boost role.").option(Opt::role("role", "Role").required(true)))
                .option(Opt::sub_command("list", "List the XP boost roles.")),
        )
        .option(
            Opt::sub_command_group("xp", "Manage member XP (MEE6 /give-xp, /remove-xp).")
                .option(
                    Opt::sub_command("give", "Give XP to a member, or to every member of a role.")
                        .option(Opt::mentionable("target", "Member or role").required(true))
                        .option(Opt::integer("amount", "XP to give").required(true).min_value(1).max_value(1_000_000)),
                )
                .option(
                    Opt::sub_command("remove", "Remove XP from a member.")
                        .option(Opt::user("user", "Member").required(true))
                        .option(Opt::integer("amount", "XP to remove").required(true).min_value(1).max_value(1_000_000)),
                )
                .option(
                    Opt::sub_command("set", "Set a member's total XP.")
                        .option(Opt::user("user", "Member").required(true))
                        .option(Opt::integer("amount", "New total XP").required(true).min_value(0).max_value(1_000_000_000)),
                )
                .option(Opt::sub_command("reset", "Reset a member's XP and level to zero.").option(Opt::user("user", "Member").required(true)))
                .option(Opt::sub_command("view", "View a member's XP data.").option(Opt::user("user", "Member").required(true))),
        )
}

/// `/levelconfig` (`module 'leveling'`, `gated = false`: works while the module is off, so an admin can turn it on;
/// ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Leveling), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

/// A validation failure inside a settings mutator; `execute` turns it into an error reply and nothing is saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

async fn reply(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
}

async fn fail(ctx: &CommandCtx, title: &str, text: &str) {
    reply(ctx, error_embed(title, Some(text), None)).await;
}

/// `isExpiringDiscordLink(url)`: true for Discord CDN upload links, which are signed and stop working after a while.
/// Its static art (avatars, emoji) never expires.
pub fn is_expiring_discord_link(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else { return false };
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if host != "cdn.discordapp.com" && host != "media.discordapp.net" {
        return false;
    }
    let path = parsed.path().to_ascii_lowercase();
    path.starts_with("/attachments/") || path.starts_with("/ephemeral-attachments/")
}

/// `voice:start:{guildId}:*`.
pub fn voice_keys(guild_id: &str) -> String {
    format!("voice:start:{guild_id}:*")
}

fn role_line(role_id: &str) -> String {
    format!("<@&{role_id}>")
}

fn list_or(ids: &[String]) -> String {
    if ids.is_empty() { "none".into() } else { ids.iter().map(|i| role_line(i)).collect::<Vec<_>>().join(", ") }
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/levelconfig outside a guild"))
}

fn num(n: f64) -> String {
    number_to_string(n)
}

// ── Entry points ──────────────────────────────────────────────────────────────

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    // The XP management group answers privately; every other subcommand answers publicly.
    if group.as_deref() == Some("xp") {
        return execute_xp(ctx).await;
    }
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let sub = ctx.subcommand().unwrap_or_default();
    if !resolve_new_options(&mut ctx, NewOptions { required: &["role"], private: &[] }).await {
        return Ok(());
    }
    let result = match (group.as_deref(), sub.as_str()) {
        (Some("noxp"), "role") => sub_noxp_role(&ctx).await,
        (Some("noxp"), _) => sub_noxp_channel(&ctx).await,
        (Some("role"), "add") => sub_role_add(&ctx).await,
        (Some("role"), "remove") => sub_role_remove(&ctx).await,
        (Some("role"), "list") => sub_role_list(&ctx).await,
        (Some("role"), "stack") => sub_role_stack(&ctx).await,
        (Some("role"), "sync") => sub_role_sync(&ctx).await,
        (Some("boost"), "add") => sub_boost_add(&ctx).await,
        (Some("boost"), "remove") => sub_boost_remove(&ctx).await,
        (Some("boost"), "list") => sub_boost_list(&ctx).await,
        (Some(_), _) => Ok(()),
        (None, "enable") => sub_enable(&ctx).await,
        (None, "disable") => sub_disable(&ctx).await,
        (None, "status") => sub_status(&ctx).await,
        (None, "range") => sub_range(&ctx).await,
        (None, "cooldown") => sub_cooldown(&ctx).await,
        (None, "multiplier") => sub_multiplier(&ctx).await,
        (None, "announce") => sub_announce(&ctx).await,
        (None, "message") => sub_message(&ctx).await,
        (None, "voicexp") => sub_voice_xp(&ctx).await,
        (None, "card") => sub_card(&ctx).await,
        (None, "demotion-sync") => sub_demotion_sync(&ctx).await,
        (None, _) => Ok(()),
    };
    match result {
        Err(e) if e.downcast_ref::<ConfigError>().is_some() => {
            let msg = e.downcast_ref::<ConfigError>().map(|c| c.0.clone()).unwrap_or_default();
            reply(&ctx, error_embed("Not Saved", Some(&msg), None)).await;
            Ok(())
        }
        other => other,
    }
}

/// The `/levelconfig role remove level:` autocomplete: the configured reward levels starting with what was typed.
async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let typed = bot_util::js::trim(&ctx.focused_value()).to_string();
    let Some(guild_id) = ctx.guild_id() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    // Rule 21: the answer is due in 3 s, so the lookup is bounded; a timeout answers with no choices.
    let (app, g) = (ctx.app.clone(), guild_id.to_string());
    let rows = bot_util::async_util::with_timeout(
        async move { level_roles(app.db(), &g).await.map(|r| r.into_iter().take(bot_config::limits::LEVEL_ROLES).collect::<Vec<_>>()) },
        bot_core::interaction::ack_budget_ms(ctx.interaction.id),
        "levelconfig autocomplete",
    )
    .await
    .unwrap_or_default();
    let guild = cached_guild(&ctx.app, guild_id);
    // An integer column cannot use a database `contains`, so the prefix filter runs here.
    let choices: Vec<Choice> = rows
        .iter()
        .filter(|r| r.level.to_string().starts_with(&typed))
        .take(25)
        .map(|r| {
            let name = guild
                .as_ref()
                .and_then(|g| r.role_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|id| g.roles.get(&RoleId::new(id))).map(|role| role.name.clone()))
                .unwrap_or_else(|| "deleted role".into());
            Choice::new(format!("Level {} → {name}", r.level), json!(r.level))
        })
        .collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

// ── Module and XP settings ────────────────────────────────────────────────────

async fn sub_enable(ctx: &CommandCtx) -> BotResult {
    let s = ctx.app.settings().update_settings(&guild_of(ctx)?.to_string(), SettingsPatch::new().set("xpEnabled", true)).await?;
    let text = format!(
        "Members now earn **{}–{} XP** per message (once every {}s). Configure with `/levelconfig`.",
        s.xp_min, s.xp_max, s.xp_cooldown
    );
    reply(ctx, success_embed("Levels Enabled", Some(&text), None)).await;
    Ok(())
}

async fn sub_disable(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?.to_string();
    ctx.app.settings().update_settings(&g, SettingsPatch::new().set("xpEnabled", false)).await?;
    // Open voice sessions are dropped without credit, so nothing is paid after the module comes back on.
    ctx.app.cache().invalidate(&voice_keys(&g)).await;
    reply(ctx, success_embed("Levels Disabled", Some("XP is no longer granted. Existing XP and rewards are kept."), None)).await;
    Ok(())
}

async fn sub_range(ctx: &CommandCtx) -> BotResult {
    let min = ctx.get_integer("min").unwrap_or(0);
    let max = ctx.get_integer("max").unwrap_or(0);
    if min > max {
        fail(ctx, "Invalid Range", "`min` must be less than or equal to `max`.").await;
        return Ok(());
    }
    let s = ctx.app.settings().update_settings(&guild_of(ctx)?.to_string(), SettingsPatch::new().set("xpMin", min).set("xpMax", max)).await?;
    let text = format!("Members earn **{}–{} XP** per message (before multipliers). MEE6 uses 15–25.", s.xp_min, s.xp_max);
    reply(ctx, success_embed("XP Range Updated", Some(&text), None)).await;
    Ok(())
}

async fn sub_cooldown(ctx: &CommandCtx) -> BotResult {
    let seconds = ctx.get_integer("seconds").unwrap_or(60);
    let s = ctx.app.settings().update_settings(&guild_of(ctx)?.to_string(), SettingsPatch::new().set("xpCooldown", seconds)).await?;
    reply(ctx, success_embed("Cooldown Updated", Some(&format!("Members can earn XP once every **{}s**.", s.xp_cooldown)), None)).await;
    Ok(())
}

/// `Math.round(v * 100) / 100`.
pub fn round2(v: f64) -> f64 {
    bot_util::text::js_round(v * 100.0) / 100.0
}

async fn sub_multiplier(ctx: &CommandCtx) -> BotResult {
    let value = round2(ctx.get_number("value").unwrap_or(1.0));
    let s = ctx
        .app
        .settings()
        .update_settings(&guild_of(ctx)?.to_string(), SettingsPatch::new().set("xpMultiplier", bot_core::command::js_number(value)))
        .await?;
    reply(ctx, success_embed("Multiplier Updated", Some(&format!("Server-wide XP multiplier: **×{}**.", num(s.xp_multiplier))), None)).await;
    Ok(())
}

async fn sub_voice_xp(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?.to_string();
    let enabled = ctx.get_boolean("enabled").unwrap_or(false);
    let mut patch = SettingsPatch::new().set("voiceXpEnabled", enabled);
    if let Some(rate) = ctx.get_integer("rate") {
        patch = patch.set("voiceXpRate", rate);
    }
    let s = ctx.app.settings().update_settings(&g, patch).await?;
    if !enabled {
        ctx.app.cache().invalidate(&voice_keys(&g)).await; // drop open sessions without credit
    }
    let mut text = if enabled {
        format!(
            "Voice XP **on** at **{} XP/minute**. No XP while deafened, alone, in the AFK channel or in a no-XP channel.",
            s.voice_xp_rate
        )
    } else {
        "Voice XP **off**.".to_string()
    };
    if !s.xp_enabled {
        text.push_str("\n⚠️ The Levels module is off; enable it with `/levelconfig enable`.");
    }
    reply(ctx, success_embed("Voice XP Updated", Some(&text), None)).await;
    Ok(())
}

async fn sub_demotion_sync(ctx: &CommandCtx) -> BotResult {
    let enabled = ctx.get_boolean("enabled").unwrap_or(false);
    ctx.app.settings().update_settings(&guild_of(ctx)?.to_string(), SettingsPatch::new().set("levelRolesSyncOnDemotion", enabled)).await?;
    let text = if enabled {
        "Members who lose XP (`/levelconfig xp remove`, `xp set`, `xp reset`) also lose reward roles above their new level."
    } else {
        "Reward roles are kept when members lose XP."
    };
    reply(ctx, success_embed("Demotion Sync Updated", Some(text), None)).await;
    Ok(())
}

// ── Status ────────────────────────────────────────────────────────────────────

/// `prisma.levelRole.count({ where: { guildId } })`.
pub async fn level_role_count(pool: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let row = bot_db::sqlx::query(r#"SELECT COUNT(*)::bigint AS "n" FROM "LevelRole" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(pool).await?;
    Ok(bot_db::sqlx::Row::try_get::<i64, _>(&row, "n")?)
}

/// The `/levelconfig status` fields: `(name, value, inline)`.
pub fn status_fields(s: &GuildSettings, reward_count: i64, channel_exists: bool) -> Vec<(String, String, bool)> {
    let boosts = normalize_boost_roles(s.xp_boost_roles.as_ref());
    let style = normalize_card_style(s.rank_card_style.as_ref().unwrap_or(&Value::Null));
    let mode = s.level_up_mode.as_str();
    let mut announce = announce_mode_label(mode).unwrap_or("Current channel").to_string();
    if mode == "CHANNEL" {
        match s.level_up_channel_id.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => announce.push_str(&format!(" → <#{c}>{}", if channel_exists { "" } else { " ⚠️ missing" })),
            None => announce.push_str(" ⚠️ no channel set"),
        }
    }
    announce.push_str(if s.level_up_use_embed { " · embed" } else { " · text" });
    let message = clamp_text(&s.level_up_message.as_deref().unwrap_or(DEFAULT_LEVELUP_MESSAGE).replace("```", "ˋˋˋ"), 950);
    let join = |items: Vec<String>| {
        let joined = join_with_limit(&items, ", ", 900, default_more);
        if joined.is_empty() { "None".to_string() } else { joined }
    };
    vec![
        ("Module".into(), if s.xp_enabled { "✅ On".into() } else { "❌ Off — /levelconfig enable".into() }, false),
        ("XP per message".into(), format!("{}–{}", s.xp_min, s.xp_max), true),
        ("Cooldown".into(), format!("{}s", s.xp_cooldown), true),
        ("Multiplier".into(), format!("×{}", num(s.xp_multiplier)), true),
        ("Announcements".into(), announce, false),
        ("Voice XP".into(), if s.voice_xp_enabled { format!("✅ {} XP/min", s.voice_xp_rate) } else { "❌ Off".into() }, true),
        (
            "Role rewards".into(),
            format!(
                "{reward_count} configured · {} · demotion sync {}",
                if s.level_roles_stack { "Stack" } else { "Replace" },
                if s.level_roles_sync_on_demotion { "on" } else { "off" }
            ),
            false,
        ),
        ("XP boost roles".into(), join(boosts.iter().map(|b| format!("{} ×{}", role_line(&b.role_id), num(b.multiplier))).collect()), false),
        ("No-XP roles".into(), join(s.no_xp_role_ids.iter().map(|r| role_line(r)).collect()), false),
        ("No-XP channels".into(), join(s.no_xp_channel_ids.iter().map(|c| format!("<#{c}>")).collect()), false),
        (
            "Rank card".into(),
            format!(
                "background {}{} · accent {} · text {}",
                style.background,
                if style.background_url.is_some() { " + image" } else { "" },
                style.accent_color,
                style.text_color
            ),
            false,
        ),
        (
            format!("Level-up message{}", if s.level_up_message.as_deref().is_some_and(|m| !m.is_empty()) { "" } else { " (default)" }),
            format!("```\n{message}\n```"),
            false,
        ),
    ]
}

async fn sub_status(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let (s, count) = tokio::join!(ctx.app.settings().get_settings(&g), level_role_count(ctx.app.db(), &g));
    let (s, count) = (s?, count?);
    let exists = s
        .level_up_channel_id
        .as_deref()
        .and_then(|c| c.parse::<u64>().ok().filter(|v| *v != 0))
        .is_some_and(|c| cached_channel(&ctx.app, guild_id, ChannelId::new(c)).is_some());
    let mut embed = guild_embed(Some(&s), Some("⭐ Levels Configuration"), None, None);
    for (name, value, inline) in status_fields(&s, count, exists) {
        embed = embed.field(name, value, inline);
    }
    reply(ctx, embed).await;
    Ok(())
}

// ── Level-up messages ─────────────────────────────────────────────────────────

async fn sub_announce(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let mode = ctx.get_string("mode").unwrap_or_default();
    let channel = ctx.get_channel_id("channel");
    let embed = ctx.get_boolean("embed"); // None = unchanged
    let settings = ctx.app.settings().get_settings(&g).await?;

    if channel.is_some() && mode != "CHANNEL" {
        fail(ctx, "Not Saved", "The `channel` option is only used with **Custom channel** mode.").await;
        return Ok(());
    }
    let mut patch = SettingsPatch::new().set("levelUpMode", mode.clone());
    if mode == "CHANNEL" {
        let target = channel.or_else(|| {
            settings
                .level_up_channel_id
                .as_deref()
                .and_then(|c| c.parse::<u64>().ok().filter(|v| *v != 0))
                .map(ChannelId::new)
                .filter(|c| cached_channel(&ctx.app, guild_id, *c).is_some())
        });
        let Some(target) = target else {
            fail(ctx, "Channel Required", "Pick a channel for **Custom channel** mode.").await;
            return Ok(());
        };
        let needs: &[&str] =
            if embed.unwrap_or(settings.level_up_use_embed) { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] };
        let usable = check_channel_usable(&ctx.app, guild_id, Some(target), needs);
        if !usable.ok {
            let why = usable.reason.clone().filter(|r| !r.is_empty()).unwrap_or_else(|| format!("missing {}", usable.missing.join(", ")));
            fail(ctx, "Missing Permissions", &format!("I can't post in <#{target}>: {why}.")).await;
            return Ok(());
        }
        patch = patch.set("levelUpChannelId", target.to_string()); // other modes leave the channel alone
    }
    if let Some(e) = embed {
        patch = patch.set("levelUpUseEmbed", e);
    }
    let s = ctx.app.settings().update_settings(&g, patch).await?;
    let m = s.level_up_mode.as_str();
    let wher = match s.level_up_channel_id.as_deref().filter(|c| !c.is_empty()) {
        Some(c) if m == "CHANNEL" => format!(" → <#{c}>"),
        _ => String::new(),
    };
    let text = format!(
        "Mode: **{}**{wher}. Format: **{}**.",
        announce_mode_label(m).unwrap_or("undefined"),
        if s.level_up_use_embed { "embed" } else { "plain text" }
    );
    reply(ctx, success_embed("Level-up Messages", Some(&text), None)).await;
    Ok(())
}

/// The 'Level variables' help field of `/levelconfig message`.
pub const LEVEL_VARIABLES_HELP: &str = "`{player}` mention · `{player.name}` display name · `{level}` new level · `{rank}` leaderboard position · `{xp}` total XP · `{mention}` (old alias). All `{user.*}`, `{server.*}`, `{channel.*}` and `{random}` variables also work.";

async fn sub_message(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let Some(text) = ctx.get_string("text") else {
        ctx.app.settings().update_settings(&g, SettingsPatch::new().set("levelUpMessage", Value::Null)).await?;
        let msg = format!("Restored the MEE6 default: `{DEFAULT_LEVELUP_MESSAGE}`");
        reply(ctx, success_embed("Level-up Message Reset", Some(&msg), None)).await;
        return Ok(());
    };
    let trimmed = bot_util::js::trim(&text).to_string();
    if trimmed.is_empty() {
        fail(ctx, "Not Saved", "The message can't be empty.").await;
        return Ok(());
    }
    let s = ctx.app.settings().update_settings(&g, SettingsPatch::new().set("levelUpMessage", trimmed.clone())).await?;

    let mut rctx = render_context(&ctx.app, Some(guild_id), ctx.user(), ctx.member(), Some(ctx.channel_id()));
    let name = svc::display_name(ctx.member(), ctx.user());
    rctx.vars = Some(level_up_vars(&ctx.user().id.to_string(), &name, 5, 1150.0, Some(1)));
    let preview = render_placeholders_detailed(&trimmed, &rctx, Some(2000));
    let mut description = format!("Preview (sample values):\n{}", clamp_text(&preview.text, 3500));
    if s.level_up_mode.as_str() == "OFF" {
        description.push_str("\n\nAnnouncements are currently **disabled**; turn them on with `/levelconfig announce`.");
    }
    let mut embed = success_embed("Level-up Message Updated", Some(&description), None);
    if !preview.unknown_tokens.is_empty() {
        let tokens: Vec<String> = preview.unknown_tokens.iter().map(|t| format!("`{t}`")).collect();
        let value = format!("{}\nThey will appear exactly as typed.", join_with_limit(&tokens, ", ", 900, default_more));
        embed = embed.field("⚠️ Unknown variables", value, false);
    }
    embed = embed.field("Level variables", LEVEL_VARIABLES_HELP, false);
    reply(ctx, embed).await;
    Ok(())
}

// ── Rank card ─────────────────────────────────────────────────────────────────

async fn card_preview(ctx: &CommandCtx, s: &GuildSettings) -> Option<CreateAttachment> {
    let guild_id = ctx.guild_id()?;
    let g = guild_id.to_string();
    let user = ctx.user().clone();
    let u = user.id.to_string();
    let style = normalize_card_style(s.rank_card_style.as_ref().unwrap_or(&Value::Null));
    let (mut level, mut total_xp, mut rank) = (5i64, 1150i64, 1i64);
    let inner = async {
        if let Some(row) = find_user_level(ctx.app.db(), &g, &u).await?.filter(|r| r.xp > 0) {
            total_xp = i64::from(row.xp);
            level = level_from_xp(total_xp as f64);
            rank = get_rank(ctx.app.db(), &g, &u, total_xp).await?;
        }
        let url = match ctx.member() {
            Some(m) => member_display_avatar_url(guild_id, m),
            None => user_display_avatar_url(&user),
        };
        let avatar = fetch_avatar(url).await;
        let background = match style.background_url.as_deref() {
            Some(bg) => load_card_background(Some(bg)).await,
            None => None,
        };
        draw_card(CardData {
            avatar,
            username: user.name.clone(),
            display_name: svc::display_name(ctx.member(), &user),
            level,
            total_xp,
            rank,
            xp_current: xp_into_level(total_xp as f64),
            xp_needed: bot_config::xp_for_level(level) as f64,
            style: style.clone(),
            background,
        })
        .await
    };
    match inner.await {
        Ok(png) => Some(CreateAttachment::bytes(png, "rank-preview.png")),
        Err(e) => {
            bot_util::log_warn!(format!("levelconfig: could not render the rank card preview: {e}"));
            None
        }
    }
}

/// `styleSummary(style)`.
pub fn style_summary(style: &bot_cards::RankCardStyle) -> String {
    [
        format!("**Background:** {}", style.background),
        format!("**Accent:** {}", style.accent_color),
        format!("**Text:** {}", style.text_color),
        format!("**Image:** {}", style.background_url.as_deref().unwrap_or("none")),
        format!("**Overlay:** {}", num(style.overlay_opacity)),
    ]
    .join("\n")
}

async fn reply_with_card(ctx: &CommandCtx, embed: CreateEmbed, s: &GuildSettings) {
    match card_preview(ctx, s).await {
        None => reply(ctx, embed).await,
        Some(file) => {
            safe_reply(&ctx.responder, Payload::embed(embed.image("attachment://rank-preview.png")).file(file)).await;
        }
    }
}

async fn sub_card(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?.to_string();
    let color = ctx.get_string("color");
    let accent = ctx.get_string("accent");
    let text = ctx.get_string("text_color");
    let url = ctx.get_string("background_url");
    let overlay = ctx.get_number("overlay");
    let reset = ctx.get_boolean("reset") == Some(true);
    let any_change = color.is_some() || accent.is_some() || text.is_some() || url.is_some() || overlay.is_some();
    let settings = ctx.app.settings().get_settings(&g).await?;
    let style_of = |s: &GuildSettings| normalize_card_style(s.rank_card_style.as_ref().unwrap_or(&Value::Null));

    // 1. Nothing given: show the current card.
    if !reset && !any_change {
        let embed = apply_guild_color(info_embed("Rank Card", Some(&style_summary(&style_of(&settings))), None), Some(&settings));
        reply_with_card(ctx, embed, &settings).await;
        return Ok(());
    }

    // 2. Reset (any other option given alongside is ignored).
    if reset {
        let old_url = style_of(&settings).background_url;
        let s = ctx.app.settings().update_settings(&g, SettingsPatch::new().set("rankCardStyle", Value::Null)).await?;
        clear_card_background(old_url.as_deref());
        let note = if any_change { "\nThe other options were ignored because `reset` was set." } else { "" };
        reply_with_card(ctx, success_embed("Rank Card Reset", Some(&format!("Restored the default card.{note}")), None), &s).await;
        return Ok(());
    }

    // 3. Colours.
    let mut changes: Vec<(&str, Value)> = Vec::new();
    for (key, value) in [("background", &color), ("accentColor", &accent), ("textColor", &text)] {
        let Some(value) = value else { continue };
        match parse_color_str(value) {
            Some(n) => changes.push((key, json!(format_color(&json!(n))))),
            None => {
                let msg = format!("`{}` isn't a colour. Use a hex code like #5865F2 or a name like blurple.", clamp_text(value, 40));
                fail(ctx, "Invalid Colour", &msg).await;
                return Ok(());
            }
        }
    }
    if let Some(o) = overlay {
        changes.push(("overlayOpacity", bot_core::command::js_number(round2(o))));
    }

    // 4. Background image.
    let mut warning = "";
    if let Some(raw) = &url {
        let u = bot_util::js::trim(raw).to_string();
        if ["none", "clear", "remove"].contains(&u.to_lowercase().as_str()) {
            changes.push(("backgroundUrl", Value::Null));
        } else {
            if !is_http_url(&u) {
                fail(ctx, "Invalid URL", "Use a direct https link to a PNG, JPG, GIF or WebP image.").await;
                return Ok(());
            }
            if let BackgroundCheck::Invalid { reason } = validate_card_background(&u).await {
                fail(ctx, "Image Not Loaded", &format!("I couldn't load an image from that link ({reason}).")).await;
                return Ok(());
            }
            if is_expiring_discord_link(&u) {
                warning = "\n⚠️ Discord attachment links expire after about a day. Use a permanent image host.";
            }
            changes.push(("backgroundUrl", json!(u)));
        }
    }

    // 5. Write. The URL was validated above, so the mutator stays pure.
    let old_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot = old_url.clone();
    let s = ctx
        .app
        .settings()
        .update_settings_with(&g, move |cur| {
            let before = normalize_card_style(cur.rank_card_style.as_ref().unwrap_or(&Value::Null));
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = before.background_url.clone();
            Ok(SettingsPatch::new().set("rankCardStyle", merged_style(&before, &changes)))
        })
        .await?;
    let old = old_url.lock().unwrap_or_else(|e| e.into_inner()).clone();
    clear_card_background(old.as_deref());
    clear_card_background(style_of(&s).background_url.as_deref());

    // 6. Preview.
    let summary = format!("{}{warning}", style_summary(&style_of(&s)));
    reply_with_card(ctx, success_embed("Rank Card Updated", Some(&summary), None), &s).await;
    Ok(())
}

/// `{ ...before, ...changes }`: the stored card style after an edit (the JS key order).
pub fn merged_style(before: &bot_cards::RankCardStyle, changes: &[(&str, Value)]) -> Value {
    let mut obj = before.to_json();
    obj["overlayOpacity"] = bot_core::command::js_number(before.overlay_opacity);
    if let Value::Object(map) = &mut obj {
        for (k, v) in changes {
            map.insert((*k).to_string(), v.clone());
        }
    }
    obj
}

// ── No-XP roles and channels ─────────────────────────────────────────────────

/// The pure mutator of a no-XP list edit (`noxpMutator`). `exists` prunes entries whose role or channel is gone; it is
/// passed only while the guild is available, because an unavailable guild has empty caches and a cache miss is never
/// a deletion.
pub fn noxp_patch(current: &[String], field: &str, id: &str, remove: bool, exists: Option<&HashSet<String>>, cap: usize, noun: &str) -> anyhow::Result<SettingsPatch> {
    let mut list: Vec<String> = current.to_vec();
    if let Some(e) = exists {
        list.retain(|x| e.contains(x));
    }
    if remove {
        if !list.iter().any(|x| x == id) {
            anyhow::bail!(ConfigError(format!("That {noun} is not on the no-XP list.")));
        }
        list.retain(|x| x != id);
        return Ok(SettingsPatch::new().set(field, json!(list)));
    }
    if list.iter().any(|x| x == id) {
        anyhow::bail!(ConfigError("Already on the no-XP list.".into()));
    }
    if list.len() >= cap {
        anyhow::bail!(ConfigError(format!("You can have at most {cap} no-XP {noun}s.")));
    }
    list.push(id.to_string());
    Ok(SettingsPatch::new().set(field, json!(list)))
}

async fn sub_noxp_role(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let role = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?.to_string();
    let remove = ctx.get_boolean("remove").unwrap_or(false);
    if !remove && role == g {
        fail(ctx, "Not Saved", "Use `/levelconfig disable` to stop XP for everyone.").await;
        return Ok(());
    }
    let exists: Option<HashSet<String>> = cached_guild(&ctx.app, guild_id).map(|gd| gd.roles.keys().map(|r| r.to_string()).collect());
    let id = role.clone();
    ctx.app
        .settings()
        .update_settings_with(&g, move |cur| {
            noxp_patch(&cur.no_xp_role_ids, "noXpRoleIds", &id, remove, exists.as_ref(), bot_config::limits::NOXP_ROLES, "role")
        })
        .await?;
    let text = format!("{} {} the no-XP roles.", role_line(&role), if remove { "removed from" } else { "added to" });
    reply(ctx, success_embed("No-XP Roles", Some(&text), None)).await;
    Ok(())
}

async fn sub_noxp_channel(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let channel = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?.to_string();
    let remove = ctx.get_boolean("remove").unwrap_or(false);
    let exists: Option<HashSet<String>> =
        cached_guild(&ctx.app, guild_id).map(|gd| gd.channels.keys().map(|c| c.to_string()).chain(gd.threads.iter().map(|t| t.id.to_string())).collect());
    let id = channel.clone();
    ctx.app
        .settings()
        .update_settings_with(&g, move |cur| {
            noxp_patch(&cur.no_xp_channel_ids, "noXpChannelIds", &id, remove, exists.as_ref(), bot_config::limits::NOXP_CHANNELS, "channel")
        })
        .await?;
    let text = if remove {
        format!("<#{channel}> removed from the no-XP channels.")
    } else {
        format!("<#{channel}> added to the no-XP channels. Threads, forum posts and channels inside a no-XP category earn no XP either.")
    };
    reply(ctx, success_embed("No-XP Channels", Some(&text), None)).await;
    Ok(())
}

// ── Role rewards ──────────────────────────────────────────────────────────────

fn bulk_report(ctx: &CommandCtx, guild_id: GuildId) -> BulkReport {
    BulkReport {
        responder: ctx.responder.clone(),
        guild_id,
        channel_id: ctx.channel_id(),
        created_ms: bot_core::interaction::created_ms(ctx.interaction.id) as i64,
    }
}

/// Releases the guild's bulk lock when dropped (the JS `finally { endBulk() }`).
struct BulkGuard {
    app: App,
    guild_id: String,
    armed: bool,
}

impl Drop for BulkGuard {
    fn drop(&mut self) {
        if self.armed {
            end_bulk(&self.app, &self.guild_id);
        }
    }
}

/// `runBulkInBackground(...)`: runs a role-sync task in the background (the caller already took the bulk lock) and
/// always releases the lock. `rows` are `(userId, xp)`.
pub fn run_bulk_in_background<F>(app: &App, report: BulkReport, label: String, load: F, settings: Settings, sync_down: bool) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = anyhow::Result<Vec<(String, i64)>>> + Send + 'static,
{
    let app = app.clone();
    tokio::spawn(async move {
        let _guard = BulkGuard { app: app.clone(), guild_id: report.guild_id.to_string(), armed: true };
        match load.await {
            Ok(rows) => {
                let entries: Vec<(String, i64)> = rows.into_iter().map(|(u, xp)| (u, level_from_xp(xp as f64))).collect();
                let r = run_bulk_role_sync(&app, report.guild_id, &entries, Some(&settings), sync_down).await;
                let text = format!("{label}: {} updated, {} failed, {} no longer in the server.", r.changed, r.failed, r.missing);
                report_bulk_result(&app, &report, &text, false).await;
            }
            Err(e) => bot_util::log_error!(format!("levels: {label} failed"), bot_core::error::log_arg(&e)),
        }
    })
}

/// `prisma.levelRole.findUnique({ where: { guildId_level } })`.
async fn reward_at(pool: &bot_db::PgPool, guild_id: &str, level: i64) -> anyhow::Result<Option<LevelRole>> {
    let q = bot_db::SelectBuilder::new(&LEVEL_ROLE_INFO).filter("guildId", json!(guild_id))?.filter("level", json!(level))?.build()?;
    Ok(q.fetch_optional::<LevelRole, _>(pool).await?)
}

/// `prisma.levelRole.findFirst({ where: { guildId, roleId, NOT: { level } } })`.
async fn reward_of_role_elsewhere(pool: &bot_db::PgPool, guild_id: &str, role_id: &str, level: i64) -> anyhow::Result<Option<LevelRole>> {
    let row = bot_db::sqlx::query_as::<_, LevelRole>(r#"SELECT * FROM "LevelRole" WHERE "guildId" = $1 AND "roleId" = $2 AND "level" <> $3 LIMIT 1"#)
        .bind(guild_id)
        .bind(role_id)
        .bind(level.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// `prisma.levelRole.upsert(...)`: sets the reward of a level (after `ensureGuildSettings`).
pub async fn upsert_reward(app: &App, guild_id: &str, level: i64, role_id: &str) -> anyhow::Result<LevelRole> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let q = InsertBuilder::new(&LEVEL_ROLE_INFO)
        .values([("guildId", json!(guild_id)), ("level", json!(level)), ("roleId", json!(role_id))])?
        .on_conflict_update(&["guildId", "level"], [("roleId", json!(role_id))])?
        .build()?;
    Ok(q.fetch_one::<LevelRole, _>(app.db()).await?)
}

async fn sub_role_add(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let level = ctx.get_integer("level").unwrap_or(1);
    let role = ctx.get_role("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let role_id = role.id.to_string();
    let sync = ctx.get_boolean("sync").unwrap_or(true);

    if total_xp_for_level(level as f64) > XP_MAX as f64 {
        let text = format!(
            "Level {level} needs more than {} XP, so nobody can reach it. The highest reachable level is {}.",
            format_number(XP_MAX as f64),
            level_from_xp(XP_MAX as f64)
        );
        fail(ctx, "Level Too High", &text).await;
        return Ok(());
    }
    let check = match (ctx.app.discord(), cached_guild(&ctx.app, guild_id)) {
        (Some(d), Some(guild)) => check_assignable_role(&guild, Some(&role), d.cache.current_user().id, ctx.member(), true),
        _ => bot_core::interaction_helpers::RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() },
    };
    if !check.ok {
        fail(ctx, "Can't Use This Role", check.reason.as_deref().unwrap_or_default()).await;
        return Ok(());
    }
    if let Some(other) = reward_of_role_elsewhere(ctx.app.db(), &g, &role_id, level).await? {
        let text = format!("{} is already the reward for level {}. Remove that first.", role_line(&role_id), other.level);
        fail(ctx, "Already a Reward", &text).await;
        return Ok(());
    }
    let existing = reward_at(ctx.app.db(), &g, level).await?;
    if existing.is_none() && level_role_count(ctx.app.db(), &g).await? >= bot_config::limits::LEVEL_ROLES as i64 {
        fail(ctx, "Limit Reached", &format!("You can have at most {} role rewards.", bot_config::limits::LEVEL_ROLES)).await;
        return Ok(());
    }
    upsert_reward(&ctx.app, &g, level, &role_id).await?;
    let s = ctx.app.settings().get_settings(&g).await?;

    let mut text = format!("Members who reach level **{level}** get {}.", role_line(&role_id));
    if let Some(e) = existing.as_ref().filter(|e| e.role_id != role_id) {
        text.push_str(&format!(" (Replaces {}.)", role_line(&e.role_id)));
    }
    if !check.dangerous.is_empty() {
        text.push_str(&format!("\n⚠️ This role grants: {}.", check.dangerous.join(", ")));
    }
    let mut lock = false;
    if sync {
        lock = try_start_bulk(&ctx.app, &g);
        text.push_str(&if lock {
            format!("\nGiving it to members already at level {level}+ in the background…")
        } else {
            "\nA role sync is already running; run `/levelconfig role sync` when it finishes.".to_string()
        });
    }
    if safe_reply(&ctx.responder, Payload::embed(success_embed("Role Reward Added", Some(&text), None))).await.is_none() {
        if lock {
            end_bulk(&ctx.app, &g);
        }
        return Ok(());
    }
    if lock {
        let pool = ctx.app.db().clone();
        let min = total_xp_for_level(level as f64) as i64;
        let gid = g.clone();
        run_bulk_in_background(
            &ctx.app,
            bulk_report(ctx, guild_id),
            format!("Role sync for level {level}"),
            async move { ranked_rows(&pool, &gid, None, Some(min), BULK_SYNC_MAX_MEMBERS).await },
            s,
            false,
        );
    }
    Ok(())
}

async fn sub_role_remove(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?.to_string();
    let level = ctx.get_integer("level").unwrap_or(0);
    let Some(row) = reward_at(ctx.app.db(), &g, level).await? else {
        fail(ctx, "Not Found", &format!("No role reward is set for level {level}.")).await;
        return Ok(());
    };
    bot_db::delete_where(&LEVEL_ROLE_INFO, [("guildId", json!(g)), ("level", json!(level))])?.execute(ctx.app.db()).await?;
    let text = format!("Removed the level **{level}** reward ({}). Members who already have the role keep it.", role_line(&row.role_id));
    reply(ctx, success_embed("Role Reward Removed", Some(&text), None)).await;
    Ok(())
}

/// One line of `/levelconfig role list`: `problem` is `None` when the role is fine, `Some(None)` when it was deleted,
/// `Some(Some(reason))` when the bot cannot manage it.
pub fn reward_line(level: i64, role_id: &str, problem: Option<Option<&str>>) -> String {
    let mut line = format!("**Level {level}** → {}", role_line(role_id));
    match problem {
        Some(None) => line.push_str(" ⚠️ deleted role"),
        Some(Some(reason)) => line.push_str(&format!(" ⚠️ {reason}")),
        None => {}
    }
    line
}

async fn sub_role_list(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let (s, rows) = tokio::join!(ctx.app.settings().get_settings(&g), level_roles(ctx.app.db(), &g));
    let (s, rows) = (s?, rows?);
    if rows.is_empty() {
        reply(ctx, info_embed("No Role Rewards", Some("Add one with `/levelconfig role add`."), None)).await;
        return Ok(());
    }
    let guild = cached_guild(&ctx.app, guild_id);
    let bot = ctx.app.discord().map(|d| d.cache.current_user().id);
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            let role = guild.as_ref().and_then(|gd| r.role_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|id| gd.roles.get(&RoleId::new(id))));
            match (role, guild.as_ref(), bot) {
                (Some(role), Some(gd), Some(bot)) => {
                    let check = check_assignable_role(gd, Some(role), bot, None, true);
                    reward_line(i64::from(r.level), &r.role_id, if check.ok { None } else { Some(check.reason.as_deref()) })
                }
                _ => reward_line(i64::from(r.level), &r.role_id, Some(None)),
            }
        })
        .collect();
    let footer = format!("{} reward(s) · {} mode", rows.len(), if s.level_roles_stack { "Stack" } else { "Replace" });
    let pages: Vec<CreateEmbed> =
        lines.chunks(25).map(|chunk| guild_embed(Some(&s), Some("🎁 Level Role Rewards"), Some(&chunk.join("\n")), Some(&footer))).collect();
    if pages.len() == 1 {
        reply(ctx, pages[0].clone()).await;
        return Ok(());
    }
    paginate(ctx, &pages, PaginateOptions::default()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

async fn sub_role_stack(ctx: &CommandCtx) -> BotResult {
    let enabled = ctx.get_boolean("enabled").unwrap_or(true);
    ctx.app.settings().update_settings(&guild_of(ctx)?.to_string(), SettingsPatch::new().set("levelRolesStack", enabled)).await?;
    let what = if enabled { "Members **keep** every reward role they earn." } else { "Members keep **only the highest** reward role they earned." };
    let text = format!("{what} Run `/levelconfig role sync` to apply this to existing members.");
    reply(ctx, success_embed("Role Reward Mode", Some(&text), None)).await;
    Ok(())
}

async fn sub_role_sync(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let s = ctx.app.settings().get_settings(&g).await?;

    if let Some(user) = ctx.get_user("user") {
        let Some(member) = fetch_member(&ctx.app, guild_id, user.id).await else {
            fail(ctx, "Not a Member", "That user isn't in this server.").await;
            return Ok(());
        };
        if user.bot {
            fail(ctx, "Not Saved", "Bots don't have levels.").await;
            return Ok(());
        }
        let row = find_user_level(ctx.app.db(), &g, &user.id.to_string()).await?;
        let level = row.map_or(0, |r| level_from_xp(f64::from(r.xp)));
        let opts = SyncOptions { sync_down: s.level_roles_sync_on_demotion, reason: "Level role sync".into(), ..Default::default() };
        let r = sync_level_roles(&ctx.app, guild_id, &member, level, Some(&s), opts).await;
        let mut text = format!("<@{}> (level {level}): added {}, removed {}.", user.id, list_or(&r.added), list_or(&r.removed));
        if !r.failed.is_empty() {
            let failed: Vec<String> = r.failed.iter().map(|(id, why)| format!("{} ({})", role_line(id), clamp_text(why, 120))).collect();
            text.push_str(&format!("\nFailed: {}", failed.join(", ")));
        }
        reply(ctx, success_embed("Roles Synced", Some(&text), None)).await;
        return Ok(());
    }

    if level_role_count(ctx.app.db(), &g).await? == 0 {
        reply(ctx, info_embed("No Role Rewards", Some("Add one with `/levelconfig role add`."), None)).await;
        return Ok(());
    }
    if !try_start_bulk(&ctx.app, &g) {
        fail(ctx, "Busy", "A role sync is already running for this server. Try again when it finishes.").await;
        return Ok(());
    }
    let mut guard = BulkGuard { app: ctx.app.clone(), guild_id: g.clone(), armed: true };
    let rows = ranked_rows(ctx.app.db(), &g, Some(0), None, BULK_SYNC_MAX_MEMBERS).await?;
    let mut text = format!("Re-syncing reward roles for {} ranked member(s) in the background. I'll post a summary here when it's done.", rows.len());
    if rows.len() as i64 >= BULK_SYNC_MAX_MEMBERS {
        text.push_str(&format!("\n(Only the top {BULK_SYNC_MAX_MEMBERS} are synced.)"));
    }
    if safe_reply(&ctx.responder, Payload::embed(success_embed("Role Sync Started", Some(&text), Some("Members without XP are not checked.")))).await.is_none()
    {
        return Ok(());
    }
    guard.armed = false;
    let sync_down = s.level_roles_sync_on_demotion;
    run_bulk_in_background(&ctx.app, bulk_report(ctx, guild_id), "Role sync".into(), async move { Ok(rows) }, s, sync_down);
    Ok(())
}

// ── XP boost roles ────────────────────────────────────────────────────────────

/// The boost-list edit of `/levelconfig boost add` (`ConfigError` at the cap).
pub fn boost_add_patch(current: Option<&Value>, role_id: &str, multiplier: f64) -> anyhow::Result<SettingsPatch> {
    let list = normalize_boost_roles(current);
    if !list.iter().any(|b| b.role_id == role_id) && list.len() >= bot_config::limits::XP_BOOST_ROLES {
        anyhow::bail!(ConfigError(format!("You can have at most {} XP boost roles.", bot_config::limits::XP_BOOST_ROLES)));
    }
    let mut next: Vec<Value> = list.iter().filter(|b| b.role_id != role_id).map(BoostRole::to_json).collect();
    next.push(json!({ "roleId": role_id, "multiplier": multiplier }));
    Ok(SettingsPatch::new().set("xpBoostRoles", boost_roles_json(&normalize_boost_roles(Some(&Value::Array(next))))))
}

async fn sub_boost_add(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?.to_string();
    let role = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?.to_string();
    let multiplier = round2(ctx.get_number("multiplier").unwrap_or(1.0));
    if role == g {
        fail(ctx, "Not Saved", "Use `/levelconfig multiplier` to boost everyone.").await;
        return Ok(());
    }
    let id = role.clone();
    ctx.app.settings().update_settings_with(&g, move |cur| boost_add_patch(cur.xp_boost_roles.as_ref(), &id, multiplier)).await?;
    let text = format!("{} earns **×{}** XP. When a member has several boost roles, only the highest applies.", role_line(&role), num(multiplier));
    reply(ctx, success_embed("XP Boost Saved", Some(&text), None)).await;
    Ok(())
}

async fn sub_boost_remove(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?.to_string();
    let role = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?.to_string();
    let id = role.clone();
    ctx.app
        .settings()
        .update_settings_with(&g, move |cur| {
            let list = normalize_boost_roles(cur.xp_boost_roles.as_ref());
            if !list.iter().any(|b| b.role_id == id) {
                anyhow::bail!(ConfigError("That role is not an XP boost role.".into()));
            }
            let kept: Vec<BoostRole> = list.into_iter().filter(|b| b.role_id != id).collect();
            Ok(SettingsPatch::new().set("xpBoostRoles", boost_roles_json(&kept)))
        })
        .await?;
    reply(ctx, success_embed("XP Boost Removed", Some(&format!("{} no longer boosts XP.", role_line(&role))), None)).await;
    Ok(())
}

async fn sub_boost_list(ctx: &CommandCtx) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let s = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let boosts = normalize_boost_roles(s.xp_boost_roles.as_ref());
    if boosts.is_empty() {
        let e = info_embed("XP Boost Roles", Some("No XP boost roles. Add one with `/levelconfig boost add`."), None);
        reply(ctx, apply_guild_color(e, Some(&s))).await;
        return Ok(());
    }
    let roles: Option<HashSet<String>> = cached_guild(&ctx.app, guild_id).map(|gd| gd.roles.keys().map(|r| r.to_string()).collect());
    let lines: Vec<String> = boosts
        .iter()
        .map(|b| {
            let gone = !roles.as_ref().is_some_and(|r| r.contains(&b.role_id));
            format!("{} — ×{}{}", role_line(&b.role_id), num(b.multiplier), if gone { " ⚠️ deleted role" } else { "" })
        })
        .collect();
    reply(ctx, apply_guild_color(info_embed("XP Boost Roles", Some(&lines.join("\n")), None), Some(&s))).await;
    Ok(())
}

// ── XP management (/levelconfig xp give | remove | set | reset | view) ───────

fn mention_roles(ids: &[String]) -> String {
    ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ")
}

/// `xpChangeLines(result, lc)`: the level change and reward-role lines appended to an admin reply.
pub fn xp_change_lines(old_level: i64, new_level: i64, roles: Option<&RoleSync>) -> String {
    let mut lines: Vec<String> = Vec::new();
    if new_level != old_level {
        lines.push(format!("Level {old_level} → {new_level}."));
    }
    if let Some(r) = roles {
        if !r.added.is_empty() {
            lines.push(format!("Roles added: {}.", mention_roles(&r.added)));
        }
        if !r.removed.is_empty() {
            lines.push(format!("Roles removed: {}.", mention_roles(&r.removed)));
        }
        if !r.failed.is_empty() {
            let ids: Vec<String> = r.failed.iter().map(|f| format!("<@&{}>", f.0)).collect();
            lines.push(format!("Could not change: {} (check my role position and Manage Roles).", ids.join(", ")));
        }
    }
    if lines.is_empty() { String::new() } else { format!("\n{}", lines.join("\n")) }
}

/// `displayAvatarURL()` with discord.js's defaults (WebP, or GIF when animated; no size): the server avatar first.
pub fn default_avatar_url(guild_id: GuildId, member: Option<&Member>, user: &serenity::all::User) -> String {
    let ext = |h: &str| if h.starts_with("a_") { "gif" } else { "webp" };
    if let Some(h) = member.and_then(|m| m.avatar.as_ref()) {
        let h = h.to_string();
        return format!("https://cdn.discordapp.com/guilds/{guild_id}/users/{}/avatars/{h}.{}", user.id, ext(&h));
    }
    match &user.avatar {
        Some(h) => {
            let h = h.to_string();
            format!("https://cdn.discordapp.com/avatars/{}/{h}.{}", user.id, ext(&h))
        }
        None => bot_core::adapters::default_avatar_url(user.id, &bot_core::adapters::discriminator(user)),
    }
}

fn changed(r: &XpChange, lc: &LevelChangeOutcome) -> String {
    xp_change_lines(r.old_level, r.new_level, lc.roles.as_ref())
}

async fn execute_xp(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let sub = ctx.subcommand().unwrap_or_default();
    let settings = ctx.app.settings().get_settings(&g).await?;
    if sub == "give" {
        return xp_give(&ctx, settings).await;
    }
    let user = ctx.get_user("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
    let user_id = user.id.to_string();
    let who = format!("<@{user_id}>");
    let fmt = |n: i64| format_number(n as f64);

    match sub.as_str() {
        "remove" => {
            let amount = ctx.get_integer("amount").unwrap_or(1);
            let r = subtract_xp(&ctx.app, &g, &user_id, amount as f64).await?;
            let Some(r) = r.filter(|r| r.old_xp != Some(0)) else {
                reply(&ctx, info_embed("Nothing to Remove", Some(&format!("{who} has no XP in this server.")), None)).await;
                return Ok(());
            };
            let member = fetch_member(&ctx.app, guild_id, user.id).await;
            let lc = handle_level_change(&ctx.app, LevelChange::of(guild_id, &user_id, &r, Some(settings), "admin").member(member).announce(false)).await;
            let old = r.old_xp.unwrap_or(r.xp);
            let text = format!(
                "Removed **{} XP** from {who}. Total: **{} XP** (level {}).{}",
                fmt(old - r.xp),
                fmt(r.xp),
                r.new_level,
                changed(&r, &lc)
            );
            reply(&ctx, success_embed("XP Removed", Some(&text), None)).await;
        }
        "set" => {
            let amount = ctx.get_integer("amount").unwrap_or(0);
            let member = fetch_member(&ctx.app, guild_id, user.id).await;
            if amount > 0 {
                if user.bot {
                    fail(&ctx, "Not Saved", "Bots don't earn XP.").await;
                    return Ok(());
                }
                if member.is_none() {
                    fail(&ctx, "Not Saved", "That user isn't in this server.").await;
                    return Ok(());
                }
            }
            let Some(r) = set_xp(&ctx.app, &g, &user_id, amount as f64).await? else {
                reply(&ctx, info_embed("Nothing to Set", Some(&format!("{who} already has 0 XP.")), None)).await;
                return Ok(());
            };
            let lc = handle_level_change(&ctx.app, LevelChange::of(guild_id, &user_id, &r, Some(settings), "admin").member(member).announce(false)).await;
            let text = format!("{who} now has **{} XP** (level {}).{}", fmt(r.xp), r.new_level, changed(&r, &lc));
            reply(&ctx, success_embed("XP Set", Some(&text), None)).await;
        }
        "reset" => {
            let Some(old_level) = reset_xp(ctx.app.db(), &g, &user_id).await? else {
                reply(&ctx, info_embed("Nothing to Reset", Some(&format!("{who} has no XP in this server.")), None)).await;
                return Ok(());
            };
            let mut removed = String::new();
            if settings.level_roles_sync_on_demotion
                && let Some(member) = fetch_member(&ctx.app, guild_id, user.id).await
            {
                let opts = SyncOptions { sync_down: true, reason: "XP reset".into(), ..Default::default() };
                let roles = sync_level_roles(&ctx.app, guild_id, &member, 0, Some(&settings), opts).await;
                if !roles.removed.is_empty() {
                    removed = format!("\nRoles removed: {}.", mention_roles(&roles.removed));
                }
            }
            let text = format!("Wiped XP and level for {who} (was level {old_level}).{removed}");
            reply(&ctx, success_embed("XP Reset", Some(&text), None)).await;
        }
        _ => {
            // view
            let Some(row) = find_user_level(ctx.app.db(), &g, &user_id).await? else {
                reply(&ctx, info_embed("No Data", Some(&format!("{who} has no XP in this server.")), None)).await;
                return Ok(());
            };
            let xp = i64::from(row.xp);
            let (rank, member) = tokio::join!(get_rank(ctx.app.db(), &g, &user_id, xp), fetch_member(&ctx.app, guild_id, user.id));
            let rank = rank?;
            let level = level_from_xp(xp as f64); // the XP is the truth; the stored level can lag during a race
            let name = member.as_ref().map(member_display_name).unwrap_or_else(|| user.name.clone());
            let last = row.last_xp_at.map_or_else(|| "Never".to_string(), |t| format!("<t:{}:R>", t.timestamp_millis().div_euclid(1000)));
            let embed = guild_embed(Some(&settings), Some(&format!("XP — {name}")), None, None)
                .field("Level", level.to_string(), true)
                .field("Total XP", fmt(xp), true)
                .field("Rank", format!("#{}", fmt(rank)), true)
                .field("Progress", format!("{} / {}", format_number(xp_into_level(xp as f64)), fmt(bot_config::xp_for_level(level))), true)
                .field("Messages", fmt(i64::from(row.message_count)), true)
                .field("Voice", format!("{} min", fmt(i64::from(row.voice_minutes))), true)
                .field("Last XP", last, true)
                .thumbnail(default_avatar_url(guild_id, member.as_ref(), &user));
            reply(&ctx, embed).await;
        }
    }
    Ok(())
}

// ── /levelconfig xp give ──────────────────────────────────────────────────────

/// The reply text of a member give (`None`: already at the cap, see [`cap_text`]).
pub fn give_text(user_id: &str, amount: i64, before: i64, r: &XpChange, lines: &str) -> Option<String> {
    let fmt = |n: i64| format_number(n as f64);
    let added = amount.min((r.xp - before).max(0));
    if added == 0 {
        return None;
    }
    let mut text = format!("Gave **{} XP** to <@{user_id}>. Total: **{} XP** (level {}).{lines}", fmt(added), fmt(r.xp), r.new_level);
    if added < amount {
        text.push_str(&format!("\nOnly {} of {} XP fitted: XP is capped at {}.", fmt(added), fmt(amount), fmt(XP_MAX)));
    } else if r.xp == XP_MAX {
        text.push_str(&format!("\nXP is capped at {}.", fmt(XP_MAX)));
    }
    Some(text)
}

/// The "Already at the XP Cap" text.
pub fn cap_text(user_id: &str, r: &XpChange) -> String {
    let fmt = |n: i64| format_number(n as f64);
    format!("<@{user_id}> is already at the XP cap of {}, so no XP was added. Total: **{} XP** (level {}).", fmt(XP_MAX), fmt(r.xp), r.new_level)
}

async fn xp_give(ctx: &CommandCtx, settings: Settings) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let amount = ctx.get_integer("amount").unwrap_or(1);
    let target: GenericId = ctx.get_mentionable_id("target").ok_or_else(|| anyhow::anyhow!("Required option \"target\" not found."))?;
    let resolved = &ctx.interaction.data.resolved;
    if let Some(role) = resolved.roles.get(&RoleId::new(target.get())) {
        return xp_give_to_role(ctx, settings, role.id, amount).await;
    }
    let Some(user) = resolved.users.get(&UserId::new(target.get())).cloned() else {
        fail(ctx, "Not Saved", "Pick a member or a role.").await;
        return Ok(());
    };
    if user.bot {
        fail(ctx, "Not Saved", "Bots don't earn XP.").await;
        return Ok(());
    }
    let member = match resolved.members.get(&user.id) {
        Some(p) => Some(bot_core::interaction::member_from_partial(p.clone(), user.clone(), Some(guild_id))),
        None => fetch_member(&ctx.app, guild_id, user.id).await,
    };
    let Some(member) = member else {
        fail(ctx, "Not Saved", "That user isn't in this server.").await;
        return Ok(());
    };
    let u = user.id.to_string();
    // Read first so the reply can say what the cap let through (display only; the write below stays atomic).
    let before = find_user_level(ctx.app.db(), &g, &u).await?.map_or(0, |r| i64::from(r.xp));
    let r = apply_xp_delta(&ctx.app, XpDelta::new(&g, &u, amount as f64)).await?;
    let lc = handle_level_change(
        &ctx.app,
        LevelChange::of(guild_id, &u, &r, Some(settings), "admin").member(Some(member)).channel(Some(ctx.channel_id())).announce(false),
    )
    .await;
    match give_text(&u, amount, before, &r, &changed(&r, &lc)) {
        None => reply(ctx, info_embed("Already at the XP Cap", Some(&cap_text(&u, &r)), None)).await,
        Some(text) => reply(ctx, success_embed("XP Given", Some(&text), None)).await,
    }
    Ok(())
}

/// Non-bot ids that hold `role`, from the guild's member cache.
fn xp_human_ids(app: &App, guild_id: GuildId, role: RoleId) -> Vec<String> {
    let Some(guild) = cached_guild(app, guild_id) else { return Vec::new() };
    guild.members.values().filter(|m| !m.user.bot && m.roles.contains(&role)).map(|m| m.user.id.to_string()).collect()
}

fn too_many(n: usize) -> String {
    format!("That role has {n} members; the limit is {}. Use a smaller role.", format_number(ROLE_GIVE_MAX_MEMBERS as f64))
}

async fn xp_give_to_role(ctx: &CommandCtx, settings: Settings, role: RoleId, amount: i64) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    if role.get() == guild_id.get() {
        fail(ctx, "Not Saved", "Pick a specific role; @everyone isn't allowed.").await;
        return Ok(());
    }
    if !try_start_bulk(&ctx.app, &g) {
        fail(ctx, "Busy", "A bulk XP or role task is already running for this server. Try again when it finishes.").await;
        return Ok(());
    }
    let mut guard = BulkGuard { app: ctx.app.clone(), guild_id: g.clone(), armed: true };
    // The cap is checked before the (expensive) full member fetch, against the members already cached.
    let mut ids = xp_human_ids(&ctx.app, guild_id, role);
    if ids.len() > ROLE_GIVE_MAX_MEMBERS {
        fail(ctx, "Not Saved", &too_many(ids.len())).await;
        return Ok(());
    }
    let incomplete = cached_guild(&ctx.app, guild_id).is_some_and(|gd| (gd.members.len() as u64) < gd.member_count);
    if incomplete {
        if let Err(e) = bot_core::guild_members::fetch_all_members(&ctx.app, guild_id, 60_000, bot_core::guild_members::FETCH_ALL_MAX_MEMBERS).await {
            let text = if e.reason() == "too-large" {
                "This server has too many members for me to load them all. Give XP to members one by one instead."
            } else {
                "I couldn't load the member list right now. Try again in a minute."
            };
            fail(ctx, "Not Saved", text).await;
            return Ok(());
        }
        ids = xp_human_ids(&ctx.app, guild_id, role);
    }
    if ids.is_empty() {
        fail(ctx, "Not Saved", "No members (other than bots) have that role.").await;
        return Ok(());
    }
    if ids.len() > ROLE_GIVE_MAX_MEMBERS {
        fail(ctx, "Not Saved", &too_many(ids.len())).await;
        return Ok(());
    }

    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Give XP to a role?".into()),
            description: Some(format!(
                "Give **{} XP** to **{}** members of <@&{role}>?",
                format_number(amount as f64),
                format_number(ids.len() as f64)
            )),
            confirm_label: "Give XP".into(),
            danger: false,
            ..ConfirmOptions::default()
        },
    )
    .await;
    if !ok {
        safe_reply(&ctx.responder, Payload::embed(info_embed("Cancelled", Some("No XP was given."), None)).components(Vec::new())).await;
        return Ok(());
    }

    let res = give_xp_to_members(&ctx.app, &g, &ids, amount as f64).await?;
    let mut text = format!(
        "Gave **{} XP** to **{}** members of <@&{role}>. {} levelled up.",
        format_number(amount as f64),
        format_number(res.updated as f64),
        res.level_ups.len()
    );
    // With no reward roles there is nothing to sync, so say nothing about it (the level-up hooks still run below).
    let has_rewards = !res.level_ups.is_empty() && level_role_count(ctx.app.db(), &g).await.unwrap_or(1) > 0;
    if has_rewards {
        text.push_str("\nSyncing their reward roles in the background…");
    }
    safe_reply(&ctx.responder, Payload::embed(success_embed("XP Given", Some(&text), None)).components(Vec::new())).await;
    if !res.level_ups.is_empty() {
        guard.armed = false;
        let (app, report) = (ctx.app.clone(), bulk_report(ctx, guild_id));
        tokio::spawn(async move { xp_sync_role_level_ups(app, report, settings, res.level_ups).await });
    }
    Ok(())
}

/// `xpSyncRoleLevelUps(...)`: the background half of a role give: sequential role sync and `levelUp` emission, then a
/// summary. No announcements. Always releases the bulk lock.
pub async fn xp_sync_role_level_ups(app: App, report: BulkReport, settings: Settings, level_ups: Vec<BulkLevelUp>) {
    let g = report.guild_id.to_string();
    let _guard = BulkGuard { app: app.clone(), guild_id: g.clone(), armed: true };
    let rewards = match level_roles(app.db(), &g).await {
        Ok(r) => reward_pairs(&r),
        Err(e) => {
            bot_util::log_error!("levels: role-give background task failed", bot_core::error::log_arg(&e));
            return;
        }
    };
    let mut synced = 0;
    for lu in &level_ups {
        let uid = lu.user_id.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new);
        let m = uid.and_then(|u| svc::cached_member(&app, report.guild_id, u));
        if let Some(m) = m.as_ref()
            && !rewards.is_empty()
        {
            sync_level_roles(&app, report.guild_id, m, lu.new_level, Some(&settings), SyncOptions { rewards: Some(rewards.clone()), ..Default::default() }).await;
            synced += 1;
        }
        if let Some(u) = uid {
            app.run_hooks(LevelUp {
                guild_id: report.guild_id,
                member: m,
                user_id: u,
                old_level: lu.old_level,
                new_level: lu.new_level,
                channel_id: None,
                source: "admin".into(),
            })
            .await;
        }
    }
    if !rewards.is_empty() {
        let text = format!("XP give: reward roles checked for {synced} of {} member(s) who levelled up.", level_ups.len());
        report_bulk_result(&app, &report, &text, true).await;
    }
}
