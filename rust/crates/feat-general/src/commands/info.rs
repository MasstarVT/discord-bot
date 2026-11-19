//! Port of `src/commands/utility/info.js`: `/info user | server | role` (MEE6 /user-info /server-info /role-info). A
//! core command, guild-only, answered publicly without pings.
//!
//! The embeds are built from plain views ([`ServerView`], [`RoleView`], [`UserView`], [`MemberView`]) read from the
//! serenity cache, so the builders are pure; the REST counts (`with_counts`, role member counts) are memoised for
//! 60 s per app, as the JS memoises them per process.

use std::sync::Mutex;

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Payload, safe_defer, safe_reply, with_cached_guild};
use bot_core::markdown::escape_markdown;
use bot_core::permission_checker::MemberAccess;
use bot_core::prelude::*;
use bot_core::{adapters, permissions};
use bot_db::GuildSettings;
use bot_util::jsre::LazyRe;
use bot_util::text::{clamp_text, default_more, format_number, join_with_limit};
use serde_json::{Value, json};
use serenity::all::{Guild, GuildId, Member, Permissions, Role, RoleId, User, UserId};
use serenity::builder::{CreateEmbed, CreateEmbedFooter};
use serenity::http::Route;

use crate::bot_join_service::rest_get_json;

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("info", "User, server and role info (MEE6 /user-info /server-info /role-info)")
        .option(
            Opt::sub_command("user", "Show info about a member (MEE6 /user-info)")
                .option(Opt::user("member", "Member to inspect (default: you)")),
        )
        .option(Opt::sub_command("server", "Show info about this server (MEE6 /server-info)"))
        .option(
            Opt::sub_command("role", "Show info about a role (MEE6 /role-info)").option(Opt::role("role", "Role to inspect").required(true)),
        )
}

/// The command (core, MEMBER, guild-only).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), None, execute).permission_level(PermissionLevel::Member)
}

// ── Constants ──────────────────────────────────────────────────────────────────

/// `KEY_PERMISSIONS`: the notable permissions, in display order.
pub const KEY_PERMISSIONS: [&str; 14] = [
    "ManageGuild",
    "ManageRoles",
    "ManageChannels",
    "ManageMessages",
    "ManageThreads",
    "ManageWebhooks",
    "ManageNicknames",
    "ManageGuildExpressions",
    "ManageEvents",
    "KickMembers",
    "BanMembers",
    "ModerateMembers",
    "MentionEveryone",
    "ViewAuditLog",
];

/// `VERIFICATION_LABELS`.
pub const VERIFICATION_LABELS: [&str; 5] = ["None", "Low", "Medium", "High", "Highest"];
/// `CONTENT_FILTER_LABELS`.
pub const CONTENT_FILTER_LABELS: [&str; 3] = ["Off", "Members without roles", "All members"];

/// `USER_BADGES`: (discord.js `UserFlags` key, its bit, the label), in bit order (`flags.toArray()` order).
pub const USER_BADGES: [(&str, u64, &str); 13] = [
    ("Staff", 1 << 0, "Discord Staff"),
    ("Partner", 1 << 1, "Partnered Server Owner"),
    ("Hypesquad", 1 << 2, "HypeSquad Events"),
    ("BugHunterLevel1", 1 << 3, "Bug Hunter"),
    ("HypeSquadOnlineHouse1", 1 << 6, "HypeSquad Bravery"),
    ("HypeSquadOnlineHouse2", 1 << 7, "HypeSquad Brilliance"),
    ("HypeSquadOnlineHouse3", 1 << 8, "HypeSquad Balance"),
    ("PremiumEarlySupporter", 1 << 9, "Early Supporter"),
    ("BugHunterLevel2", 1 << 14, "Bug Hunter (Gold)"),
    ("VerifiedBot", 1 << 16, "Verified Bot"),
    ("VerifiedDeveloper", 1 << 17, "Early Verified Bot Developer"),
    ("CertifiedModerator", 1 << 18, "Moderator Programs Alumni"),
    ("ActiveDeveloper", 1 << 22, "Active Developer"),
];

const COUNTS_TTL_MS: i64 = 60_000;
const COUNTS_MAX_ENTRIES: usize = 500;
const REST_TIMEOUT_MS: u64 = 3_000;

// ── Pure helpers ───────────────────────────────────────────────────────────────

/// `keyPermissionNames(perms)`: the notable permissions held, as display names, in [`KEY_PERMISSIONS`] order.
pub fn key_permission_names(perms: Permissions) -> Vec<String> {
    if perms.contains(Permissions::ADMINISTRATOR) {
        return vec!["Administrator (all permissions)".into()];
    }
    KEY_PERMISSIONS
        .iter()
        .filter(|k| permissions::flag(k).is_some_and(|f| permissions::has_exact(perms, f)))
        .map(|k| permissions::permission_label(k))
        .collect()
}

/// `formatRoleMentions(roles, guildId, max)`: role mentions, highest first, `@everyone` excluded; never cuts a
/// mention. 'None' when there are no roles. `roles` are (id, position).
pub fn format_role_mentions(roles: &[(String, i64)], guild_id: &str, max: usize) -> String {
    let mut list: Vec<&(String, i64)> = roles.iter().filter(|(id, _)| id != guild_id).collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.1));
    let items: Vec<String> = list.iter().map(|(id, _)| format!("<@&{id}>")).collect();
    if items.is_empty() {
        return "None".into();
    }
    join_with_limit(&items, " ", max, default_more)
}

/// Channel counts by kind (threads ignored).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelCounts {
    pub text: usize,
    pub voice: usize,
    pub forum: usize,
    pub category: usize,
}

/// `countChannels(channels)` over Discord channel types.
pub fn count_channels(types: &[u8]) -> ChannelCounts {
    let mut out = ChannelCounts::default();
    for t in types {
        match t {
            0 | 5 => out.text += 1,
            2 | 13 => out.voice += 1,
            15 | 16 => out.forum += 1,
            4 => out.category += 1,
            _ => {}
        }
    }
    out
}

fn ts(ms: i64) -> String {
    let s = ms.div_euclid(1000);
    format!("<t:{s}:F>\n<t:{s}:R>")
}

fn esc(s: &str) -> String {
    escape_markdown(s)
}

fn yes(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

// Discord also reports flags that only track its own rollouts ("..._MIGRATION_COMPLETE", experiments, legacy thread
// switches). They say nothing about the server, so /info server leaves them out.
static INTERNAL_FEATURE_PATTERN: LazyRe = bot_util::lazy_re!(
    r"_MIGRATION(_COMPLETE)?$|_EXPERIMENT$|_OVERRIDE$|^(CHANNEL_ICON_EMOJIS_GENERATED|GUILD_ONBOARDING_EVER_ENABLED|GUILD_ONBOARDING_HAS_PROMPTS|NEW_THREAD_PERMISSIONS|THREADS_ENABLED|TEXT_IN_VOICE_ENABLED|SEVEN_DAY_THREAD_ARCHIVE|THREE_DAY_THREAD_ARCHIVE|HAS_DIRECTORY_ENTRY)$",
    ""
);

/// `isPublicFeature(feature)`.
pub fn is_public_feature(feature: &str) -> bool {
    !INTERNAL_FEATURE_PATTERN.test(&feature.to_uppercase())
}

/// A feature flag as words: `ANIMATED_ICON` → `Animated Icon`.
pub fn feature_label(feature: &str) -> String {
    bot_util::js::lower(feature)
        .split('_')
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(c) => c.to_uppercase().chain(cs).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `accessLevelLabel(level, member, guild)`.
pub fn access_level_label(level: PermissionLevel, is_guild_owner: bool) -> &'static str {
    match level {
        PermissionLevel::Owner => "Bot Owner",
        PermissionLevel::Admin if is_guild_owner => "Server Owner",
        PermissionLevel::Admin => "Admin", // /help calls this level "Admin"
        PermissionLevel::Moderator => "Moderator",
        PermissionLevel::Member => "Member",
    }
}

const CDN: &str = "https://cdn.discordapp.com";

/// discord.js `makeURL(root/hash, { size })`: webp, gif when animated.
fn cdn_url(root: &str, hash: &str, size: u16) -> String {
    let ext = if hash.starts_with("a_") { "gif" } else { "webp" };
    format!("{CDN}{root}/{hash}.{ext}?size={size}")
}

// ── REST counts (memoised 60 s per app) ─────────────────────────────────────────

/// `{ at, members, online, stickers }` from `GET /guilds/{id}?with_counts=true`.
#[derive(Debug, Clone, PartialEq)]
pub struct GuildCounts {
    pub at: i64,
    pub members: f64,
    pub online: Option<f64>,
    /// bot.js keeps no sticker cache: only the REST answer knows.
    pub stickers: Option<usize>,
}

/// A memoised role-count answer: (fetched at, (role id, members)).
type RoleCounts = (i64, Vec<(String, f64)>);

/// The JS module-level `GUILD_COUNTS` / `ROLE_COUNTS` maps, per app (insertion-ordered, at most 500 each).
#[derive(Default)]
pub struct InfoCounts {
    guilds: Mutex<Vec<(String, GuildCounts)>>,
    roles: Mutex<Vec<(String, RoleCounts)>>,
}

fn remember<T>(map: &mut Vec<(String, T)>, key: &str, value: T) {
    if let Some(slot) = map.iter_mut().find(|(k, _)| k == key) {
        slot.1 = value;
        return;
    }
    if map.len() >= COUNTS_MAX_ENTRIES {
        map.remove(0);
    }
    map.push((key.to_string(), value));
}

/// The counts of a raw guild answer (`res`), falling back to the cached member count.
pub fn guild_counts_from(res: Option<&Value>, cached_member_count: u64, at: i64) -> GuildCounts {
    GuildCounts {
        at,
        members: res.and_then(|r| r.get("approximate_member_count")).and_then(Value::as_f64).unwrap_or(cached_member_count as f64),
        online: res.and_then(|r| r.get("approximate_presence_count")).and_then(Value::as_f64),
        stickers: res.and_then(|r| r.get("stickers")).and_then(Value::as_array).map(Vec::len),
    }
}

/// `getGuildCounts(guild)`: memoised 60 s; a failed or slow REST call falls back to the cached member count.
pub async fn get_guild_counts(app: &App, guild_id: GuildId, cached_member_count: u64) -> GuildCounts {
    let state = app.ext::<InfoCounts>();
    let key = guild_id.to_string();
    let now = app.clock().now_ms();
    let hit = state.guilds.lock().ok().and_then(|m| m.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()));
    if let Some(h) = hit
        && now - h.at < COUNTS_TTL_MS
    {
        return h;
    }
    let res = match app.http() {
        Some(http) => {
            let params = Some(vec![("with_counts", "true".to_string())]);
            let fetched = bot_util::async_util::with_timeout(
                async move { rest_get_json(&http, Route::Guild { guild_id }, params).await.map_err(|e| e.to_string()) },
                REST_TIMEOUT_MS,
                "info guild counts",
            )
            .await;
            fetched.ok()
        }
        None => None,
    };
    let entry = guild_counts_from(res.as_ref(), cached_member_count, app.clock().now_ms());
    if let Ok(mut m) = state.guilds.lock() {
        remember(&mut m, &key, entry.clone());
    }
    entry
}

/// `getRoleCounts(guild)`: `GET /guilds/{id}/roles/member-counts`, memoised 60 s. Fails when Discord does not answer
/// in time.
pub async fn get_role_counts(app: &App, guild_id: GuildId) -> anyhow::Result<Vec<(String, f64)>> {
    let state = app.ext::<InfoCounts>();
    let key = guild_id.to_string();
    let now = app.clock().now_ms();
    let hit = state.roles.lock().ok().and_then(|m| m.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()));
    if let Some((at, counts)) = hit
        && now - at < COUNTS_TTL_MS
    {
        return Ok(counts);
    }
    let http = app.http().ok_or_else(|| anyhow::anyhow!("no Discord connection"))?;
    let data = bot_util::async_util::with_timeout(
        async move { rest_get_json(&http, Route::GuildRoleMemberCounts { guild_id }, None).await.map_err(|e| e.to_string()) },
        REST_TIMEOUT_MS,
        "info role counts",
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let counts: Vec<(String, f64)> = data
        .as_object()
        .map(|o| o.iter().map(|(id, n)| (id.clone(), n.as_f64().or_else(|| n.as_str().and_then(|s| s.parse().ok())).unwrap_or(f64::NAN))).collect())
        .unwrap_or_default();
    if let Ok(mut m) = state.roles.lock() {
        remember(&mut m, &key, (app.clock().now_ms(), counts.clone()));
    }
    Ok(counts)
}

// ── Views ──────────────────────────────────────────────────────────────────────

/// What `/info server` reads of a guild.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: String,
    pub created_ms: i64,
    pub icon_url: Option<String>,
    pub banner_url: Option<String>,
    /// Channel types of the (non-thread) channels.
    pub channel_types: Vec<u8>,
    /// `animated` of every custom emoji.
    pub emojis_animated: Vec<bool>,
    /// `guild.roles.cache.size` (`@everyone` included).
    pub role_count: usize,
    pub premium_tier: u8,
    pub premium_subscription_count: Option<u64>,
    pub verification_level: u8,
    pub explicit_content_filter: u8,
    pub mfa_level: u8,
    pub preferred_locale: String,
    pub vanity_url_code: Option<String>,
    pub features: Vec<String>,
    pub member_count: u64,
}

impl ServerView {
    /// From a cached guild.
    pub fn of(guild: &Guild) -> Self {
        let id = guild.id;
        Self {
            id: id.to_string(),
            name: guild.name.clone(),
            description: guild.description.clone(),
            owner_id: guild.owner_id.to_string(),
            created_ms: adapters::snowflake_ms(id.get()) as i64,
            icon_url: adapters::guild_icon_url_with(id, guild.icon.as_ref(), adapters::ImageUrlOptions::new("webp", Some(256))),
            banner_url: guild.banner.as_deref().map(|h| cdn_url(&format!("/banners/{id}"), h, 1024)),
            channel_types: guild.channels.values().map(|c| u8::from(c.kind)).collect(),
            emojis_animated: guild.emojis.values().map(|e| e.animated).collect(),
            role_count: guild.roles.len(),
            premium_tier: u8::from(guild.premium_tier),
            premium_subscription_count: guild.premium_subscription_count,
            verification_level: u8::from(guild.verification_level),
            explicit_content_filter: u8::from(guild.explicit_content_filter),
            mfa_level: u8::from(guild.mfa_level),
            preferred_locale: guild.preferred_locale.clone(),
            vanity_url_code: guild.vanity_url_code.clone(),
            features: guild.features.clone(),
            member_count: guild.member_count,
        }
    }
}

fn role_count_text(n: usize) -> String {
    format_number(n.saturating_sub(1) as f64)
}

/// `buildServerEmbed(guild, settings)` from the view and the counts.
pub fn build_server_embed(v: &ServerView, counts: &GuildCounts, settings: Option<&GuildSettings>) -> CreateEmbed {
    let mut embed = guild_embed(settings, Some(&clamp_text(&v.name, 256)), None, None).footer(CreateEmbedFooter::new(format!("Server ID: {}", v.id)));
    if let Some(d) = v.description.as_deref().filter(|d| !d.is_empty()) {
        embed = embed.description(clamp_text(&escape_markdown(d), 1024));
    }
    if let Some(icon) = &v.icon_url {
        embed = embed.thumbnail(icon);
    }
    if let Some(banner) = &v.banner_url {
        embed = embed.image(banner);
    }
    let ch = count_channels(&v.channel_types);
    let animated = v.emojis_animated.iter().filter(|a| **a).count();
    let members = match counts.online {
        Some(online) => format!("{} total\n🟢 {} online", format_number(counts.members), format_number(online)),
        None => format!("{} total", format_number(counts.members)),
    };
    let emojis = match counts.stickers {
        Some(s) => format!("{} emojis ({animated} animated)\n{s} stickers", v.emojis_animated.len()),
        None => format!("{} emojis ({animated} animated)", v.emojis_animated.len()),
    };
    let security = [
        format!("Verification: {}", VERIFICATION_LABELS.get(v.verification_level as usize).copied().unwrap_or("Unknown")),
        format!("Content filter: {}", CONTENT_FILTER_LABELS.get(v.explicit_content_filter as usize).copied().unwrap_or("Unknown")),
        format!("2FA for moderators: {}", if v.mfa_level == 1 { "Required" } else { "Not required" }),
    ]
    .join("\n");
    embed = embed
        .field("👑 Owner", format!("<@{}>", v.owner_id), true)
        .field("📅 Created", ts(v.created_ms), true)
        .field("👥 Members", members, true)
        .field("💬 Channels", format!("Text: {}\nVoice: {}\nForum: {}\nCategories: {}", ch.text, ch.voice, ch.forum, ch.category), true)
        .field("🎭 Roles", role_count_text(v.role_count), true)
        .field("😀 Emojis & stickers", emojis, true)
        .field("💎 Boosts", format!("Level {} · {} boosts", v.premium_tier, v.premium_subscription_count.unwrap_or(0)), true)
        .field("🛡️ Security", security, true)
        .field("🌐 Locale", clamp_text(&esc(&v.preferred_locale), 1024), true);
    if let Some(code) = v.vanity_url_code.as_deref().filter(|c| !c.is_empty()) {
        embed = embed.field("🔗 Vanity URL", clamp_text(&format!("discord.gg/{}", esc(code)), 1024), true);
    }
    let mut list: Vec<String> = v.features.iter().filter(|f| is_public_feature(f)).map(|f| feature_label(f)).collect();
    if !list.is_empty() {
        list.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let escaped: Vec<String> = list.iter().map(|f| esc(f)).collect();
        embed = embed.field("✨ Features", join_with_limit(&escaped, ", ", 1024, default_more), false);
    }
    embed
}

/// A role's tags (`role.tags`), present when any is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleTagsView {
    pub bot_id: Option<String>,
    pub premium_subscriber_role: bool,
    pub subscription_listing_id: Option<String>,
    pub guild_connections: bool,
    pub integration_id: Option<String>,
}

/// What `/info role` reads of a role.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleView {
    pub id: String,
    pub name: String,
    pub unicode_emoji: Option<String>,
    pub primary_color: u32,
    pub secondary_color: Option<u32>,
    pub icon_url: Option<String>,
    pub position: i64,
    pub created_ms: i64,
    pub hoist: bool,
    pub mentionable: bool,
    pub managed: bool,
    pub tags: Option<RoleTagsView>,
    pub permissions: Permissions,
}

impl RoleView {
    /// From a serenity role. `position` is discord.js's `role.position`: its rank among the guild's cached roles (the
    /// raw API position only when the guild does not have the role).
    pub fn of(role: &Role, guild: Option<&Guild>) -> Self {
        let t = &role.tags;
        let tags = RoleTagsView {
            bot_id: t.bot_id.map(|b| b.to_string()),
            premium_subscriber_role: t.premium_subscriber,
            subscription_listing_id: t.subscription_listing_id.map(|s| s.to_string()),
            guild_connections: t.guild_connections,
            integration_id: t.integration_id.map(|i| i.to_string()),
        };
        // serenity always has a tags struct; discord.js has `tags: null` when Discord sent none.
        let has_tags = tags != RoleTagsView::default() || t.available_for_purchase;
        Self {
            id: role.id.to_string(),
            name: role.name.clone(),
            unicode_emoji: role.unicode_emoji.clone().filter(|e| !e.is_empty()),
            primary_color: role_primary(role),
            secondary_color: role.colours.secondary_colour.map(|c| c.0),
            icon_url: role.icon.as_ref().map(|h| cdn_url(&format!("/role-icons/{}", role.id), &h.to_string(), 256)),
            position: guild.and_then(|g| permissions::role_position(g, role.id)).map_or(i64::from(role.position), |p| p as i64),
            created_ms: adapters::snowflake_ms(role.id.get()) as i64,
            hoist: role.hoist,
            mentionable: role.mentionable,
            managed: role.managed,
            tags: has_tags.then_some(tags),
            permissions: role.permissions,
        }
    }
}

fn fmt_color(c: u32) -> String {
    bot_core::custom_embed::format_color(&json!(c))
}

/// `buildRoleEmbed(guild, role, settings)` from the view; `member_text` is the members field (see
/// [`role_member_text`]), `role_count` is `guild.roles.cache.size`.
pub fn build_role_embed(guild_id: &str, role_count: usize, role: &RoleView, member_text: &str, settings: Option<&GuildSettings>) -> CreateEmbed {
    let title = match &role.unicode_emoji {
        Some(e) => format!("{e} {}", role.name),
        None => role.name.clone(),
    };
    let description = if role.id == guild_id { "@everyone".to_string() } else { format!("<@&{}>", role.id) };
    let mut embed =
        guild_embed(settings, Some(&clamp_text(&title, 256)), Some(&description), None).footer(CreateEmbedFooter::new(format!("Role ID: {}", role.id)));
    if role.primary_color > 0 {
        embed = embed.color(role.primary_color);
    }
    if let Some(icon) = &role.icon_url {
        embed = embed.thumbnail(icon);
    }
    let colour = if role.primary_color == 0 {
        "Default".to_string()
    } else {
        match role.secondary_color.filter(|c| *c != 0) {
            Some(s) => format!("{} → {}", fmt_color(role.primary_color), fmt_color(s)),
            None => fmt_color(role.primary_color),
        }
    };
    embed = embed
        .field("🎨 Colour", colour, true)
        .field("👥 Members", member_text, true)
        .field("📊 Position", format!("{} of {}", role.position, role_count as i64 - 1), true)
        .field("📅 Created", ts(role.created_ms), true)
        .field("⚙️ Settings", format!("Hoisted: {}\nMentionable: {}\nManaged: {}", yes(role.hoist), yes(role.mentionable), yes(role.managed)), true);
    if role.managed || role.tags.is_some() {
        let tags = role.tags.clone().unwrap_or_default();
        let managed_by = if let Some(b) = &tags.bot_id {
            format!("<@{b}> (bot role)")
        } else if tags.premium_subscriber_role {
            "Server Boosts".into()
        } else if tags.subscription_listing_id.is_some() {
            "Server Subscription".into()
        } else if tags.guild_connections {
            "Linked Roles".into()
        } else if tags.integration_id.is_some() {
            "An integration".into()
        } else {
            "Unknown".into()
        };
        embed = embed.field("🔗 Managed by", managed_by, true);
    }
    let perms = key_permission_names(role.permissions);
    let perms_text = if perms.is_empty() { "None".to_string() } else { join_with_limit(&perms, ", ", 1024, default_more) };
    embed.field("🔑 Key permissions", perms_text, false)
}

/// The members field of `/info role`: `@everyone` is the member count; another role is its REST count, or the cached
/// members holding it when the counts are unavailable.
pub fn role_member_text(is_everyone: bool, member_count: u64, counts: Result<&[(String, f64)], ()>, role_id: &str, cached_holders: usize) -> String {
    if is_everyone {
        return format_number(member_count as f64);
    }
    match counts {
        Ok(c) => format_number(c.iter().find(|(id, _)| id == role_id).map_or(0.0, |(_, n)| *n)),
        Err(()) => format!("{} (cached members only)", format_number(cached_holders as f64)),
    }
}

/// What `/info user` reads of the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserView {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub bot: bool,
    pub created_ms: i64,
    /// `user.displayAvatarURL({ size: 256 })`.
    pub avatar_url: String,
    /// `user.flags` bits (`None` when unknown).
    pub flags: Option<u64>,
}

impl UserView {
    pub fn of(user: &User) -> Self {
        Self {
            id: user.id.to_string(),
            username: user.name.clone(),
            global_name: user.global_name.clone(),
            bot: user.bot,
            created_ms: adapters::snowflake_ms(user.id.get()) as i64,
            avatar_url: adapters::user_display_avatar_url_with(user, adapters::ImageUrlOptions::new("webp", Some(256))),
            flags: user.public_flags.map(|f| u64::from(f.bits())),
        }
    }
}

/// What `/info user` reads of the member.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberView {
    pub display_name: String,
    pub display_color: u32,
    /// `member.displayAvatarURL({ size: 256 })`.
    pub avatar_url: String,
    pub joined_ms: Option<i64>,
    pub premium_since_ms: Option<i64>,
    pub timeout_until_ms: Option<i64>,
    /// `member.roles.cache` as (id, position), `@everyone` included.
    pub roles: Vec<(String, i64)>,
    /// Guild-level permissions (`member.permissions`).
    pub permissions: Permissions,
    /// `resolvePermissionLevel(member, settings)`.
    pub level: PermissionLevel,
    /// `member.id === guild.ownerId`.
    pub is_guild_owner: bool,
}

impl MemberView {
    /// From a member of a cached guild.
    pub fn of(app: &App, guild: &Guild, member: &Member, settings: Option<&GuildSettings>) -> Self {
        let role_ids: Vec<RoleId> = bot_core::interaction_helpers::member_role_ids(guild, member);
        let roles: Vec<(String, i64)> =
            role_ids.iter().filter_map(|r| permissions::role_position(guild, *r).map(|p| (r.to_string(), p as i64))).collect();
        // discord.js displayColor: the highest role (position, then the lower id) with a colour.
        let display_color = role_ids
            .iter()
            .filter_map(|r| guild.roles.get(r))
            .filter(|r| role_primary(r) != 0)
            .max_by(|a, b| a.position.cmp(&b.position).then_with(|| b.id.cmp(&a.id)))
            .map_or(0, role_primary);
        let member_roles: Vec<RoleId> = member.roles.clone();
        let perms = permissions::guild_permissions(guild, member.user.id, &member_roles);
        let access = MemberAccess {
            user_id: member.user.id.to_string(),
            guild_owner_id: Some(guild.owner_id.to_string()),
            permissions: Some(perms),
            role_ids: member.roles.iter().map(|r| r.to_string()).collect(),
        };
        let level = resolve_permission_level(Some(&access), settings, app.config().bot_owner_id.as_deref());
        Self {
            display_name: adapters::member_display_name(member),
            display_color,
            avatar_url: adapters::member_display_avatar_url_with(guild.id, member, adapters::ImageUrlOptions::new("webp", Some(256))),
            joined_ms: member.joined_at.map(|t| t.timestamp_millis()),
            premium_since_ms: member.premium_since.map(|t| t.timestamp_millis()),
            timeout_until_ms: member.communication_disabled_until.map(|t| t.timestamp_millis()),
            roles,
            permissions: perms,
            level,
            is_guild_owner: member.user.id == guild.owner_id,
        }
    }
}

/// `buildUserEmbed(interaction, settings)` from the views. `viewer_is_moderator`: the invoker's level is at least
/// MODERATOR (the timeout is moderation information).
pub fn build_user_embed(
    guild_id: &str,
    user: &UserView,
    member: Option<&MemberView>,
    viewer_is_moderator: bool,
    now_ms: i64,
    settings: Option<&GuildSettings>,
) -> CreateEmbed {
    let display_name = member.map(|m| m.display_name.clone()).unwrap_or_else(|| user.global_name.clone().unwrap_or_else(|| user.username.clone()));
    let mut embed = guild_embed(settings, Some(&clamp_text(&display_name, 256)), None, None)
        .thumbnail(member.map_or(user.avatar_url.as_str(), |m| m.avatar_url.as_str()))
        .footer(CreateEmbedFooter::new(format!("User ID: {}", user.id)));
    if let Some(m) = member.filter(|m| m.display_color > 0) {
        embed = embed.color(m.display_color);
    }
    let user_lines = [
        format!("**Mention:** <@{}>", user.id),
        format!("**Username:** {}", esc(&user.username)),
        format!("**Display name:** {}", esc(&display_name)),
        format!("**Bot:** {}", yes(user.bot)),
    ]
    .join("\n");
    embed = embed.field("👤 User", user_lines, true).field("📅 Account created", ts(user.created_ms), true).field(
        "📥 Joined server",
        match member.and_then(|m| m.joined_ms) {
            Some(j) => ts(j),
            None => "Not a member of this server".into(),
        },
        true,
    );
    if let Some(p) = member.and_then(|m| m.premium_since_ms).filter(|p| *p != 0) {
        embed = embed.field("💎 Boosting since", ts(p), true);
    }
    if let Some(until) = member.and_then(|m| m.timeout_until_ms).filter(|u| *u > now_ms)
        && viewer_is_moderator
    {
        embed = embed.field("🔇 Timed out until", ts(until), true);
    }
    let badges: Vec<&str> = USER_BADGES.iter().filter(|(_, bit, _)| user.flags.is_some_and(|f| f & bit != 0)).map(|(_, _, label)| *label).collect();
    if !badges.is_empty() {
        embed = embed.field("🏅 Badges", clamp_text(&badges.join(", "), 1024), false);
    }
    if let Some(m) = member {
        let role_count = m.roles.iter().filter(|(id, _)| id != guild_id).count();
        embed = embed.field(format!("🎭 Roles [{role_count}]"), format_role_mentions(&m.roles, guild_id, 1024), false);
        let perms = key_permission_names(m.permissions);
        let perms_text = if perms.is_empty() { "None".to_string() } else { join_with_limit(&perms, ", ", 1024, default_more) };
        embed = embed
            .field("🔑 Key permissions", perms_text, false)
            .field("🛡️ Access level", access_level_label(m.level, m.is_guild_owner), true);
    }
    embed
}

// ── Command ────────────────────────────────────────────────────────────────────

async fn fetch_member(app: &App, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    if let Some(m) = with_cached_guild(app, guild_id, |g| g.members.get(&user_id).cloned()).flatten() {
        return Some(m);
    }
    let http = app.http()?;
    bot_util::async_util::with_timeout(
        async move { http.get_member(guild_id, user_id).await.map_err(|e| e.to_string()) },
        REST_TIMEOUT_MS,
        "info member fetch",
    )
    .await
    .ok()
}

async fn user_embed(ctx: &CommandCtx, guild_id: GuildId, settings: Option<&GuildSettings>) -> anyhow::Result<CreateEmbed> {
    let user = ctx.get_user("member").unwrap_or_else(|| ctx.user().clone());
    let member = fetch_member(&ctx.app, guild_id, user.id).await;
    let member_view = match &member {
        Some(m) => with_cached_guild(&ctx.app, guild_id, |g| MemberView::of(&ctx.app, g, m, settings)),
        None => None,
    };
    let viewer = resolve_permission_level(ctx.member_access().as_ref(), settings, ctx.app.config().bot_owner_id.as_deref());
    // The member as fetched carries the freshest user (flags included when Discord sent them).
    let mut user_view = UserView::of(&user);
    if user_view.flags.is_none()
        && let Some(f) = member.as_ref().and_then(|m| m.user.public_flags)
    {
        user_view.flags = Some(u64::from(f.bits()));
    }
    Ok(build_user_embed(
        &guild_id.to_string(),
        &user_view,
        member_view.as_ref(),
        viewer >= PermissionLevel::Moderator,
        ctx.app.clock().now_ms(),
        settings,
    ))
}

async fn server_embed(ctx: &CommandCtx, guild_id: GuildId, settings: Option<&GuildSettings>) -> anyhow::Result<CreateEmbed> {
    let view = with_cached_guild(&ctx.app, guild_id, ServerView::of).ok_or_else(|| anyhow::anyhow!("guild {guild_id} is not cached"))?;
    let counts = get_guild_counts(&ctx.app, guild_id, view.member_count).await;
    Ok(build_server_embed(&view, &counts, settings))
}

async fn role_embed(ctx: &CommandCtx, guild_id: GuildId, settings: Option<&GuildSettings>) -> anyhow::Result<CreateEmbed> {
    let picked = option_role(ctx, "role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let (view, role_count, member_count, holders) = with_cached_guild(&ctx.app, guild_id, |g| {
        let role = g.roles.get(&picked.id).cloned().unwrap_or_else(|| picked.clone());
        let holders = g.members.values().filter(|m| m.roles.contains(&picked.id)).count();
        (RoleView::of(&role, Some(g)), g.roles.len(), g.member_count, holders)
    })
    .ok_or_else(|| anyhow::anyhow!("guild {guild_id} is not cached"))?;
    let g = guild_id.to_string();
    let is_everyone = view.id == g;
    let counts = if is_everyone { Ok(Vec::new()) } else { get_role_counts(&ctx.app, guild_id).await };
    if let Err(e) = &counts {
        bot_util::log_debug!(format!("info: role counts unavailable: {e}"));
    }
    let text = role_member_text(is_everyone, member_count, counts.as_deref().map_err(|_| ()), &view.id, holders);
    Ok(build_role_embed(&g, role_count, &view, &text, settings))
}

/// `options.getRole(name)`: the resolved role Discord sent, else the cached role with that id.
pub(crate) fn option_role(ctx: &CommandCtx, name: &str) -> Option<Role> {
    ctx.get_role(name).or_else(|| {
        let (g, r) = (ctx.guild_id()?, ctx.get_role_id(name)?);
        with_cached_guild(&ctx.app, g, |guild| guild.roles.get(&r).cloned()).flatten()
    })
}

async fn reply_embed(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id().filter(|g| with_cached_guild(&ctx.app, *g, |_| ()).is_some()) else {
        reply_embed(&ctx, error_embed("Server Only", Some("Use this command in a server."), None)).await;
        return Ok(());
    };
    // No settings is fine: the default colour, no moderator roles.
    let settings = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    let built = match ctx.subcommand().as_deref() {
        Some("user") => user_embed(&ctx, guild_id, settings.as_deref()).await,
        Some("server") => server_embed(&ctx, guild_id, settings.as_deref()).await,
        Some("role") => role_embed(&ctx, guild_id, settings.as_deref()).await,
        _ => {
            reply_embed(&ctx, error_embed("Unknown Subcommand", Some("Use `/info user`, `/info server` or `/info role`."), None)).await;
            return Ok(());
        }
    };
    match built {
        Ok(embed) => {
            safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
        }
        Err(e) => {
            bot_util::log_error!("info failed", bot_core::error::log_arg(&e));
            reply_embed(&ctx, error_embed("Info Unavailable", Some("I could not load that information. Please try again."), None)).await;
        }
    }
    Ok(())
}

/// `role.colors?.primaryColor ?? role.color` (serenity fills the gradient colours with 0 when Discord sent none).
fn role_primary(role: &Role) -> u32 {
    if role.colours.primary_colour.0 != 0 { role.colours.primary_colour.0 } else { role.colour.0 }
}
