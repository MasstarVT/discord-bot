//! Port of `src/services/tempVoiceService.js`: temporary voice channels (join-to-create hubs, owner controls through
//! `/voice` and a persistent control panel, keep-alive deletion, ownership claims, bans, a companion text channel and
//! startup reconciliation; MEE6 "Temporary Channels").
//!
//! Everything lives here, as in the JS: constants and pure helpers, the guild routing index, the per-hub and
//! per-channel locks, every flow, the persistent component handlers, the scheduler job body, the channelDelete /
//! guildReady / voiceStateUpdate hook bodies and the CRUD `/voice-hub` and the dashboard use. Module-level state of the
//! JS is the per-app [`TempVoiceState`] (`app.ext`).
//!
//! Rule 20 (a cache miss is not a deletion): a row is purged only after a delete event, or after a REST fetch or delete
//! answered 10003 while the guild is available ([`confirm_channel`]). Any other failure keeps the row.
//!
//! Discord REST calls go through [`crate::discord_port::VoiceRest`] (the JS `discordCall`); reads come from the serenity
//! cache.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::deletion_tracker::mark_bot_deletion;
use bot_core::discord::GuildState;
use bot_core::embed_builder::{apply_guild_color, error_embed, info_embed, neutral_embed, success_embed, warn_embed};
use bot_core::interaction::{ComponentCtx, ModalCtx, Responder};
use bot_core::interaction_helpers::{PermTarget, Payload, check_channel_usable, is_interaction_gone, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::markdown::escape_markdown;
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_core::permissions::permission_names;
use bot_core::scheduler::{CancelJobs, OnConflict, ScheduleJob};
use bot_core::settings::Settings;
use bot_core::sync::KeyedMutex;
use bot_config::{JobType, PermissionLevel, limits};
use bot_db::{
    DbError, InsertBuilder, SelectBuilder, TEMP_VOICE_CHANNEL_INFO, TEMP_VOICE_HUB_INFO, TempVoiceChannel, TempVoiceHub, TxOptions,
    UpdateBuilder, delete_where, format_js_date,
};
use bot_util::placeholder_parser::{PlaceholderGuild, PlaceholderMember, RenderContext, VarValue, render_placeholders, render_placeholders_detailed};
use bot_util::text::{clamp_text, join_with_limit};
use bot_util::time::discord_timestamp_ms;
use bot_util::{js, log_debug, log_error, log_info, log_warn};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use serde_json::{Value, json};
use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, Guild, GuildChannel, GuildId, InputTextStyle, Member, MessageId, ReactionType, RoleId, UserId,
};
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed, CreateInputText, CreateModal, CreateSelectMenu, CreateSelectMenuKind};

use crate::discord_port::{NewChannel, Overwrite, RestError, rest};

// ── Constants (spec §6.1) ─────────────────────────────────────────────────────

pub const DEFAULT_NAME_FORMAT: &str = "#{index} - {username}'s Channel";
pub const HUB_CHANNEL_NAME: &str = "➕ Join to Create";
pub const MAX_HUB_ROLE_LIST: usize = 10;
pub const MAX_BANS_PER_CHANNEL: usize = 50;
pub const RENAME_WINDOW_MS: i64 = 600_000;
pub const RENAME_MAX: usize = 2;
pub const RENAME_TIMEOUT_MS: u64 = 8_000;
pub const CREATE_COOLDOWN_MS: i64 = 5_000;
pub const INDEX_TTL_MS: i64 = 300_000;
pub const TEXT_SYNC_DELAY_MS: u64 = 1_500;
pub const DELETE_RETRY_MS: i64 = 5 * 60_000;
pub const CLEANUP_JOB: &str = "tempvoice.cleanup";

/// `cleanupKey(channelId)`: the dedupe key of the keep-alive job.
pub fn cleanup_key(channel_id: &str) -> String {
    format!("tempvoice.cleanup:{channel_id}")
}

/// `renameKey(channelId)`: the Redis key of the rename stamps.
pub fn rename_key(channel_id: &str) -> String {
    format!("tempvoice:rename:{channel_id}")
}

const VIEW_CHANNEL: u64 = 1 << 10;
const CONNECT: u64 = 1 << 20;
const MANAGE_CHANNELS: u64 = 1 << 4;
const MOVE_MEMBERS: u64 = 1 << 24;
const PRIORITY_SPEAKER: u64 = 1 << 8;
const SEND_MESSAGES: u64 = 1 << 11;
const EMBED_LINKS: u64 = 1 << 14;
const READ_MESSAGE_HISTORY: u64 = 1 << 16;
const MANAGE_MESSAGES: u64 = 1 << 13;
const PIN_MESSAGES: u64 = 1 << 51;
const MANAGE_ROLES: u64 = 1 << 28;
const ADMINISTRATOR: u64 = 1 << 3;

/// `OWNER_PERMS` (17827088).
pub const OWNER_PERMS: u64 = MANAGE_CHANNELS | MOVE_MEMBERS | PRIORITY_SPEAKER | CONNECT | VIEW_CHANNEL;
/// `BOT_VOICE_PERMS` (17910800).
pub const BOT_VOICE_PERMS: u64 = VIEW_CHANNEL | CONNECT | MANAGE_CHANNELS | MOVE_MEMBERS | SEND_MESSAGES | EMBED_LINKS | READ_MESSAGE_HISTORY;
/// `BOT_TEXT_PERMS`.
pub const BOT_TEXT_PERMS: u64 =
    VIEW_CHANNEL | SEND_MESSAGES | EMBED_LINKS | READ_MESSAGE_HISTORY | MANAGE_MESSAGES | PIN_MESSAGES | MANAGE_CHANNELS;
/// Permission names for `missingBotPermissions`.
pub const REQUIRED_BOT_PERMS: [&str; 5] = ["ManageChannels", "ManageRoles", "MoveMembers", "ViewChannel", "Connect"];
pub const VOICE_ACTIONS: [&str; 11] = ["ban", "claim", "hide", "kick", "limit", "lock", "rename", "reveal", "transfer", "unban", "unlock"];
pub const BUTTON_ACTIONS: [&str; 10] = ["lock", "unlock", "hide", "reveal", "claim", "limit", "rename", "kick", "ban", "transfer"];
pub const SELECT_ACTIONS: [&str; 4] = ["kick", "ban", "unban", "transfer"];
pub const MODAL_ACTIONS: [&str; 2] = ["limit", "rename"];

const INDEX_MEMO_MAX: usize = 10_000;
const RENAME_STAMPS_MAX: usize = 5_000;
const CREATE_PRUNE_MS: i64 = 60_000;
const HUB_CHANNEL_PERMS: [&str; 3] = ["ViewChannel", "Connect", "MoveMembers"];

/// `SNOWFLAKE_RE` (`/^\d{17,20}$/`).
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `HubLimitError`: the per-server hub cap was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubLimitError;

impl std::fmt::Display for HubLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "A server can have at most {} hubs", limits::TEMPVOICE_HUBS)
    }
}

impl std::error::Error for HubLimitError {}

/// Whether a `createHub` error is Prisma's P2002 (the channel is already a hub).
pub fn is_duplicate_hub(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
}

// ── Raw SQL (rule 22) ─────────────────────────────────────────────────────────

/// `guildSettingsLockSql(guildId)`.
pub const GUILD_LOCK_SQL: &str = r#"SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#;
/// `hubLockSql(hubId, guildId)`.
pub const HUB_LOCK_SQL: &str = r#"SELECT 1 AS ok FROM "TempVoiceHub" WHERE "id" = $1 AND "guildId" = $2 FOR UPDATE"#;

/// `RAW_SQL`: (name, statement, binds of the JS builder's sample call).
pub const RAW_SQL: [(&str, &str, &[&str]); 2] =
    [("tempvoice.guildLock", GUILD_LOCK_SQL, &["0"]), ("tempvoice.hubLock", HUB_LOCK_SQL, &["0", "0"])];

// ── Messages (spec §4.3) ──────────────────────────────────────────────────────

/// `describeDiscordError(code)`.
pub fn describe_discord_error(code: Option<u64>) -> String {
    match code {
        Some(50013) => "missing permissions".into(),
        Some(50001) => "no access".into(),
        Some(10003) => "channel not found".into(),
        Some(30013) => "the server has 500 channels".into(),
        Some(50035) => "invalid request (is the category full?)".into(),
        Some(40032) => "member not connected".into(),
        Some(c) => format!("error {c}"),
        None => "error unknown".into(),
    }
}

/// The params of an [`Msg`] embed: `target` / `owner` (user ids), `n`, `at` / `retry_at` (ms), `names` (permission
/// display names), `channel_id`, `code`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MsgParams {
    pub target: Option<String>,
    pub owner: Option<String>,
    pub n: Option<i64>,
    pub at: Option<f64>,
    pub retry_at: Option<f64>,
    pub names: Vec<String>,
    pub channel_id: Option<String>,
    pub code: Option<u64>,
}

/// The keys of `MSG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Msg {
    NotInTemp,
    ChannelGone,
    NotAllowed,
    CleanNotAllowed,
    TargetSelf,
    TargetOwner,
    TargetProtected,
    TargetNotMember,
    TargetNotConnected,
    TargetBot,
    TargetAlreadyOwner,
    AlreadyBanned,
    NotBanned,
    BanLimit,
    NoChangeLock,
    NoChangeUnlock,
    NoChangeHide,
    NoChangeReveal,
    NoChangeName,
    NoChangeLimit,
    ClaimAlreadyOwner,
    ClaimNotConnected,
    ClaimOwnerPresent,
    ClaimLocked,
    RenameInvalid,
    RenameRate,
    RenameQueued,
    LimitInvalid,
    BotMissing,
    DiscordError,
    DbError,
    HubGone,
    Internal,
    NotInGuild,
    TryAgain,
}

fn ping(id: Option<&str>) -> String {
    format!("<@{}>", id.unwrap_or("undefined"))
}

impl Msg {
    pub const ALL: [Msg; 35] = [
        Msg::NotInTemp,
        Msg::ChannelGone,
        Msg::NotAllowed,
        Msg::CleanNotAllowed,
        Msg::TargetSelf,
        Msg::TargetOwner,
        Msg::TargetProtected,
        Msg::TargetNotMember,
        Msg::TargetNotConnected,
        Msg::TargetBot,
        Msg::TargetAlreadyOwner,
        Msg::AlreadyBanned,
        Msg::NotBanned,
        Msg::BanLimit,
        Msg::NoChangeLock,
        Msg::NoChangeUnlock,
        Msg::NoChangeHide,
        Msg::NoChangeReveal,
        Msg::NoChangeName,
        Msg::NoChangeLimit,
        Msg::ClaimAlreadyOwner,
        Msg::ClaimNotConnected,
        Msg::ClaimOwnerPresent,
        Msg::ClaimLocked,
        Msg::RenameInvalid,
        Msg::RenameRate,
        Msg::RenameQueued,
        Msg::LimitInvalid,
        Msg::BotMissing,
        Msg::DiscordError,
        Msg::DbError,
        Msg::HubGone,
        Msg::Internal,
        Msg::NotInGuild,
        Msg::TryAgain,
    ];

    /// The JS key (`NOT_IN_TEMP`, ...).
    pub fn key(self) -> &'static str {
        match self {
            Msg::NotInTemp => "NOT_IN_TEMP",
            Msg::ChannelGone => "CHANNEL_GONE",
            Msg::NotAllowed => "NOT_ALLOWED",
            Msg::CleanNotAllowed => "CLEAN_NOT_ALLOWED",
            Msg::TargetSelf => "TARGET_SELF",
            Msg::TargetOwner => "TARGET_OWNER",
            Msg::TargetProtected => "TARGET_PROTECTED",
            Msg::TargetNotMember => "TARGET_NOT_MEMBER",
            Msg::TargetNotConnected => "TARGET_NOT_CONNECTED",
            Msg::TargetBot => "TARGET_BOT",
            Msg::TargetAlreadyOwner => "TARGET_ALREADY_OWNER",
            Msg::AlreadyBanned => "ALREADY_BANNED",
            Msg::NotBanned => "NOT_BANNED",
            Msg::BanLimit => "BAN_LIMIT",
            Msg::NoChangeLock => "NO_CHANGE_LOCK",
            Msg::NoChangeUnlock => "NO_CHANGE_UNLOCK",
            Msg::NoChangeHide => "NO_CHANGE_HIDE",
            Msg::NoChangeReveal => "NO_CHANGE_REVEAL",
            Msg::NoChangeName => "NO_CHANGE_NAME",
            Msg::NoChangeLimit => "NO_CHANGE_LIMIT",
            Msg::ClaimAlreadyOwner => "CLAIM_ALREADY_OWNER",
            Msg::ClaimNotConnected => "CLAIM_NOT_CONNECTED",
            Msg::ClaimOwnerPresent => "CLAIM_OWNER_PRESENT",
            Msg::ClaimLocked => "CLAIM_LOCKED",
            Msg::RenameInvalid => "RENAME_INVALID",
            Msg::RenameRate => "RENAME_RATE",
            Msg::RenameQueued => "RENAME_QUEUED",
            Msg::LimitInvalid => "LIMIT_INVALID",
            Msg::BotMissing => "BOT_MISSING",
            Msg::DiscordError => "DISCORD_ERROR",
            Msg::DbError => "DB_ERROR",
            Msg::HubGone => "HUB_GONE",
            Msg::Internal => "INTERNAL",
            Msg::NotInGuild => "NOT_IN_GUILD",
            Msg::TryAgain => "TRY_AGAIN",
        }
    }

    /// `(kind, title, text)` of the message.
    pub fn parts(self, p: &MsgParams) -> (&'static str, &'static str, String) {
        let target = || ping(p.target.as_deref());
        match self {
            Msg::NotInTemp => ("error", "Not In A Temporary Channel", "Join your temporary voice channel first, then run this command.".into()),
            Msg::ChannelGone => ("error", "Channel Gone", "This temporary channel no longer exists.".into()),
            Msg::NotAllowed => ("error", "Not Your Channel", "Only the channel owner or a temp-voice moderator can do that.".into()),
            Msg::CleanNotAllowed => ("error", "Insufficient Permissions", "Only moderators can clean up temporary channels.".into()),
            Msg::TargetSelf => ("error", "Invalid Member", "You can't do that to yourself.".into()),
            Msg::TargetOwner => ("error", "Invalid Member", format!("{} owns this channel. Transfer ownership first.", target())),
            Msg::TargetProtected => (
                "error",
                "Protected Member",
                format!("{} is a moderator or has an ignored role, so /voice commands don't affect them.", target()),
            ),
            Msg::TargetNotMember => ("error", "Invalid Member", "That user is not in this server.".into()),
            Msg::TargetNotConnected => ("error", "Not In Channel", format!("{} is not in this voice channel.", target())),
            Msg::TargetBot => ("error", "Invalid Member", "Bots can't own a channel.".into()),
            Msg::TargetAlreadyOwner => ("warn", "No Change", format!("{} already owns this channel.", target())),
            Msg::AlreadyBanned => ("warn", "No Change", format!("{} is already banned from this channel.", target())),
            Msg::NotBanned => ("warn", "No Change", format!("{} is not banned from this channel.", target())),
            Msg::BanLimit => ("error", "Limit Reached", format!("A channel can ban at most {MAX_BANS_PER_CHANNEL} members.")),
            Msg::NoChangeLock => ("warn", "No Change", "This channel is already locked.".into()),
            Msg::NoChangeUnlock => ("warn", "No Change", "This channel is already unlocked.".into()),
            Msg::NoChangeHide => ("warn", "No Change", "This channel is already hidden.".into()),
            Msg::NoChangeReveal => ("warn", "No Change", "This channel is already visible.".into()),
            Msg::NoChangeName => ("warn", "No Change", "That is already the channel's name.".into()),
            Msg::NoChangeLimit => {
                let n = match p.n {
                    Some(n) if n != 0 => n.to_string(),
                    _ => "off".into(),
                };
                ("warn", "No Change", format!("The member limit is already {n}."))
            }
            Msg::ClaimAlreadyOwner => ("warn", "No Change", "You already own this channel.".into()),
            Msg::ClaimNotConnected => ("error", "Not In Channel", "Join the channel first, then claim it.".into()),
            Msg::ClaimOwnerPresent => ("error", "Owner Still Here", format!("{} is still in the channel.", ping(p.owner.as_deref()))),
            Msg::ClaimLocked => {
                ("error", "Not Yet", format!("This channel can be claimed {}.", discord_timestamp_ms(p.at.unwrap_or(f64::NAN), "R")))
            }
            Msg::RenameInvalid => ("error", "Invalid Name", "Channel names must be 1–100 characters.".into()),
            Msg::RenameRate => (
                "error",
                "Slow Down",
                format!(
                    "Discord allows only 2 renames per channel every 10 minutes. Try again {}.",
                    discord_timestamp_ms(p.retry_at.unwrap_or(f64::NAN), "R")
                ),
            ),
            Msg::RenameQueued => (
                "warn",
                "Rename Queued",
                "Discord is rate-limiting renames of this channel. The new name will apply when the limit ends (up to 10 minutes).".into(),
            ),
            Msg::LimitInvalid => ("error", "Invalid Limit", "Enter a whole number from 0 to 99 (0 = no limit).".into()),
            Msg::BotMissing => (
                "error",
                "Missing Permissions",
                format!("I need **{}** in <#{}> to do that.", p.names.join(", "), p.channel_id.as_deref().unwrap_or("undefined")),
            ),
            Msg::DiscordError => (
                "error",
                "Something Went Wrong",
                format!("Discord rejected that change ({}). Please try again in a moment.", describe_discord_error(p.code)),
            ),
            Msg::DbError => ("error", "Something Went Wrong", "The channel changed, but I could not save it. Please try again.".into()),
            Msg::HubGone => (
                "error",
                "Hub Deleted",
                "The hub this channel came from was deleted, so its permissions cannot be changed safely any more. Lock, unlock, hide, reveal, ban, unban, transfer and claim are unavailable here; kick, limit and rename still work.".into(),
            ),
            Msg::Internal => ("error", "Something Went Wrong", "Something went wrong. Please try again in a moment.".into()),
            Msg::NotInGuild => ("error", "Server Only", "This only works in a server.".into()),
            Msg::TryAgain => ("error", "Something Went Wrong", "That took too long. Please try again.".into()),
        }
    }

    /// `MSG[key](params)`.
    pub fn embed(self, p: &MsgParams) -> CreateEmbed {
        let (kind, title, text) = self.parts(p);
        kind_embed(kind, title, &text)
    }

    /// `MSG[key]()`.
    pub fn plain(self) -> CreateEmbed {
        self.embed(&MsgParams::default())
    }
}

fn kind_embed(kind: &str, title: &str, text: &str) -> CreateEmbed {
    match kind {
        "error" => error_embed(title, Some(text), None),
        "warn" => warn_embed(title, Some(text), None),
        "info" => info_embed(title, Some(text), None),
        _ => success_embed(title, Some(text), None),
    }
}

fn channel_label(name: Option<&str>) -> String {
    format!("**{}**", escape_markdown(&clamp_text(name.unwrap_or("this channel"), 100)))
}

/// The params of [`ok_embed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OkParams {
    pub name: Option<String>,
    pub target: Option<String>,
    pub n: Option<i64>,
    pub new_name: Option<String>,
}

/// `okEmbed(action, p)`: the success embed of a voice action, as `(kind, title, text)`.
pub fn ok_parts(action: &str, p: &OkParams) -> (&'static str, &'static str, String) {
    let name = channel_label(p.name.as_deref());
    let target = ping(p.target.as_deref());
    match action {
        "lock" => ("success", "Channel Locked", format!("🔒 New members can no longer join {name}.")),
        "unlock" => ("success", "Channel Unlocked", format!("🔓 {name} is open to join again.")),
        "hide" => ("success", "Channel Hidden", format!("🙈 {name} is hidden from the channel list.")),
        "reveal" => ("success", "Channel Revealed", format!("👁️ {name} is visible again.")),
        "limit" => match p.n {
            Some(n) if n > 0 => ("success", "Limit Updated", format!("👥 {name} now allows **{n}** members.")),
            _ => ("success", "Limit Updated", format!("👥 {name} has no member limit now.")),
        },
        "rename" => ("success", "Channel Renamed", format!("✏️ The channel is now called {}.", channel_label(p.new_name.as_deref()))),
        "kick" => ("success", "Member Kicked", format!("👢 Disconnected {target} from {name}.")),
        "ban" => ("success", "Member Banned", format!("🔨 {target} can no longer join {name}.")),
        "unban" => ("success", "Member Unbanned", format!("✅ {target} can join {name} again.")),
        "transfer" => ("success", "Ownership Transferred", format!("👑 {target} now owns {name}.")),
        "claim" => ("success", "Channel Claimed", format!("👑 You now own {name}.")),
        "clean" => match p.n {
            Some(n) if n > 0 => ("success", "Channels Cleaned", format!("🧹 Deleted **{n}** empty temporary channel(s).")),
            _ => ("info", "Nothing To Clean", "There are no empty temporary channels.".into()),
        },
        _ => Msg::Internal.parts(&MsgParams::default()),
    }
}

/// `okEmbed(action, p)`.
pub fn ok_embed(action: &str, p: &OkParams) -> CreateEmbed {
    let (kind, title, text) = ok_parts(action, p);
    kind_embed(kind, title, &text)
}

// ── Pure helpers (spec §6.1) ──────────────────────────────────────────────────

/// `lowestFreeIndex(used)`: the smallest positive integer not in `used` (values <= 0 are ignored).
pub fn lowest_free_index(used: &[i64]) -> i64 {
    let taken: HashSet<i64> = used.iter().copied().filter(|n| *n > 0).collect();
    let mut i = 1;
    while taken.contains(&i) {
        i += 1;
    }
    i
}

/// Collapses runs of JS whitespace into one space.
fn collapse_js_space(s: &str, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if js::is_js_space(c) {
            if !in_space {
                out.push_str(with);
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `normalizeChannelName(input)`: collapses whitespace, trims, cuts to 100 code points; `None` for an empty result.
pub fn normalize_channel_name(input: Option<&str>) -> Option<String> {
    let input = input?;
    let collapsed = collapse_js_space(input, " ");
    let s = js::trim(&collapsed);
    if s.is_empty() {
        return None;
    }
    if s.chars().count() <= 100 {
        return Some(s.to_string());
    }
    let cut: String = s.chars().take(100).collect();
    Some(js::trim_end(&cut).to_string())
}

fn mention_token_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<(?:@[!&]?|#)[0-9]+>").unwrap_or_else(|e| panic!("valid regex: {e}")))
}

fn mention_check_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<[@#][!&]?[0-9]+>").unwrap_or_else(|e| panic!("valid regex: {e}")))
}

fn text_name_strip_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[^\p{L}\p{N}_-]").unwrap_or_else(|e| panic!("valid regex: {e}")))
}

/// Who a name format is rendered for: the member and guild placeholders and `{username}` (`member.displayName`).
#[derive(Debug, Clone, Default)]
pub struct NameContext {
    pub member: Option<PlaceholderMember>,
    pub guild: Option<PlaceholderGuild>,
}

impl NameContext {
    /// From a serenity member and its cached guild.
    pub fn of(member: &Member, guild: Option<&Guild>) -> Self {
        Self { member: Some(bot_core::adapters::placeholder_member(member, guild)), guild: guild.map(bot_core::adapters::placeholder_guild) }
    }

    fn render_ctx(&self, index: i64) -> RenderContext {
        let username = self.member.as_ref().and_then(|m| m.display_name.clone()).unwrap_or_default();
        RenderContext {
            member: self.member.clone(),
            guild: self.guild.clone(),
            vars: Some(vec![("index".into(), VarValue::Number(index as f64)), ("username".into(), VarValue::Text(username))]),
            ..Default::default()
        }
    }
}

/// `renderChannelName(format, { member, guild, index })`: never empty, never contains a mention token.
pub fn render_channel_name(format: &str, ctx: &NameContext, index: i64) -> String {
    let render = |fmt: &str| {
        let fmt = if fmt.is_empty() { DEFAULT_NAME_FORMAT } else { fmt };
        let text = render_placeholders(fmt, &ctx.render_ctx(index), Some(100));
        normalize_channel_name(Some(&mention_token_re().replace_all(&text, "")))
    };
    render(format).or_else(|| render(DEFAULT_NAME_FORMAT)).unwrap_or_else(|| format!("#{index}"))
}

/// The result of [`validate_name_format`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameFormatCheck {
    Ok { preview: String, warnings: Vec<String> },
    Err(String),
}

/// `validateNameFormat(format, { member, guild })`.
pub fn validate_name_format(format: Option<&str>, ctx: &NameContext) -> NameFormatCheck {
    let raw = js::trim(format.unwrap_or(""));
    let len = js::utf16_len(raw);
    if len == 0 || len > 100 {
        return NameFormatCheck::Err("Name formats must be 1–100 characters.".into());
    }
    let r = render_placeholders_detailed(raw, &ctx.render_ctx(1), Some(100));
    if mention_check_re().is_match(&r.text) {
        return NameFormatCheck::Err("Channel names can't contain mentions. Use {username} or {user.name} instead of {user}.".into());
    }
    let Some(preview) = normalize_channel_name(Some(&r.text)) else {
        return NameFormatCheck::Err("That format renders an empty name.".into());
    };
    let mut warnings = Vec::new();
    if !r.unknown_tokens.is_empty() {
        let tokens: Vec<String> = r
            .unknown_tokens
            .iter()
            .map(|t| if t.starts_with('{') && t.ends_with('}') { format!("`{t}`") } else { format!("`{{{t}}}`") })
            .collect();
        warnings.push(format!("Unknown placeholders are kept as typed: {}", tokens.join(", ")));
    }
    NameFormatCheck::Ok { preview, warnings }
}

/// `textChannelName(voiceName, index)`: a Discord text-channel name derived from the voice channel's name.
pub fn text_channel_name(voice_name: &str, index: i64) -> String {
    use unicode_normalization::UnicodeNormalization;
    let s: String = voice_name.nfkc().collect::<String>().to_lowercase();
    let s = collapse_js_space(&s, "-");
    let s = text_name_strip_re().replace_all(&s, "").into_owned();
    let mut collapsed = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    let trimmed = collapsed.trim_matches('-');
    let cut: String = trimmed.chars().take(100).collect();
    let cut = cut.trim_matches('-');
    if cut.is_empty() { format!("voice-{index}") } else { cut.to_string() }
}

/// `effectiveBitrate(stored, guildMax)`: the bitrate (bps) a new channel gets, or `None` for Discord's default.
pub fn effective_bitrate(stored: Option<i64>, guild_max: Option<i64>) -> Option<i64> {
    let stored = stored.filter(|s| *s > 0)?;
    let max = guild_max.filter(|m| *m > 0).unwrap_or(stored);
    Some(stored.min(max).max(8000))
}

/// `guild.maximumBitrate` (discord.js): 384 kbps with the VIP_REGIONS feature, else by boost tier.
pub fn maximum_bitrate(guild: &Guild) -> i64 {
    if guild.features.iter().any(|f| f == "VIP_REGIONS") {
        return 384_000;
    }
    match u8::from(guild.premium_tier) {
        1 => 128_000,
        2 => 256_000,
        3 => 384_000,
        _ => 96_000,
    }
}

/// `parseLimitInput(str)`: a whole number 0-99, or `None`.
pub fn parse_limit_input(s: &str) -> Option<i64> {
    let t = js::trim(s);
    if (1..=2).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_digit()) { t.parse().ok() } else { None }
}

/// The result of [`check_rename_window`].
#[derive(Debug, Clone, PartialEq)]
pub struct RenameWindow {
    pub ok: bool,
    pub retry_at: Option<f64>,
    pub recent: Vec<f64>,
}

/// `checkRenameWindow(stamps, now)`.
pub fn check_rename_window(stamps: &[f64], now: f64) -> RenameWindow {
    let mut recent: Vec<f64> = stamps.iter().copied().filter(|t| t.is_finite() && *t > now - RENAME_WINDOW_MS as f64 && *t <= now).collect();
    recent.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if recent.len() >= RENAME_MAX {
        let retry_at = recent[recent.len() - RENAME_MAX] + RENAME_WINDOW_MS as f64;
        return RenameWindow { ok: false, retry_at: Some(retry_at), recent };
    }
    RenameWindow { ok: true, retry_at: None, recent }
}

/// The input of [`claim_status`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaimInput {
    pub is_owner: bool,
    pub actor_connected: bool,
    pub owner_connected: bool,
    /// ms
    pub owner_left_at: Option<f64>,
    pub lock_minutes: f64,
    pub is_mod: bool,
    pub now: f64,
}

/// The result of [`claim_status`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimStatus {
    Ok,
    AlreadyOwner,
    NotConnected,
    OwnerPresent,
    Locked { claimable_at: f64 },
}

/// `claimStatus(...)`: whether `/voice claim` may succeed.
pub fn claim_status(c: &ClaimInput) -> ClaimStatus {
    if c.is_owner {
        return ClaimStatus::AlreadyOwner;
    }
    if !c.actor_connected {
        return ClaimStatus::NotConnected;
    }
    if c.owner_connected {
        return ClaimStatus::OwnerPresent;
    }
    let minutes = if c.lock_minutes.is_finite() { c.lock_minutes } else { 0.0 };
    let claimable_at = c.owner_left_at.unwrap_or(c.now) + minutes * 60_000.0;
    if !c.is_mod && c.now < claimable_at {
        return ClaimStatus::Locked { claimable_at };
    }
    ClaimStatus::Ok
}

/// The input of [`target_problem`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetInput<'a> {
    pub action: &'a str,
    pub actor_id: &'a str,
    pub target_id: &'a str,
    pub owner_id: &'a str,
    pub target_is_bot: bool,
    pub target_protected: bool,
    pub target_connected: bool,
    pub is_banned: bool,
    pub ban_count: usize,
}

/// `targetProblem(...)`: why a kick / ban / unban / transfer target is refused, or `None`.
pub fn target_problem(t: &TargetInput<'_>) -> Option<Msg> {
    match t.action {
        "kick" => {
            if t.target_id == t.actor_id {
                return Some(Msg::TargetSelf);
            }
            if t.target_id == t.owner_id {
                return Some(Msg::TargetOwner);
            }
            if t.target_protected {
                return Some(Msg::TargetProtected);
            }
            if !t.target_connected {
                return Some(Msg::TargetNotConnected);
            }
            None
        }
        "ban" => {
            if t.target_id == t.actor_id {
                return Some(Msg::TargetSelf);
            }
            if t.target_id == t.owner_id {
                return Some(Msg::TargetOwner);
            }
            if t.target_protected {
                return Some(Msg::TargetProtected);
            }
            if t.is_banned {
                return Some(Msg::AlreadyBanned);
            }
            if t.ban_count >= MAX_BANS_PER_CHANNEL {
                return Some(Msg::BanLimit);
            }
            None
        }
        "unban" => {
            if t.is_banned {
                None
            } else {
                Some(Msg::NotBanned)
            }
        }
        "transfer" => {
            if t.target_id == t.owner_id {
                return Some(Msg::TargetAlreadyOwner);
            }
            if t.target_is_bot {
                return Some(Msg::TargetBot);
            }
            if !t.target_connected {
                return Some(Msg::TargetNotConnected);
            }
            None
        }
        _ => None,
    }
}

/// `canControl({ action, isOwner, isMod })`: claim is open to anyone; everything else needs the owner or a temp-voice
/// moderator.
pub fn can_control(action: &str, is_owner: bool, is_mod: bool) -> bool {
    if action == "claim" {
        return true;
    }
    VOICE_ACTIONS.contains(&action) && (is_owner || is_mod)
}

fn has_any_role(roles: &[RoleId], ids: &[String]) -> bool {
    ids.iter().any(|id| roles.iter().any(|r| r.to_string() == *id))
}

fn member_level(app: &App, guild_id: GuildId, member: &Member, settings: Option<&bot_db::GuildSettings>) -> PermissionLevel {
    let cache = app.discord().map(|d| d.cache.clone());
    let access = MemberAccess::from_cache(cache.as_deref(), guild_id, member);
    resolve_permission_level(Some(&access), settings, app.config().bot_owner_id.as_deref())
}

/// `isTempVoiceMod(member, hub, settings)`: MODERATOR level or a hub moderator role.
pub fn is_temp_voice_mod(app: &App, guild_id: GuildId, member: &Member, hub: Option<&TempVoiceHub>, settings: Option<&bot_db::GuildSettings>) -> bool {
    member_level(app, guild_id, member, settings) >= PermissionLevel::Moderator
        || hub.is_some_and(|h| has_any_role(&member.roles, &h.moderator_role_ids))
}

/// `isProtectedTarget(member, hub, settings)`: moderators and members with an ignored role.
pub fn is_protected_target(app: &App, guild_id: GuildId, member: &Member, hub: Option<&TempVoiceHub>, settings: Option<&bot_db::GuildSettings>) -> bool {
    member_level(app, guild_id, member, settings) >= PermissionLevel::Moderator
        || hub.is_some_and(|h| {
            let all: Vec<String> = h.moderator_role_ids.iter().chain(h.ignored_role_ids.iter()).cloned().collect();
            has_any_role(&member.roles, &all)
        })
}

/// `exemptRoleIds(hub, settings, guild)`: ignored + moderator roles + the guild's mod roles that exist.
pub fn exempt_role_ids(hub: Option<&TempVoiceHub>, settings: Option<&bot_db::GuildSettings>, guild: Option<&Guild>) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    let mut push = |id: &String| {
        if !all.contains(id) {
            all.push(id.clone());
        }
    };
    if let Some(h) = hub {
        h.ignored_role_ids.iter().for_each(&mut push);
        h.moderator_role_ids.iter().for_each(&mut push);
    }
    if let Some(s) = settings {
        s.mod_role_ids.iter().for_each(&mut push);
    }
    let Some(guild) = guild else { return Vec::new() };
    all.into_iter()
        .filter(|id| *id != guild.id.to_string() && id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|v| guild.roles.contains_key(&RoleId::new(v))))
        .collect()
}

fn is_bot_user(guild: &Guild, user: UserId, state_member: Option<&Member>) -> bool {
    state_member.map(|m| m.user.bot).or_else(|| guild.members.get(&user).map(|m| m.user.bot)).unwrap_or(false)
}

/// The human (non-bot) members connected to a voice channel of a cached guild.
pub fn human_ids_in(guild: &Guild, channel_id: ChannelId) -> Vec<UserId> {
    let mut ids: Vec<UserId> = guild
        .voice_states
        .values()
        .filter(|v| v.channel_id == Some(channel_id) && !is_bot_user(guild, v.user_id, v.member.as_ref()))
        .map(|v| v.user_id)
        .collect();
    ids.sort();
    ids
}

/// `countHumans(channel)` for a channel of a cached guild.
pub fn count_humans_in(guild: &Guild, channel_id: ChannelId) -> usize {
    human_ids_in(guild, channel_id).len()
}

/// `countHumans(channel)`: human members connected to a voice channel (0 when the guild is not cached). Other crates
/// (events) call this.
pub fn count_humans(app: &App, guild_id: GuildId, channel_id: ChannelId) -> usize {
    with_guild(app, guild_id, |g| count_humans_in(g, channel_id)).unwrap_or(0)
}

/// Whether `user` is connected to `channel_id` (`channel.members.has(id)`).
fn connected_to(guild: &Guild, user: UserId, channel_id: ChannelId) -> bool {
    guild.voice_states.get(&user).is_some_and(|v| v.channel_id == Some(channel_id))
}

/// `snapshotOverwrites(channel)`.
pub fn snapshot_overwrites(channel: Option<&GuildChannel>) -> Vec<Overwrite> {
    let Some(ch) = channel else { return Vec::new() };
    ch.permission_overwrites
        .iter()
        .map(|o| {
            let (id, kind) = match o.kind {
                serenity::all::PermissionOverwriteType::Member(u) => (u.to_string(), 1),
                serenity::all::PermissionOverwriteType::Role(r) => (r.to_string(), 0),
                _ => (String::new(), 0),
            };
            Overwrite { id, kind, allow: o.allow.bits(), deny: o.deny.bits() }
        })
        .filter(|o| !o.id.is_empty())
        .collect()
}

/// What [`reconcile_decision`] decides for one row.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileAction {
    Purge,
    Occupied,
    Delete,
    Schedule { run_at: f64, set_empty_since: bool },
}

/// `reconcileDecision(...)`. `channel_exists` must come from [`confirm_channel`], never from a cache miss.
pub fn reconcile_decision(channel_exists: bool, humans: usize, keep_alive_minutes: i64, empty_since: Option<f64>, now: f64) -> ReconcileAction {
    if !channel_exists {
        return ReconcileAction::Purge;
    }
    if humans > 0 {
        return ReconcileAction::Occupied;
    }
    if keep_alive_minutes == 0 {
        return ReconcileAction::Delete;
    }
    let due = empty_since.unwrap_or(now) + keep_alive_minutes as f64 * 60_000.0;
    if due <= now {
        return ReconcileAction::Delete;
    }
    ReconcileAction::Schedule { run_at: due, set_empty_since: empty_since.is_none() }
}

// ── Overwrites (spec §6.10, §6.11; pure) ──────────────────────────────────────

struct OwMap(IndexMap<String, Overwrite>);

impl OwMap {
    fn new(template: &[Overwrite]) -> Self {
        let mut map = IndexMap::new();
        for o in template {
            map.insert(o.id.clone(), o.clone());
        }
        Self(map)
    }

    fn ensure(&mut self, id: &str, kind: u8) -> &mut Overwrite {
        self.0.entry(id.to_string()).or_insert_with(|| Overwrite { id: id.to_string(), kind, allow: 0, deny: 0 })
    }

    fn allow(&mut self, id: &str, kind: u8, bits: u64) {
        let o = self.ensure(id, kind);
        o.allow |= bits;
        o.deny &= !bits;
    }

    fn deny(&mut self, id: &str, kind: u8, bits: u64) {
        let o = self.ensure(id, kind);
        o.deny |= bits;
        o.allow &= !bits;
    }

    fn finish(self, grantable: Option<u64>) -> Vec<Overwrite> {
        let mut out = Vec::new();
        for (_, mut o) in self.0 {
            if let Some(g) = grantable {
                // Discord only lets a bot allow or deny permissions it has itself; Manage Permissions is never granted by
                // an overwrite.
                o.allow = o.allow & g & !MANAGE_ROLES;
                o.deny &= g;
            }
            if o.allow != 0 || o.deny != 0 {
                out.push(o);
            }
        }
        // JS compares the id strings.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// The input of [`compute_voice_overwrites`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceOverwriteInput {
    pub everyone_id: String,
    pub bot_id: String,
    pub owner_id: String,
    pub template: Vec<Overwrite>,
    pub locked: bool,
    pub hidden: bool,
    pub banned_user_ids: Vec<String>,
    pub exempt_role_ids: Vec<String>,
    /// `None` when the bot is Administrator (nothing is masked).
    pub grantable: Option<u64>,
}

/// `computeVoiceOverwrites(...)`: the whole overwrite list of a temporary voice channel, sorted by id, no empty
/// entries.
pub fn compute_voice_overwrites(i: &VoiceOverwriteInput) -> Vec<Overwrite> {
    let mut map = OwMap::new(&i.template);
    map.ensure(&i.everyone_id, 0);
    if i.hidden {
        map.deny(&i.everyone_id, 0, VIEW_CHANNEL);
    }
    if i.locked {
        map.deny(&i.everyone_id, 0, CONNECT);
    }
    if i.hidden || i.locked {
        // A template allow beats the @everyone deny, so while locked or hidden only exempt roles (and the owner) keep it.
        let stripped = (if i.hidden { VIEW_CHANNEL } else { 0 }) | (if i.locked { CONNECT } else { 0 });
        for o in map.0.values_mut() {
            if o.id != i.everyone_id && !i.exempt_role_ids.contains(&o.id) {
                o.allow &= !stripped;
            }
        }
        for role in &i.exempt_role_ids {
            if *role == i.everyone_id {
                continue;
            }
            if i.hidden {
                map.allow(role, 0, VIEW_CHANNEL);
            }
            if i.locked {
                map.allow(role, 0, CONNECT);
            }
        }
    }
    for user in &i.banned_user_ids {
        if *user == i.owner_id || *user == i.bot_id {
            continue;
        }
        map.deny(user, 1, CONNECT);
    }
    map.allow(&i.owner_id, 1, OWNER_PERMS);
    map.allow(&i.bot_id, 1, BOT_VOICE_PERMS);
    map.finish(i.grantable)
}

/// The input of [`compute_text_overwrites`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextOverwriteInput {
    pub everyone_id: String,
    pub bot_id: String,
    pub owner_id: String,
    pub template: Vec<Overwrite>,
    pub members_only: bool,
    pub member_ids: Vec<String>,
    pub banned_user_ids: Vec<String>,
    pub exempt_role_ids: Vec<String>,
    pub grantable: Option<u64>,
}

impl Default for TextOverwriteInput {
    fn default() -> Self {
        Self {
            everyone_id: String::new(),
            bot_id: String::new(),
            owner_id: String::new(),
            template: Vec::new(),
            members_only: true,
            member_ids: Vec::new(),
            banned_user_ids: Vec::new(),
            exempt_role_ids: Vec::new(),
            grantable: None,
        }
    }
}

/// `computeTextOverwrites(...)`: the companion text channel (members-only visibility mirrors the voice members).
pub fn compute_text_overwrites(i: &TextOverwriteInput) -> Vec<Overwrite> {
    let mut map = OwMap::new(&i.template);
    if i.members_only {
        map.deny(&i.everyone_id, 0, VIEW_CHANNEL);
        for role in &i.exempt_role_ids {
            if *role != i.everyone_id {
                map.allow(role, 0, VIEW_CHANNEL);
            }
        }
        for user in &i.member_ids {
            map.allow(user, 1, VIEW_CHANNEL);
        }
    } else {
        for user in &i.banned_user_ids {
            if *user == i.owner_id || *user == i.bot_id {
                continue;
            }
            map.deny(user, 1, VIEW_CHANNEL);
        }
    }
    map.allow(&i.bot_id, 1, BOT_TEXT_PERMS);
    map.finish(i.grantable)
}

/// `grantableBits(guild, scope)`: `None` when the bot is Administrator (or its permissions cannot be told), else the
/// bits the bot holds in `scope` (a channel) or the guild.
pub fn grantable_bits(app: &App, guild_id: GuildId, scope: Option<ChannelId>) -> Option<u64> {
    let target = match scope {
        Some(c) => PermTarget::Channel(guild_id, c),
        None => PermTarget::Guild(guild_id),
    };
    let bits = bot_core::interaction_helpers::bot_permissions(app, target)?.bits();
    if bits & ADMINISTRATOR != 0 { None } else { Some(bits) }
}

// ── Control panel, pickers and the value modal (spec §6.12; pure builders) ────

const PANEL_ROW_1: [(&str, &str, &str, ButtonStyle); 5] = [
    ("lock", "Lock", "🔒", ButtonStyle::Secondary),
    ("unlock", "Unlock", "🔓", ButtonStyle::Secondary),
    ("hide", "Hide", "🙈", ButtonStyle::Secondary),
    ("reveal", "Reveal", "👁️", ButtonStyle::Secondary),
    ("claim", "Claim", "👑", ButtonStyle::Primary),
];
const PANEL_ROW_2: [(&str, &str, &str, ButtonStyle); 5] = [
    ("limit", "Limit", "👥", ButtonStyle::Secondary),
    ("rename", "Rename", "✏️", ButtonStyle::Secondary),
    ("kick", "Kick", "👢", ButtonStyle::Secondary),
    ("ban", "Ban", "🔨", ButtonStyle::Danger),
    ("transfer", "Transfer", "🤝", ButtonStyle::Secondary),
];

fn cid(prefix: &str, params: &[&str]) -> String {
    bot_config::custom_ids::build_custom_id(prefix, params).unwrap_or_else(|e| panic!("customId {prefix}: {e}"))
}

/// The control panel's embed (`buildControlPanel(...).embeds[0]`).
pub fn control_panel_embed(owner_id: &str, settings: Option<&bot_db::GuildSettings>) -> CreateEmbed {
    let description = [
        format!("**Owner:** <@{owner_id}>"),
        String::new(),
        "Use the buttons below or the `/voice` commands:".into(),
        "🔒 **Lock** / 🔓 **Unlock**: stop or allow new members joining".into(),
        "🙈 **Hide** / 👁️ **Reveal**: hide the channel from the channel list".into(),
        "👑 **Claim**: take over after the owner leaves".into(),
        "👥 **Limit** · ✏️ **Rename** · 👢 **Kick** · 🔨 **Ban** · 🤝 **Transfer**".into(),
    ]
    .join("\n");
    apply_guild_color(
        neutral_embed(Some("🔊 Temporary channel"), Some(&description), Some("Only the owner and moderators can use these controls.")),
        settings,
    )
}

/// The control panel's two button rows.
pub fn control_panel_rows(channel_id: &str) -> Vec<CreateActionRow> {
    let row = |defs: &[(&str, &str, &str, ButtonStyle)]| {
        CreateActionRow::Buttons(
            defs.iter()
                .map(|(action, label, emoji, style)| {
                    CreateButton::new(cid("voice", &[action, channel_id]))
                        .label(*label)
                        .emoji(ReactionType::Unicode((*emoji).to_string()))
                        .style(*style)
                })
                .collect(),
        )
    };
    vec![row(&PANEL_ROW_1), row(&PANEL_ROW_2)]
}

/// `buildControlPanel({ channelId, ownerId, settings })`: `{ embeds, components, allowedMentions: { parse: [] } }`.
pub fn build_control_panel(channel_id: &str, owner_id: &str, settings: Option<&bot_db::GuildSettings>) -> Payload {
    Payload::embed(control_panel_embed(owner_id, settings)).components(control_panel_rows(channel_id)).no_mentions()
}

/// The content and select rows of [`build_member_picker`].
pub fn member_picker_parts(action: &str, channel_id: &str, banned_user_ids: &[String]) -> (String, Vec<CreateActionRow>) {
    let select = |act: &str, placeholder: &str| {
        CreateActionRow::SelectMenu(
            CreateSelectMenu::new(cid("voice-select", &[act, channel_id]), CreateSelectMenuKind::User { default_users: None })
                .placeholder(placeholder)
                .min_values(1)
                .max_values(1),
        )
    };
    match action {
        "kick" => ("Choose a member to disconnect.".to_string(), vec![select("kick", "Member to disconnect")]),
        "ban" => {
            let mut content = "Choose a member to ban, or one to unban.".to_string();
            if !banned_user_ids.is_empty() {
                let mentions: Vec<String> = banned_user_ids.iter().map(|u| format!("<@{u}>")).collect();
                content.push_str(&format!("\nBanned: {}", join_with_limit(&mentions, ", ", 1500, bot_util::text::default_more)));
            }
            (content, vec![select("ban", "Member to ban"), select("unban", "Member to unban")])
        }
        _ => ("Choose the new owner (they must be in the channel).".to_string(), vec![select("transfer", "New owner")]),
    }
}

/// `buildMemberPicker(action, channelId, bannedUserIds)`: the ephemeral user-select message behind the kick / ban /
/// transfer buttons.
pub fn build_member_picker(action: &str, channel_id: &str, banned_user_ids: &[String]) -> Payload {
    let (content, rows) = member_picker_parts(action, channel_id, banned_user_ids);
    Payload::text(content).components(rows).ephemeral(true).no_mentions()
}

/// `buildValueModal(action, channelId, current)`: the limit / rename modal.
pub fn build_value_modal(action: &str, channel_id: &str, current: &str) -> CreateModal {
    let field = cid("voice-value", &[]);
    let (title, input) = if action == "limit" {
        let value = if current.is_empty() { "0".to_string() } else { current.to_string() };
        (
            "Member limit",
            CreateInputText::new(InputTextStyle::Short, "Maximum members (0 = no limit)", field)
                .required(true)
                .min_length(1)
                .max_length(2)
                .value(value),
        )
    } else {
        let mut input = CreateInputText::new(InputTextStyle::Short, "New name", field).required(true).min_length(1).max_length(100);
        let value = clamp_text(current, 100);
        if !value.is_empty() {
            input = input.value(value);
        }
        ("Rename channel", input)
    };
    CreateModal::new(cid("voice-modal", &[action, channel_id]), title).components(vec![CreateActionRow::InputText(input)])
}

// ── In-process state (spec §3.4; every table starts empty after a restart) ────

/// The routing index of one guild.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuildIndex {
    pub at: i64,
    /// hub channel id -> hub id
    pub hubs: HashMap<String, String>,
    /// temporary voice channel ids
    pub temps: HashSet<String>,
    /// companion text channel id -> voice channel id
    pub texts: HashMap<String, String>,
    /// hub category ids
    pub categories: HashSet<String>,
}

type IndexLoad = Shared<BoxFuture<'static, Option<(Arc<GuildIndex>, bool)>>>;

/// The module-level state of the JS service, per app.
pub struct TempVoiceState {
    memo: Mutex<IndexMap<String, Arc<GuildIndex>>>,
    loads: Mutex<HashMap<String, (u64, IndexLoad)>>,
    gens: Mutex<HashMap<String, u64>>,
    next_load: std::sync::atomic::AtomicU64,
    /// `hub:{hubId}` and `ch:{voiceChannelId}`; lock order: hub -> ch, never the reverse.
    pub locks: KeyedMutex,
    rename_stamps: Mutex<IndexMap<String, Vec<f64>>>,
    last_create_at: Mutex<HashMap<String, i64>>,
    pending_retries: Mutex<HashSet<String>>,
    text_sync_timers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    warn_throttle: bot_util::async_util::Throttle,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for TempVoiceState {
    fn default() -> Self {
        Self {
            memo: Mutex::default(),
            loads: Mutex::default(),
            gens: Mutex::default(),
            next_load: std::sync::atomic::AtomicU64::new(1),
            locks: KeyedMutex::new(),
            rename_stamps: Mutex::default(),
            last_create_at: Mutex::default(),
            pending_retries: Mutex::default(),
            text_sync_timers: Mutex::default(),
            // At most one logger.warn per key per 10 minutes (missing permissions, full categories, ...).
            warn_throttle: bot_util::async_util::create_throttle(600_000),
            tasks: Mutex::default(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn state(app: &App) -> Arc<TempVoiceState> {
    app.ext::<TempVoiceState>()
}

fn warn_allowed(app: &App, key: &str) -> bool {
    state(app).warn_throttle.allow(key)
}

/// `runTask(label, fn)`: runs a hook body off the dispatcher so the other hooks are never delayed. Tracked so tests
/// can wait for it ([`idle`]).
pub fn run_task<F>(app: &App, label: &'static str, fut: F)
where
    F: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let handle = tokio::spawn(async move {
        if let Err(e) = fut.await {
            log_error!(format!("tempvoice {label} failed"), bot_core::error::log_arg(&e));
        }
    });
    let st = state(app);
    let mut tasks = lock(&st.tasks);
    tasks.retain(|h| !h.is_finished());
    tasks.push(handle);
}

/// Resolves once every task queued with [`run_task`] (and every task they queued) has finished. Debounce and retry
/// timers that have not fired yet are not waited for.
pub async fn idle(app: &App) {
    loop {
        let pending: Vec<_> = std::mem::take(&mut *lock(&state(app).tasks));
        if pending.is_empty() {
            return;
        }
        for h in pending {
            let _ = h.await;
        }
    }
}

/// Runs `f` under the per-key lock (`hub:{id}` / `ch:{id}`); the place in the queue is taken now.
pub fn locked<T, F, Fut>(app: &App, key: &str, f: F) -> impl Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    state(app).locks.run(key, f)
}

/// Runs `f` with the cached guild (`None` when there is no Discord connection or the guild is not cached).
pub fn with_guild<R>(app: &App, guild_id: GuildId, f: impl FnOnce(&Guild) -> R) -> Option<R> {
    let d = app.discord()?;
    let guild = d.cache.guild(guild_id)?;
    Some(f(&guild))
}

fn bot_id(app: &App) -> String {
    app.discord().map(|d| d.cache.current_user().id.to_string()).unwrap_or_default()
}

fn guild_available(app: &App, guild_id: GuildId) -> bool {
    app.guild_directory().guild_state(&guild_id.to_string()) == GuildState::Available
}

fn parse_channel(id: &str) -> Option<ChannelId> {
    id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
}

fn parse_user(id: &str) -> Option<UserId> {
    id.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new)
}

fn cached_channel(app: &App, guild_id: GuildId, channel_id: &str) -> Option<GuildChannel> {
    let c = parse_channel(channel_id)?;
    bot_core::interaction_helpers::cached_channel(app, guild_id, c)
}

fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

fn ms_to_date(ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

fn date_json(ms: i64) -> Value {
    json!(format_js_date(&ms_to_date(ms)))
}

fn date_ms(d: &Option<DateTime<Utc>>) -> Option<f64> {
    d.map(|d| d.timestamp_millis() as f64)
}

const QUIET_CODES: [u64; 5] = [10003, 10007, 10008, 10013, 40032];

/// `discordCall(label, fn, guildId)`: runs one Discord REST call; never panics. Logs as the JS does: quiet codes at
/// debug level, missing access or permissions once per 10 minutes, anything else as a warning.
pub async fn discord_call<T, F>(app: &App, label: &str, guild_id: Option<&str>, fut: F) -> Result<T, RestError>
where
    F: Future<Output = Result<T, RestError>>,
{
    match fut.await {
        Ok(v) => Ok(v),
        Err(err) => {
            match err.code {
                Some(c) if QUIET_CODES.contains(&c) => log_debug!(format!("tempvoice {label}: Discord error {c}")),
                Some(c) if c == 50001 || c == 50013 => {
                    if warn_allowed(app, &format!("{label}:{}", guild_id.unwrap_or("-"))) {
                        log_warn!(format!("tempvoice {label}: {} (guild {})", describe_discord_error(Some(c)), guild_id.unwrap_or("?")));
                    }
                }
                _ => log_warn!(format!("tempvoice {label} failed: {}", err.message)),
            }
            Err(err)
        }
    }
}

/// `deleteDiscordChannel(guild, channelId, reason, label)`: deletes a channel, marking it as a bot deletion first.
pub async fn delete_discord_channel(app: &App, guild_id: GuildId, channel_id: &str, reason: &str, label: &str) -> Result<(), RestError> {
    mark_bot_deletion(app, &[channel_id], Some("tempvoice"), None);
    let Some(c) = parse_channel(channel_id) else { return Err(RestError::new(Some(10003), "Unknown Channel")) };
    let g = guild_id.to_string();
    discord_call(app, label, Some(&g), rest(app).delete_channel(guild_id, c, reason)).await
}

/// What [`confirm_channel`] found.
#[derive(Debug, Clone)]
pub enum ChannelCheck {
    Present(Box<GuildChannel>),
    /// A REST 10003 on an available guild.
    Gone,
    /// Anything else: keep every row.
    Unknown(Option<u64>),
}

impl ChannelCheck {
    pub fn state(&self) -> &'static str {
        match self {
            ChannelCheck::Present(_) => "present",
            ChannelCheck::Gone => "gone",
            ChannelCheck::Unknown(_) => "unknown",
        }
    }
}

/// Puts a channel the bot just created into the serenity cache, as discord.js adds a REST result to
/// `guild.channels.cache` at once (serenity waits for the gateway's CHANNEL_CREATE echo, which then replaces it).
pub fn cache_created_channel(app: &App, channel: &GuildChannel) {
    bot_core::discord::cache_created_channel(app, channel);
}

/// `confirmChannel(guild, channelId)`: rule 20, decided by the cache or a REST fetch, never by a cache miss alone.
pub async fn confirm_channel(app: &App, guild_id: GuildId, channel_id: &str) -> ChannelCheck {
    if let Some(ch) = cached_channel(app, guild_id, channel_id) {
        return ChannelCheck::Present(Box::new(ch));
    }
    if !guild_available(app, guild_id) {
        return ChannelCheck::Unknown(None);
    }
    let Some(c) = parse_channel(channel_id) else { return ChannelCheck::Gone };
    let g = guild_id.to_string();
    match discord_call(app, "fetch channel", Some(&g), rest(app).fetch_channel(guild_id, c)).await {
        Ok(ch) => ChannelCheck::Present(Box::new(ch)),
        Err(e) if e.code == Some(10003) => ChannelCheck::Gone,
        Err(e) => ChannelCheck::Unknown(e.code),
    }
}

// ── The guild index (spec §6.2) ───────────────────────────────────────────────

/// `peekIndex(guildId)`: the fresh memo entry of a guild (sync, no I/O).
pub fn peek_index(app: &App, guild_id: &str) -> Option<Arc<GuildIndex>> {
    let st = state(app);
    let memo = lock(&st.memo);
    let entry = memo.get(guild_id)?;
    (now_ms(app) - entry.at < INDEX_TTL_MS).then(|| entry.clone())
}

/// `invalidateIndex(guildId)`: called after EVERY create/update/delete of a TempVoiceHub or TempVoiceChannel row.
pub fn invalidate_index(app: &App, guild_id: &str) {
    let st = state(app);
    lock(&st.memo).shift_remove(guild_id);
    lock(&st.loads).remove(guild_id);
    *lock(&st.gens).entry(guild_id.to_string()).or_insert(0) += 1;
}

async fn read_index(app: &App, guild_id: &str) -> Result<GuildIndex, DbError> {
    let hubs: Vec<TempVoiceHub> = SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_all(app.db()).await?;
    let temps: Vec<TempVoiceChannel> =
        SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_all(app.db()).await?;
    Ok(GuildIndex {
        at: now_ms(app),
        hubs: hubs.iter().map(|h| (h.channel_id.clone(), h.id.clone())).collect(),
        temps: temps.iter().map(|t| t.channel_id.clone()).collect(),
        texts: temps.iter().filter_map(|t| t.text_channel_id.clone().filter(|x| !x.is_empty()).map(|x| (x, t.channel_id.clone()))).collect(),
        categories: hubs.iter().filter_map(|h| h.category_id.clone().filter(|c| !c.is_empty())).collect(),
    })
}

async fn do_load_index(app: App, guild_id: String) -> Option<(Arc<GuildIndex>, bool)> {
    let st = state(&app);
    let gen_of = |st: &TempVoiceState| lock(&st.gens).get(&guild_id).copied().unwrap_or(0);
    let generation = gen_of(&st);
    match read_index(&app, &guild_id).await {
        Ok(idx) => {
            let idx = Arc::new(idx);
            let stale = gen_of(&st) != generation;
            if !stale {
                let mut memo = lock(&st.memo);
                memo.shift_remove(&guild_id);
                memo.insert(guild_id.clone(), idx.clone());
                while memo.len() > INDEX_MEMO_MAX {
                    memo.shift_remove_index(0);
                }
            }
            Some((idx, stale))
        }
        Err(e) => {
            // Fail closed: without the database no temp-voice action is taken (callers skip the event).
            if warn_allowed(&app, &format!("index:{guild_id}")) {
                log_warn!(format!("tempvoice: could not load the channel index of guild {guild_id}: {e}"));
            }
            None
        }
    }
}

/// `loadIndex(guildId)`: the routing index, or `None` on a database error. Concurrent loads of a guild share one
/// query; a load that raced a write is not memoised and is repeated (at most 3 times).
pub async fn load_index(app: &App, guild_id: &str) -> Option<Arc<GuildIndex>> {
    let mut last = None;
    for _ in 0..3 {
        if let Some(fresh) = peek_index(app, guild_id) {
            return Some(fresh);
        }
        let st = state(app);
        let (id, load) = {
            let mut loads = lock(&st.loads);
            match loads.get(guild_id) {
                Some((id, l)) => (*id, l.clone()),
                None => {
                    let id = st.next_load.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let l: IndexLoad = do_load_index(app.clone(), guild_id.to_string()).boxed().shared();
                    loads.insert(guild_id.to_string(), (id, l.clone()));
                    (id, l)
                }
            }
        };
        let res = load.await;
        {
            let mut loads = lock(&st.loads);
            if loads.get(guild_id).is_some_and(|(i, _)| *i == id) {
                loads.remove(guild_id);
            }
        }
        let (idx, stale) = res?;
        if !stale {
            return Some(idx);
        }
        last = Some(idx); // a write raced this load: load again
    }
    last
}

// ── Rows ──────────────────────────────────────────────────────────────────────

/// A temporary channel row with its hub (`include: { hub: true }`).
#[derive(Debug, Clone, PartialEq)]
pub struct RowWithHub {
    pub row: TempVoiceChannel,
    pub hub: TempVoiceHub,
}

/// `findUnique({ where: { channelId }, include: { hub: true } })`.
pub async fn load_row(app: &App, channel_id: &str) -> anyhow::Result<Option<RowWithHub>> {
    let row: Option<TempVoiceChannel> =
        SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO).filter("channelId", json!(channel_id))?.build()?.fetch_optional(app.db()).await?;
    let Some(row) = row else { return Ok(None) };
    let hub: Option<TempVoiceHub> = SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("id", json!(row.hub_id))?.build()?.fetch_optional(app.db()).await?;
    Ok(hub.map(|hub| RowWithHub { row, hub }))
}

async fn load_plain_row(app: &App, channel_id: &str) -> anyhow::Result<Option<TempVoiceChannel>> {
    Ok(SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO).filter("channelId", json!(channel_id))?.build()?.fetch_optional(app.db()).await?)
}

/// `updateRow(channelId, data)`: an update that ignores a row deleted meanwhile (P2025: `None`).
pub async fn update_row(app: &App, channel_id: &str, data: Vec<(&str, Value)>) -> anyhow::Result<Option<TempVoiceChannel>> {
    let q = UpdateBuilder::new(&TEMP_VOICE_CHANNEL_INFO).set_all(data)?.filter("channelId", json!(channel_id))?.build()?;
    Ok(q.fetch_optional(app.db()).await?)
}

/// `prisma.tempVoiceChannel.update(...)` (a missing row is an error, as P2025 is in the JS).
async fn update_row_strict(app: &App, channel_id: &str, data: Vec<(&str, Value)>) -> anyhow::Result<TempVoiceChannel> {
    update_row(app, channel_id, data).await?.ok_or_else(|| anyhow::anyhow!("No record was found for an update. (P2025)"))
}

async fn count_rows(app: &App, sql: &str, bind: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql.to_string())).bind(bind).fetch_one(app.db()).await.map_err(DbError::from)?;
    Ok(n)
}

async fn hub_channel_count(app: &App, hub_id: &str) -> anyhow::Result<i64> {
    count_rows(app, r#"SELECT COUNT(*)::bigint FROM "TempVoiceChannel" WHERE "hubId" = $1"#, hub_id).await
}

async fn find_hub_by_id(app: &App, hub_id: &str) -> anyhow::Result<Option<TempVoiceHub>> {
    Ok(SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("id", json!(hub_id))?.build()?.fetch_optional(app.db()).await?)
}

/// `retireHubIfDone(guild, hubId)`: deletes a hub whose channel is gone (confirmed) once its last temporary channel
/// closed.
async fn retire_hub_if_done(app: &App, guild_id: GuildId, hub_id: &str, known: Option<(String, String)>) -> anyhow::Result<bool> {
    let hub = match known {
        Some(k) => Some(k),
        None => find_hub_by_id(app, hub_id).await?.map(|h| (h.guild_id, h.channel_id)),
    };
    let Some((hub_guild, hub_channel)) = hub else { return Ok(false) };
    if hub_guild != guild_id.to_string() {
        return Ok(false);
    }
    if !matches!(confirm_channel(app, guild_id, &hub_channel).await, ChannelCheck::Gone) {
        return Ok(false);
    }
    if hub_channel_count(app, hub_id).await? > 0 {
        return Ok(false);
    }
    delete_where(&TEMP_VOICE_HUB_INFO, [("id", json!(hub_id))])?.execute(app.db()).await?;
    invalidate_index(app, &guild_id.to_string());
    log_info!(format!("tempvoice: retired hub {hub_id} deleted (its channels are closed)"));
    Ok(true)
}

fn forget_channel_state(app: &App, channel_id: &str) {
    let st = state(app);
    lock(&st.rename_stamps).shift_remove(channel_id);
    if let Some(timer) = lock(&st.text_sync_timers).remove(channel_id) {
        timer.abort();
    }
}

/// `purgeRow(guild, row)`: forgets a temp channel's row, jobs, limiter state and timers (the caller holds `ch:` when
/// it can). The companion text channel goes too.
pub async fn purge_row(app: &App, guild_id: GuildId, row: &TempVoiceChannel, hub: Option<&TempVoiceHub>) -> anyhow::Result<()> {
    if let Some(text) = row.text_channel_id.as_deref().filter(|t| !t.is_empty())
        && cached_channel(app, guild_id, text).is_some()
    {
        let _ = delete_discord_channel(app, guild_id, text, "Temp voice: channel closed", "delete text").await;
    }
    delete_where(&TEMP_VOICE_CHANNEL_INFO, [("channelId", json!(row.channel_id))])?.execute(app.db()).await?;
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(cleanup_key(&row.channel_id)), ..Default::default() }).await;
    forget_channel_state(app, &row.channel_id);
    app.cache().del_one(&rename_key(&row.channel_id)).await;
    invalidate_index(app, &guild_id.to_string());
    let known = hub.map(|h| (h.guild_id.clone(), h.channel_id.clone()));
    if let Err(e) = retire_hub_if_done(app, guild_id, &row.hub_id, known).await {
        log_warn!(format!("tempvoice: could not check the hub of channel {}: {e}", row.channel_id));
    }
    Ok(())
}

async fn schedule_delete_retry(app: &App, guild_id: GuildId, channel_id: &str) {
    let job = ScheduleJob::new(guild_id.to_string(), JobType::TempvoiceCleanup, ms_to_date(now_ms(app) + DELETE_RETRY_MS))
        .payload(json!({ "channelId": channel_id }))
        .dedupe_key(cleanup_key(channel_id))
        .max_attempts(5)
        .on_conflict(OnConflict::Replace);
    if let Err(e) = app.scheduler().schedule_job(job).await {
        log_error!(format!("tempvoice: could not schedule the cleanup of {channel_id}"), bot_core::error::log_arg(&e));
    }
}

/// `deleteTempChannel(guild, row, reason)`: deletes a temporary channel (and its text channel) and forgets the row.
/// The caller holds `ch:`. The row is kept when Discord refuses (anything but 10003); `Err(code)` then.
pub async fn delete_temp_channel(app: &App, guild_id: GuildId, row: &TempVoiceChannel, hub: Option<&TempVoiceHub>, reason: &str) -> anyhow::Result<Result<(), Option<u64>>> {
    if !guild_available(app, guild_id) {
        return Ok(Err(None));
    }
    let r = delete_discord_channel(app, guild_id, &row.channel_id, &format!("Temp voice: {reason}"), "delete voice").await;
    if let Err(e) = &r
        && e.code != Some(10003)
    {
        return Ok(Err(e.code));
    }
    if let Some(text) = row.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
        let _ = delete_discord_channel(app, guild_id, text, "Temp voice: channel closed", "delete text").await;
    }
    purge_row(app, guild_id, row, hub).await?;
    Ok(Ok(()))
}

// ── Companion text channel (spec §6.11) ───────────────────────────────────────

/// `requestTextSync(guild, channelId)`: debounced (1.5 s), so bursts of joins and leaves coalesce into one PATCH.
pub fn request_text_sync(app: &App, guild_id: GuildId, channel_id: &str) {
    let st = state(app);
    let mut timers = lock(&st.text_sync_timers);
    if timers.get(channel_id).is_some_and(|h| !h.is_finished()) {
        return;
    }
    let (a, ch) = (app.clone(), channel_id.to_string());
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(TEXT_SYNC_DELAY_MS)).await;
        lock(&state(&a).text_sync_timers).remove(&ch);
        let (a2, ch2) = (a.clone(), ch.clone());
        run_task(&a, "text-sync", locked(&a, &format!("ch:{ch}"), move || async move { sync_text_overwrites(&a2, guild_id, &ch2).await }));
    });
    timers.insert(channel_id.to_string(), handle);
}

/// Whether a text sync is waiting for its debounce (tests).
pub fn text_sync_pending(app: &App, channel_id: &str) -> bool {
    lock(&state(app).text_sync_timers).get(channel_id).is_some_and(|h| !h.is_finished())
}

/// `syncTextOverwrites(guild, channelId)`: rewrites the companion text channel's overwrites from the voice channel's
/// current members. The caller holds `ch:`.
pub async fn sync_text_overwrites(app: &App, guild_id: GuildId, channel_id: &str) -> anyhow::Result<()> {
    let Some(RowWithHub { row, hub }) = load_row(app, channel_id).await? else { return Ok(()) };
    let Some(text_id) = row.text_channel_id.clone().filter(|t| !t.is_empty()) else { return Ok(()) };
    if row.guild_id != guild_id.to_string() {
        return Ok(());
    }
    let Some(vc) = cached_channel(app, guild_id, channel_id) else { return Ok(()) }; // a cache miss is not a deletion
    let text = match cached_channel(app, guild_id, &text_id) {
        Some(t) => t,
        None => {
            let chk = confirm_channel(app, guild_id, &text_id).await;
            if matches!(chk, ChannelCheck::Gone) {
                update_row(app, channel_id, vec![("textChannelId", Value::Null), ("controlMessageId", Value::Null)]).await?;
                invalidate_index(app, &guild_id.to_string());
            }
            match chk {
                ChannelCheck::Present(ch) => *ch,
                _ => return Ok(()),
            }
        }
    };
    let g = guild_id.to_string();
    let settings = match app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(&g).await,
    };
    let hub_channel = cached_channel(app, guild_id, &hub.channel_id);
    let parent = text.parent_id.and_then(|p| cached_channel(app, guild_id, &p.to_string()));
    let template = snapshot_overwrites(if hub.sync_permissions { parent.as_ref() } else { hub_channel.as_ref() });
    let (member_ids, exempt) = with_guild(app, guild_id, |gd| {
        (human_ids_in(gd, vc.id).iter().map(|u| u.to_string()).collect::<Vec<_>>(), exempt_role_ids(Some(&hub), settings.as_deref(), Some(gd)))
    })
    .unwrap_or_default();
    let list = compute_text_overwrites(&TextOverwriteInput {
        everyone_id: g.clone(),
        bot_id: bot_id(app),
        owner_id: row.owner_id.clone(),
        template,
        members_only: hub.text_channel_members_only,
        member_ids,
        banned_user_ids: row.banned_user_ids.clone(),
        exempt_role_ids: exempt,
        grantable: grantable_bits(app, guild_id, text.parent_id),
    });
    let _ = discord_call(app, "text overwrites", Some(&g), rest(app).set_overwrites(guild_id, text.id, list, None)).await;
    Ok(())
}

// ── Voice overwrites (spec §6.10) ─────────────────────────────────────────────

/// The state [`apply_voice_overwrites`] computes the list from.
#[derive(Debug, Clone)]
pub struct VoiceState2 {
    pub hub: TempVoiceHub,
    pub owner_id: String,
    pub locked: bool,
    pub hidden: bool,
    pub banned_user_ids: Vec<String>,
}

/// Why [`apply_voice_overwrites`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyFailed {
    pub code: Option<u64>,
    /// The hub channel was deleted: there is no template left to rebuild the overwrites from.
    pub hub_gone: bool,
}

/// `applyVoiceOverwrites(guild, vc, state, settings, reason)`: rewrites the whole overwrite list of a temp channel
/// from (template + state).
pub async fn apply_voice_overwrites(
    app: &App,
    guild_id: GuildId,
    vc: &GuildChannel,
    st: &VoiceState2,
    settings: Option<&bot_db::GuildSettings>,
    reason: &str,
) -> Result<(), ApplyFailed> {
    let hub_channel = cached_channel(app, guild_id, &st.hub.channel_id);
    // A retired hub has no template left to copy: rewriting from an empty one would drop what the hub restricted.
    if !st.hub.sync_permissions && hub_channel.is_none() {
        return Err(ApplyFailed { code: None, hub_gone: true });
    }
    let parent = vc.parent_id.and_then(|p| cached_channel(app, guild_id, &p.to_string()));
    let template = snapshot_overwrites(if st.hub.sync_permissions { parent.as_ref() } else { hub_channel.as_ref() });
    let exempt = with_guild(app, guild_id, |g| exempt_role_ids(Some(&st.hub), settings, Some(g))).unwrap_or_default();
    let list = compute_voice_overwrites(&VoiceOverwriteInput {
        everyone_id: guild_id.to_string(),
        bot_id: bot_id(app),
        owner_id: st.owner_id.clone(),
        template,
        locked: st.locked,
        hidden: st.hidden,
        banned_user_ids: st.banned_user_ids.clone(),
        exempt_role_ids: exempt,
        grantable: grantable_bits(app, guild_id, vc.parent_id),
    });
    let g = guild_id.to_string();
    discord_call(app, "overwrites", Some(&g), rest(app).set_overwrites(guild_id, vc.id, list, Some(reason)))
        .await
        .map_err(|e| ApplyFailed { code: e.code, hub_gone: false })
}

// ── Control panel (spec §6.12) ────────────────────────────────────────────────

/// `postControlPanel(guild, row, voice, text)`: posts the panel (pinned in the companion text channel). Best effort;
/// stores `controlMessageId`. The caller holds `ch:`.
pub async fn post_control_panel(app: &App, guild_id: GuildId, row: &TempVoiceChannel, voice_id: ChannelId, text_id: Option<ChannelId>) {
    let panel_channel = text_id.unwrap_or(voice_id);
    if !check_channel_usable(app, guild_id, Some(panel_channel), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS).ok {
        return;
    }
    let g = guild_id.to_string();
    let settings = match app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(&g).await,
    };
    let panel = build_control_panel(&voice_id.to_string(), &row.owner_id, settings.as_deref());
    let Ok(message_id) = discord_call(app, "post panel", Some(&g), rest(app).send_message(guild_id, panel_channel, panel.to_message())).await else {
        return;
    };
    if let Err(e) = update_row(app, &voice_id.to_string(), vec![("controlMessageId", json!(message_id.to_string()))]).await {
        log_warn!(format!("tempvoice: could not store the panel message: {e}"));
    }
    if text_id.is_some() {
        let target = PermTarget::Channel(guild_id, panel_channel);
        let can_pin = missing_bot_permissions(app, target, &["PinMessages"]).is_empty() || missing_bot_permissions(app, target, &["ManageMessages"]).is_empty();
        if can_pin {
            let _ = discord_call(app, "pin panel", Some(&g), rest(app).pin_message(guild_id, panel_channel, message_id, "Temp voice controls")).await;
        }
    }
}

/// `refreshControlPanel(...)`: refreshes the pinned panel after an ownership change. Best effort.
async fn refresh_control_panel(app: &App, guild_id: GuildId, row: &TempVoiceChannel, vc: ChannelId, new_owner_id: &str, settings: Option<&bot_db::GuildSettings>) {
    let Some(message) = row.control_message_id.as_deref().and_then(|m| m.parse::<u64>().ok()).filter(|v| *v != 0).map(MessageId::new) else {
        return;
    };
    let panel_channel = row
        .text_channel_id
        .as_deref()
        .and_then(|t| cached_channel(app, guild_id, t))
        .map(|c| c.id)
        .unwrap_or(vc);
    let g = guild_id.to_string();
    if let Err(e) = discord_call(app, "fetch panel", Some(&g), rest(app).fetch_message(guild_id, panel_channel, message)).await {
        if e.code == Some(10008) {
            let _ = update_row(app, &row.channel_id, vec![("controlMessageId", Value::Null)]).await;
        }
        return;
    }
    let panel = build_control_panel(&vc.to_string(), new_owner_id, settings);
    if let Err(e) = discord_call(app, "edit panel", Some(&g), rest(app).edit_message(guild_id, panel_channel, message, panel.to_message_edit())).await
        && e.code == Some(10008)
    {
        let _ = update_row(app, &row.channel_id, vec![("controlMessageId", Value::Null)]).await;
    }
}

// ── Hub join (spec §6.4) ──────────────────────────────────────────────────────

fn voice_channel_of(app: &App, guild_id: GuildId, user: UserId) -> Option<ChannelId> {
    with_guild(app, guild_id, |g| g.voice_states.get(&user).and_then(|v| v.channel_id)).flatten()
}

fn still_in_hub(app: &App, guild_id: GuildId, user: UserId, hub: &TempVoiceHub) -> bool {
    voice_channel_of(app, guild_id, user).is_some_and(|c| c.to_string() == hub.channel_id)
}

/// What a successful creation hands to the panel step.
struct Created {
    row: TempVoiceChannel,
    voice: ChannelId,
    text: Option<ChannelId>,
}

/// `createForMember(guild, hubId, member, settings)`: creates the channel for one member. The caller holds
/// `hub:{hubId}`.
async fn create_for_member(app: &App, guild_id: GuildId, hub_id: &str, member: &Member, settings: Option<Settings>) -> anyhow::Result<Option<Created>> {
    let g = guild_id.to_string();
    let Some(hub) = find_hub_by_id(app, hub_id).await? else { return Ok(None) };
    if hub.guild_id != g {
        return Ok(None);
    }
    let Some(hub_channel) = cached_channel(app, guild_id, &hub.channel_id) else { return Ok(None) }; // retired hub
    let user = member.user.id;
    if !still_in_hub(app, guild_id, user, &hub) {
        return Ok(None);
    }

    // One channel per owner per hub: move the member back into it (checked under the channel lock).
    let existing: Option<TempVoiceChannel> = SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO)
        .filter("guildId", json!(g))?
        .filter("hubId", json!(hub_id))?
        .filter("ownerId", json!(user.to_string()))?
        .limit(1)
        .build()?
        .fetch_optional(app.db())
        .await?;
    if let Some(existing) = existing {
        let (a, hub2, ch) = (app.clone(), hub.clone(), existing.channel_id.clone());
        let outcome: anyhow::Result<&'static str> = locked(app, &format!("ch:{ch}"), move || async move {
            let Some(row) = load_plain_row(&a, &ch).await? else { return Ok("gone") };
            match confirm_channel(&a, guild_id, &row.channel_id).await {
                ChannelCheck::Gone => {
                    purge_row(&a, guild_id, &row, None).await?;
                    Ok("gone")
                }
                ChannelCheck::Unknown(_) => Ok("unknown"),
                ChannelCheck::Present(channel) => {
                    if still_in_hub(&a, guild_id, user, &hub2) {
                        let gs = guild_id.to_string();
                        let _ = discord_call(&a, "move back", Some(&gs), rest(&a).move_member(guild_id, user, Some(channel.id), "Temp voice: back to own channel"))
                            .await;
                    }
                    Ok("present")
                }
            }
        })
        .await;
        if outcome? != "gone" {
            return Ok(None);
        }
        // The row or its channel is gone: continue and create a new one.
    }

    // Creation cooldown (one retry when it ends).
    let key = format!("{g}:{user}");
    let now = now_ms(app);
    let last = lock(&state(app).last_create_at).get(&key).copied().unwrap_or(0);
    let wait = CREATE_COOLDOWN_MS - (now - last);
    if wait > 0 {
        let st = state(app);
        let first = lock(&st.pending_retries).insert(key.clone());
        if first {
            let (a, hid, m, s) = (app.clone(), hub_id.to_string(), member.clone(), settings.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
                lock(&state(&a).pending_retries).remove(&key);
                let a2 = a.clone();
                run_task(&a, "hub-retry", async move {
                    queue_hub_join(a2, guild_id, hid, m, s).await;
                    Ok(())
                });
            });
        }
        return Ok(None);
    }

    let target_parent: Option<GuildChannel> =
        hub.category_id.as_deref().and_then(|c| cached_channel(app, guild_id, c)).or_else(|| hub_channel.parent_id.and_then(|p| cached_channel(app, guild_id, &p.to_string())));
    let parent_target = match &target_parent {
        Some(p) => PermTarget::Channel(guild_id, p.id),
        None => PermTarget::Guild(guild_id),
    };
    let mut missing = missing_bot_permissions(app, parent_target, &REQUIRED_BOT_PERMS);
    missing.extend(missing_bot_permissions(app, PermTarget::Channel(guild_id, hub_channel.id), &HUB_CHANNEL_PERMS));
    if !missing.is_empty() {
        if warn_allowed(app, &format!("perms:{g}")) {
            let mut uniq: Vec<String> = Vec::new();
            for m in missing {
                if !uniq.contains(&m) {
                    uniq.push(m);
                }
            }
            log_warn!(format!("tempvoice: missing {} in guild {g}; cannot create channels", uniq.join(", ")));
        }
        return Ok(None);
    }

    let rows = list_hub_channels(app.db(), hub_id).await?;
    let index = lowest_free_index(&rows.iter().map(|r| i64::from(r.index)).collect::<Vec<_>>());
    let guild_snapshot = with_guild(app, guild_id, |gd| (NameContext::of(member, Some(gd)), maximum_bitrate(gd), exempt_role_ids(Some(&hub), settings.as_deref(), Some(gd))));
    let (name_ctx, max_bitrate, exempt) = guild_snapshot.unwrap_or_else(|| (NameContext::of(member, None), 96_000, Vec::new()));
    let name = render_channel_name(&hub.name_format, &name_ctx, index);
    let parent_id = target_parent.as_ref().map(|p| p.id);
    let grantable = grantable_bits(app, guild_id, parent_id);
    let template = snapshot_overwrites(if hub.sync_permissions { target_parent.as_ref() } else { Some(&hub_channel) });
    let bot = bot_id(app);

    let spec = NewChannel {
        name: name.clone(),
        kind: ChannelType::Voice,
        parent: parent_id,
        user_limit: u32::try_from(hub.user_limit).ok(),
        bitrate: effective_bitrate(hub.bitrate.map(i64::from), Some(max_bitrate)).and_then(|b| u32::try_from(b).ok()),
        topic: None,
        overwrites: compute_voice_overwrites(&VoiceOverwriteInput {
            everyone_id: g.clone(),
            bot_id: bot.clone(),
            owner_id: user.to_string(),
            template: template.clone(),
            locked: false,
            hidden: false,
            banned_user_ids: Vec::new(),
            exempt_role_ids: exempt.clone(),
            grantable,
        }),
        reason: Some(clamp_text(&format!("Temporary channel for {}", bot_core::adapters::user_tag(&member.user)), 512)),
    };
    let voice = match discord_call(app, "create voice", Some(&g), rest(app).create_channel(guild_id, spec)).await {
        Ok(v) => {
            cache_created_channel(app, &v);
            v
        }
        Err(e) => {
            if warn_allowed(app, &format!("create:{g}")) {
                log_warn!(format!("tempvoice: could not create a channel in guild {g} ({})", describe_discord_error(e.code)));
            }
            return Ok(None);
        }
    };
    {
        let st = state(app);
        let mut lc = lock(&st.last_create_at);
        let t = now_ms(app);
        lc.insert(key.clone(), t);
        lc.retain(|_, v| t - *v <= CREATE_PRUNE_MS);
    }

    let mut text: Option<ChannelId> = None;
    if hub.create_text_channel {
        let spec = NewChannel {
            name: text_channel_name(&name, index),
            kind: ChannelType::Text,
            parent: parent_id,
            user_limit: None,
            bitrate: None,
            topic: Some(clamp_text(&format!("Text chat for 🔊 {name}"), 1024)),
            overwrites: compute_text_overwrites(&TextOverwriteInput {
                everyone_id: g.clone(),
                bot_id: bot.clone(),
                owner_id: user.to_string(),
                template,
                members_only: hub.text_channel_members_only,
                member_ids: vec![user.to_string()],
                banned_user_ids: Vec::new(),
                exempt_role_ids: exempt,
                grantable,
            }),
            reason: Some("Temporary text channel".into()),
        };
        if let Ok(t) = discord_call(app, "create text", Some(&g), rest(app).create_channel(guild_id, spec)).await {
            cache_created_channel(app, &t);
            text = Some(t.id);
        }
    }

    let saved: anyhow::Result<TempVoiceChannel> = async {
        app.settings().ensure_guild_settings(&g).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let row: TempVoiceChannel = InsertBuilder::new(&TEMP_VOICE_CHANNEL_INFO)
            .values([
                ("guildId", json!(g)),
                ("hubId", json!(hub_id)),
                ("channelId", json!(voice.id.to_string())),
                ("textChannelId", text.map(|t| json!(t.to_string())).unwrap_or(Value::Null)),
                ("ownerId", json!(user.to_string())),
                ("index", json!(index)),
            ])?
            .build()?
            .fetch_one(app.db())
            .await?;
        Ok(row)
    }
    .await;
    let row = match saved {
        Ok(r) => r,
        Err(e) => {
            log_error!(format!("tempvoice: could not save the channel {}", voice.id), bot_core::error::log_arg(&e));
            let _ = delete_discord_channel(app, guild_id, &voice.id.to_string(), "Temp voice: could not save the channel", "rollback voice").await;
            if let Some(t) = text {
                let _ = delete_discord_channel(app, guild_id, &t.to_string(), "Temp voice: could not save the channel", "rollback text").await;
            }
            return Ok(None);
        }
    };
    invalidate_index(app, &g);

    let moved = if still_in_hub(app, guild_id, user, &hub) {
        discord_call(app, "move", Some(&g), rest(app).move_member(guild_id, user, Some(voice.id), "Temp voice: created")).await.is_ok()
    } else {
        false
    };
    if !moved {
        // The member left meanwhile (or Discord refused the move): the channel would stay empty forever.
        let (a, r, h) = (app.clone(), row.clone(), hub.clone());
        let closed = locked(app, &format!("ch:{}", voice.id), move || async move { delete_temp_channel(&a, guild_id, &r, Some(&h), "creator left").await }).await?;
        if closed.is_err() {
            schedule_delete_retry(app, guild_id, &voice.id.to_string()).await;
        }
        return Ok(None);
    }
    Ok(Some(Created { row, voice: voice.id, text }))
}

/// `queueHubJoin(guild, hubId, member, settings)`: serialises creation per hub; posts the control panel after the
/// hub lock is released.
pub fn queue_hub_join(app: App, guild_id: GuildId, hub_id: String, member: Member, settings: Option<Settings>) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        let (a, hid, m) = (app.clone(), hub_id.clone(), member.clone());
        let made = locked(&app, &format!("hub:{hub_id}"), move || async move { create_for_member(&a, guild_id, &hid, &m, settings).await }).await;
        let made = match made {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(e) => {
                log_error!("tempvoice voice failed", bot_core::error::log_arg(&e));
                return;
            }
        };
        let a = app.clone();
        let voice = made.voice;
        locked(&app, &format!("ch:{voice}"), move || async move {
            // The channel may already be gone (the creator left at once): then there is nothing to post into.
            let still = matches!(load_plain_row(&a, &voice.to_string()).await, Ok(Some(_)));
            if !still {
                return;
            }
            post_control_panel(&a, guild_id, &made.row, made.voice, made.text).await;
        })
        .await;
    })
}

// ── Joining and leaving a temp channel (spec §6.5, §6.6) ──────────────────────

/// `onJoinTemp(guild, channelId, member)`. The caller holds `ch:{channelId}`.
pub async fn on_join_temp(app: &App, guild_id: GuildId, channel_id: &str, member: &Member) -> anyhow::Result<()> {
    let Some(RowWithHub { row, hub }) = load_row(app, channel_id).await? else { return Ok(()) };
    if row.guild_id != guild_id.to_string() {
        return Ok(());
    }
    match confirm_channel(app, guild_id, channel_id).await {
        ChannelCheck::Gone => return purge_row(app, guild_id, &row, Some(&hub)).await,
        ChannelCheck::Unknown(_) => return Ok(()),
        ChannelCheck::Present(_) => {}
    }
    let user = member.user.id.to_string();
    let mut data: Vec<(&str, Value)> = Vec::new();
    if row.empty_since.is_some() {
        data.push(("emptySince", Value::Null));
        app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(cleanup_key(channel_id)), ..Default::default() }).await;
    }
    if user == row.owner_id && row.owner_left_at.is_some() {
        data.push(("ownerLeftAt", Value::Null));
    }
    if row.banned_user_ids.contains(&user) {
        let settings = app.settings().get_settings_safe(&guild_id.to_string()).await;
        if !is_protected_target(app, guild_id, member, Some(&hub), settings.as_deref()) {
            // Discord already denies Connect; this covers members moved in by someone with Move Members.
            let g = guild_id.to_string();
            let _ = discord_call(app, "kick banned", Some(&g), rest(app).move_member(guild_id, member.user.id, None, "Temp voice: banned from this channel")).await;
        }
    }
    if !data.is_empty() {
        update_row(app, channel_id, data).await?;
    }
    if row.text_channel_id.is_some() && hub.text_channel_members_only {
        request_text_sync(app, guild_id, channel_id);
    }
    Ok(())
}

/// `onLeaveTemp(guild, channelId, member)`. The caller holds `ch:{channelId}`.
pub async fn on_leave_temp(app: &App, guild_id: GuildId, channel_id: &str, member: &Member) -> anyhow::Result<()> {
    let Some(RowWithHub { row, hub }) = load_row(app, channel_id).await? else { return Ok(()) };
    if row.guild_id != guild_id.to_string() {
        return Ok(());
    }
    let channel = match confirm_channel(app, guild_id, channel_id).await {
        ChannelCheck::Gone => return purge_row(app, guild_id, &row, Some(&hub)).await,
        ChannelCheck::Unknown(_) => return Ok(()),
        ChannelCheck::Present(c) => c,
    };
    // The cache is updated before the event, so the leaver is not counted.
    let humans = count_humans(app, guild_id, channel.id);
    let is_owner = member.user.id.to_string() == row.owner_id;
    let now = now_ms(app);
    if humans == 0 {
        if hub.keep_alive_minutes == 0 {
            if delete_temp_channel(app, guild_id, &row, Some(&hub), "empty").await?.is_err() {
                schedule_delete_retry(app, guild_id, channel_id).await;
            }
            return Ok(());
        }
        let left = if is_owner { Some(row.owner_left_at.map(|d| d.timestamp_millis()).unwrap_or(now)) } else { row.owner_left_at.map(|d| d.timestamp_millis()) };
        update_row(app, channel_id, vec![("emptySince", date_json(now)), ("ownerLeftAt", left.map(date_json).unwrap_or(Value::Null))]).await?;
        let job = ScheduleJob::new(guild_id.to_string(), JobType::TempvoiceCleanup, ms_to_date(now + i64::from(hub.keep_alive_minutes) * 60_000))
            .payload(json!({ "channelId": channel_id }))
            .dedupe_key(cleanup_key(channel_id))
            .max_attempts(5)
            .on_conflict(OnConflict::Replace);
        if let Err(e) = app.scheduler().schedule_job(job).await {
            log_error!(format!("tempvoice: could not schedule the keep-alive of {channel_id}; reconciliation will repair it"), bot_core::error::log_arg(&e));
        }
        return Ok(());
    }
    if is_owner && row.owner_left_at.is_none() {
        update_row(app, channel_id, vec![("ownerLeftAt", date_json(now))]).await?;
    }
    if row.text_channel_id.is_some() && hub.text_channel_members_only {
        request_text_sync(app, guild_id, channel_id);
    }
    Ok(())
}

// ── Job tempvoice.cleanup (spec §6.8) ─────────────────────────────────────────

/// `handleCleanupJob(job, ctx)`: deletes a temp channel that stayed empty for `keepAliveMinutes`. Idempotent; never
/// checks the module toggle.
pub async fn handle_cleanup_job(job: bot_core::scheduler::Job, ctx: bot_core::scheduler::JobCtx) -> anyhow::Result<bot_core::scheduler::JobOutcome> {
    use bot_core::scheduler::JobOutcome;
    let Some(channel_id) = job.payload.get("channelId").and_then(Value::as_str).filter(|c| is_snowflake(c)).map(str::to_string) else {
        return Ok(JobOutcome::Done);
    };
    let Some(guild_id) = ctx.guild_id.parse::<u64>().ok().filter(|v| *v != 0).map(GuildId::new) else { return Ok(JobOutcome::Done) };
    let app = ctx.app.clone();
    let a = app.clone();
    let job_guild = job.guild_id.clone();
    locked(&app, &format!("ch:{channel_id}"), move || async move {
        let Some(RowWithHub { row, hub }) = load_row(&a, &channel_id).await? else { return Ok(JobOutcome::Done) };
        if row.guild_id != job_guild {
            return Ok(JobOutcome::Done);
        }
        let channel = match confirm_channel(&a, guild_id, &channel_id).await {
            ChannelCheck::Gone => {
                purge_row(&a, guild_id, &row, Some(&hub)).await?;
                return Ok(JobOutcome::Done);
            }
            // transient: look again later
            ChannelCheck::Unknown(_) => return Ok(JobOutcome::Reschedule { next_run_at: ms_to_date(now_ms(&a) + DELETE_RETRY_MS), payload: None }),
            ChannelCheck::Present(c) => c,
        };
        if count_humans(&a, guild_id, channel.id) > 0 {
            update_row(&a, &channel_id, vec![("emptySince", Value::Null)]).await?; // someone is back
            return Ok(JobOutcome::Done);
        }
        if let Err(code) = delete_temp_channel(&a, guild_id, &row, Some(&hub), "keep-alive expired").await? {
            // The scheduler retries with backoff.
            anyhow::bail!("delete failed ({})", code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()));
        }
        Ok(JobOutcome::Done)
    })
    .await
}

// ── Voice actions (spec §6.9) ─────────────────────────────────────────────────

fn action_perms(action: &str) -> &'static [&'static str] {
    match action {
        "kick" => &["MoveMembers"],
        "limit" | "rename" => &["ManageChannels"],
        _ => &["ManageChannels", "ManageRoles"],
    }
}

/// The value of a limit (a number) or rename (a name) action.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ActionValue {
    #[default]
    None,
    Int(i64),
    Text(String),
}

/// Where a voice action came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActionSource {
    #[default]
    Command,
    Button,
    Select,
    Modal,
}

/// One `/voice` action or panel click (`runVoiceAction(req)`).
#[derive(Debug, Clone)]
pub struct VoiceActionReq {
    pub guild_id: GuildId,
    pub actor: Member,
    pub channel_id: String,
    pub action: String,
    pub target_id: Option<String>,
    pub value: ActionValue,
    pub settings: Option<Settings>,
    pub source: ActionSource,
}

/// `{ ok, embed }`.
#[derive(Debug, Clone)]
pub struct ActionResult {
    pub ok: bool,
    pub embed: CreateEmbed,
}

fn fail(m: Msg, p: MsgParams) -> ActionResult {
    ActionResult { ok: false, embed: m.embed(&p) }
}

fn fail0(m: Msg) -> ActionResult {
    fail(m, MsgParams::default())
}

fn ok_result(action: &str, p: OkParams) -> ActionResult {
    ActionResult { ok: true, embed: ok_embed(action, &p) }
}

/// `runVoiceAction(req)`: runs one `/voice` action or panel click, serialised per channel; never fails.
pub async fn run_voice_action(app: &App, req: VoiceActionReq) -> ActionResult {
    let a = app.clone();
    let action = req.action.clone();
    let key = format!("ch:{}", req.channel_id);
    match locked(app, &key, move || async move { run_voice_action_locked(&a, req).await }).await {
        Ok(r) => r,
        Err(e) => {
            log_error!(format!("tempvoice {action} failed"), bot_core::error::log_arg(&e));
            fail0(Msg::Internal)
        }
    }
}

async fn record_rename(app: &App, channel_id: &str, recent: &[f64]) {
    // Strictly increasing: two renames stamped in the same millisecond must not fold into one.
    let last = recent.last().copied().unwrap_or(0.0);
    let mut next = recent.to_vec();
    next.push((now_ms(app) as f64).max(last + 1.0));
    {
        let st = state(app);
        let mut stamps = lock(&st.rename_stamps);
        stamps.shift_remove(channel_id);
        stamps.insert(channel_id.to_string(), next.clone());
        while stamps.len() > RENAME_STAMPS_MAX {
            stamps.shift_remove_index(0);
        }
    }
    // Fail-open: Redis down keeps only the in-memory stamps.
    app.cache().set(&rename_key(channel_id), &next, 600).await;
}

/// The rename stamps of a channel: the in-memory ones and Redis's, merged (`new Set([...])`).
pub async fn rename_stamps(app: &App, channel_id: &str) -> Vec<f64> {
    let mut stamps: Vec<f64> = lock(&state(app).rename_stamps).get(channel_id).cloned().unwrap_or_default();
    if let Some(Value::Array(from_redis)) = app.cache().get(&rename_key(channel_id)).await {
        for v in from_redis {
            if let Some(n) = v.as_f64()
                && !stamps.contains(&n)
            {
                stamps.push(n);
            }
        }
    }
    stamps
}

async fn rename_channel(app: &App, guild_id: GuildId, vc: &GuildChannel, channel_id: &str, value: &ActionValue, reason: &str) -> ActionResult {
    let raw = match value {
        ActionValue::Text(s) => Some(s.clone()),
        ActionValue::Int(n) => Some(n.to_string()),
        ActionValue::None => None,
    };
    let Some(name) = normalize_channel_name(raw.as_deref()) else { return fail0(Msg::RenameInvalid) };
    if name == vc.name {
        return fail0(Msg::NoChangeName);
    }
    let stamps = rename_stamps(app, channel_id).await;
    let w = check_rename_window(&stamps, now_ms(app) as f64);
    if !w.ok {
        return fail(Msg::RenameRate, MsgParams { retry_at: w.retry_at, ..Default::default() });
    }
    let (r, n, rs, ch) = (rest(app), name.clone(), reason.to_string(), vc.id);
    let call = async move { r.set_name(guild_id, ch, &n, &rs).await };
    match bot_util::async_util::with_timeout(call, RENAME_TIMEOUT_MS, "tempvoice rename").await {
        Ok(()) => {}
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            // The REST client is waiting out Discord's own rate limit; the call keeps running and applies later.
            record_rename(app, channel_id, &w.recent).await;
            return fail0(Msg::RenameQueued);
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => {
            log_warn!(format!("tempvoice rename failed in guild {guild_id}: {}", e.message));
            return fail(Msg::DiscordError, MsgParams { code: e.code, ..Default::default() });
        }
    }
    record_rename(app, channel_id, &w.recent).await;
    ok_result("rename", OkParams { new_name: Some(name), ..Default::default() })
}

/// `changeOwner(...)`: overwrites, row, text visibility, panel refresh and a public notice. The caller holds `ch:`.
async fn change_owner(app: &App, guild_id: GuildId, vc: &GuildChannel, rh: &RowWithHub, new_owner_id: &str, settings: Option<&bot_db::GuildSettings>, reason: &str) -> Result<(), Box<(Msg, MsgParams)>> {
    let row = &rh.row;
    let banned: Vec<String> = row.banned_user_ids.iter().filter(|id| *id != new_owner_id).cloned().collect();
    let st = VoiceState2 { hub: rh.hub.clone(), owner_id: new_owner_id.to_string(), locked: row.locked, hidden: row.hidden, banned_user_ids: banned.clone() };
    if let Err(f) = apply_voice_overwrites(app, guild_id, vc, &st, settings, reason).await {
        return Err(Box::new((if f.hub_gone { Msg::HubGone } else { Msg::DiscordError }, MsgParams { code: f.code, ..Default::default() })));
    }
    if let Err(e) = update_row_strict(app, &row.channel_id, vec![("ownerId", json!(new_owner_id)), ("ownerLeftAt", Value::Null), ("bannedUserIds", json!(banned))]).await {
        log_error!(format!("tempvoice: could not save the new owner of {}", row.channel_id), bot_core::error::log_arg(&e));
        return Err(Box::new((Msg::DbError, MsgParams::default())));
    }
    if row.text_channel_id.is_some() {
        request_text_sync(app, guild_id, &row.channel_id);
    }
    refresh_control_panel(app, guild_id, row, vc.id, new_owner_id, settings).await;
    let panel_channel = row.text_channel_id.as_deref().and_then(|t| cached_channel(app, guild_id, t)).map(|c| c.id).unwrap_or(vc.id);
    if check_channel_usable(app, guild_id, Some(panel_channel), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS).ok {
        let g = guild_id.to_string();
        let msg = Payload::text(format!("👑 <@{new_owner_id}> is now the owner of this channel.")).no_mentions().to_message();
        let _ = discord_call(app, "ownership notice", Some(&g), rest(app).send_message(guild_id, panel_channel, msg)).await;
    }
    Ok(())
}

/// The member a kick / ban / transfer is aimed at (`guild.members.fetch(id)`: the cache first).
async fn fetch_target(app: &App, guild_id: GuildId, target_id: &str) -> Option<Member> {
    let user = parse_user(target_id)?;
    if let Some(m) = with_guild(app, guild_id, |g| g.members.get(&user).cloned()).flatten() {
        return Some(m);
    }
    rest(app).fetch_member(guild_id, user).await.ok()
}

async fn run_voice_action_locked(app: &App, req: VoiceActionReq) -> anyhow::Result<ActionResult> {
    let VoiceActionReq { guild_id, actor, channel_id, action, target_id, value, settings, source } = req;
    if !VOICE_ACTIONS.contains(&action.as_str()) {
        return Ok(fail0(Msg::Internal));
    }
    let g = guild_id.to_string();
    let Some(rh) = load_row(app, &channel_id).await?.filter(|r| r.row.guild_id == g) else {
        return Ok(fail0(if source == ActionSource::Command { Msg::NotInTemp } else { Msg::ChannelGone }));
    };
    let vc = match confirm_channel(app, guild_id, &channel_id).await {
        ChannelCheck::Gone => {
            purge_row(app, guild_id, &rh.row, Some(&rh.hub)).await?;
            return Ok(fail0(Msg::ChannelGone));
        }
        ChannelCheck::Unknown(code) => return Ok(fail(Msg::DiscordError, MsgParams { code, ..Default::default() })),
        ChannelCheck::Present(c) => *c,
    };
    let row = &rh.row;
    let settings_ref = settings.as_deref();
    let actor_id = actor.user.id.to_string();
    let is_owner = actor_id == row.owner_id;
    let is_mod = is_temp_voice_mod(app, guild_id, &actor, Some(&rh.hub), settings_ref);
    if !can_control(&action, is_owner, is_mod) {
        return Ok(fail0(Msg::NotAllowed));
    }
    let missing = missing_bot_permissions(app, PermTarget::Channel(guild_id, vc.id), action_perms(&action));
    if !missing.is_empty() {
        return Ok(fail(Msg::BotMissing, MsgParams { names: permission_names(&missing), channel_id: Some(channel_id.clone()), ..Default::default() }));
    }
    let reason = clamp_text(&format!("Temp voice: {action} by {}", bot_core::adapters::user_tag(&actor.user)), 512);
    let name = Some(vc.name.clone());
    let target = target_id.clone().unwrap_or_default();
    let refuse = |m: Msg| fail(m, MsgParams { target: target_id.clone(), owner: Some(row.owner_id.clone()), ..Default::default() });
    let vc_id = vc.id;
    let connected = |user: UserId| with_guild(app, guild_id, |gd| connected_to(gd, user, vc_id)).unwrap_or(false);

    // Applies a state patch to the overwrites, then saves it.
    let apply_and_save = |locked: bool, hidden: bool, banned: Vec<String>, patch: Vec<(&'static str, Value)>| {
        let rh = rh.clone();
        let vc = vc.clone();
        let reason = reason.clone();
        let channel_id = channel_id.clone();
        let action = action.clone();
        async move {
            let st = VoiceState2 { hub: rh.hub.clone(), owner_id: rh.row.owner_id.clone(), locked, hidden, banned_user_ids: banned };
            if let Err(f) = apply_voice_overwrites(app, guild_id, &vc, &st, settings_ref, &reason).await {
                return Some(fail(if f.hub_gone { Msg::HubGone } else { Msg::DiscordError }, MsgParams { code: f.code, ..Default::default() }));
            }
            if let Err(e) = update_row_strict(app, &channel_id, patch).await {
                log_error!(format!("tempvoice: could not save {action} for {channel_id}"), bot_core::error::log_arg(&e));
                return Some(fail0(Msg::DbError));
            }
            None
        }
    };

    let result = match action.as_str() {
        "lock" | "unlock" | "hide" | "reveal" => {
            let is_lock_field = action == "lock" || action == "unlock";
            let next = action == "lock" || action == "hide";
            let current = if is_lock_field { row.locked } else { row.hidden };
            if current == next {
                return Ok(fail0(match action.as_str() {
                    "lock" => Msg::NoChangeLock,
                    "unlock" => Msg::NoChangeUnlock,
                    "hide" => Msg::NoChangeHide,
                    _ => Msg::NoChangeReveal,
                }));
            }
            let (l, h) = if is_lock_field { (next, row.hidden) } else { (row.locked, next) };
            let field = if is_lock_field { "locked" } else { "hidden" };
            match apply_and_save(l, h, row.banned_user_ids.clone(), vec![(field, json!(next))]).await {
                Some(err) => err,
                None => ok_result(&action, OkParams { name, ..Default::default() }),
            }
        }
        "limit" => {
            let n = match value {
                ActionValue::Int(n) => n,
                _ => return Ok(fail0(Msg::LimitInvalid)),
            };
            if !(0..=99).contains(&n) {
                return Ok(fail0(Msg::LimitInvalid));
            }
            if i64::from(vc.user_limit.unwrap_or(0)) == n {
                return Ok(fail(Msg::NoChangeLimit, MsgParams { n: Some(n), ..Default::default() }));
            }
            match discord_call(app, "set limit", Some(&g), rest(app).set_user_limit(guild_id, vc.id, n as u32, &reason)).await {
                Err(e) => fail(Msg::DiscordError, MsgParams { code: e.code, ..Default::default() }),
                Ok(()) => ok_result("limit", OkParams { name, n: Some(n), ..Default::default() }),
            }
        }
        "rename" => rename_channel(app, guild_id, &vc, &channel_id, &value, &reason).await,
        "kick" => {
            let Some(t) = fetch_target(app, guild_id, &target).await else { return Ok(fail0(Msg::TargetNotMember)) };
            let problem = target_problem(&TargetInput {
                action: "kick",
                actor_id: &actor_id,
                target_id: &target,
                owner_id: &row.owner_id,
                target_protected: is_protected_target(app, guild_id, &t, Some(&rh.hub), settings_ref),
                target_connected: connected(t.user.id),
                ..Default::default()
            });
            if let Some(p) = problem {
                return Ok(refuse(p));
            }
            match discord_call(app, "kick", Some(&g), rest(app).move_member(guild_id, t.user.id, None, &reason)).await {
                Err(e) if e.code == Some(40032) => refuse(Msg::TargetNotConnected),
                Err(e) => fail(Msg::DiscordError, MsgParams { code: e.code, ..Default::default() }),
                Ok(()) => ok_result("kick", OkParams { name, target: Some(target.clone()), ..Default::default() }),
            }
        }
        "ban" => {
            let Some(t) = fetch_target(app, guild_id, &target).await else { return Ok(fail0(Msg::TargetNotMember)) };
            let problem = target_problem(&TargetInput {
                action: "ban",
                actor_id: &actor_id,
                target_id: &target,
                owner_id: &row.owner_id,
                target_protected: is_protected_target(app, guild_id, &t, Some(&rh.hub), settings_ref),
                is_banned: row.banned_user_ids.contains(&target),
                ban_count: row.banned_user_ids.len(),
                ..Default::default()
            });
            if let Some(p) = problem {
                return Ok(refuse(p));
            }
            let mut banned = row.banned_user_ids.clone();
            banned.push(target.clone());
            if let Some(err) = apply_and_save(row.locked, row.hidden, banned.clone(), vec![("bannedUserIds", json!(banned))]).await {
                return Ok(err);
            }
            if connected(t.user.id) && missing_bot_permissions(app, PermTarget::Channel(guild_id, vc.id), &["MoveMembers"]).is_empty() {
                let _ = discord_call(app, "kick banned", Some(&g), rest(app).move_member(guild_id, t.user.id, None, &reason)).await;
            }
            if row.text_channel_id.is_some() {
                request_text_sync(app, guild_id, &channel_id);
            }
            ok_result("ban", OkParams { name, target: Some(target.clone()), ..Default::default() })
        }
        "unban" => {
            // No member fetch: the user may have left the server.
            let problem = target_problem(&TargetInput {
                action: "unban",
                actor_id: &actor_id,
                target_id: &target,
                owner_id: &row.owner_id,
                is_banned: row.banned_user_ids.contains(&target),
                ..Default::default()
            });
            if let Some(p) = problem {
                return Ok(refuse(p));
            }
            let banned: Vec<String> = row.banned_user_ids.iter().filter(|id| **id != target).cloned().collect();
            if let Some(err) = apply_and_save(row.locked, row.hidden, banned.clone(), vec![("bannedUserIds", json!(banned))]).await {
                return Ok(err);
            }
            if row.text_channel_id.is_some() {
                request_text_sync(app, guild_id, &channel_id);
            }
            ok_result("unban", OkParams { name, target: Some(target.clone()), ..Default::default() })
        }
        "transfer" => {
            let Some(t) = fetch_target(app, guild_id, &target).await else { return Ok(fail0(Msg::TargetNotMember)) };
            let problem = target_problem(&TargetInput {
                action: "transfer",
                actor_id: &actor_id,
                target_id: &target,
                owner_id: &row.owner_id,
                target_is_bot: t.user.bot,
                target_connected: connected(t.user.id),
                ..Default::default()
            });
            if let Some(p) = problem {
                return Ok(refuse(p));
            }
            let new_owner = t.user.id.to_string();
            match change_owner(app, guild_id, &vc, &rh, &new_owner, settings_ref, &reason).await {
                Ok(()) => ok_result("transfer", OkParams { name, target: Some(new_owner), ..Default::default() }),
                Err(e) => fail(e.0, e.1),
            }
        }
        "claim" => {
            let owner_connected = parse_user(&row.owner_id).is_some_and(&connected);
            let mut owner_left_at = date_ms(&row.owner_left_at);
            if !owner_connected && owner_left_at.is_none() {
                // The owner left while the bot was not watching: the ownership-lock timer starts now.
                let now = now_ms(app);
                owner_left_at = Some(now as f64);
                if let Err(e) = update_row(app, &channel_id, vec![("ownerLeftAt", date_json(now))]).await {
                    log_warn!(format!("tempvoice: could not store ownerLeftAt: {e}"));
                }
            }
            let st = claim_status(&ClaimInput {
                is_owner,
                actor_connected: connected(actor.user.id),
                owner_connected,
                owner_left_at,
                lock_minutes: f64::from(rh.hub.ownership_lock_minutes),
                is_mod,
                now: now_ms(app) as f64,
            });
            match st {
                ClaimStatus::AlreadyOwner => fail0(Msg::ClaimAlreadyOwner),
                ClaimStatus::NotConnected => fail0(Msg::ClaimNotConnected),
                ClaimStatus::OwnerPresent => fail(Msg::ClaimOwnerPresent, MsgParams { owner: Some(row.owner_id.clone()), ..Default::default() }),
                ClaimStatus::Locked { claimable_at } => fail(Msg::ClaimLocked, MsgParams { at: Some(claimable_at), ..Default::default() }),
                ClaimStatus::Ok => match change_owner(app, guild_id, &vc, &rh, &actor_id, settings_ref, &reason).await {
                    Ok(()) => ok_result("claim", OkParams { name, ..Default::default() }),
                    Err(e) => fail(e.0, e.1),
                },
            }
        }
        _ => fail0(Msg::Internal),
    };
    Ok(result)
}

// ── Hooks: voiceStateUpdate, channelDelete, guildReady (spec §5, §6.4, §6.14, §6.15) ──

/// `voiceFilter(ctx)`: skips mute/deafen/stream changes and, with a fresh memo, every event that touches no hub or
/// temp channel. Sync, cheap.
pub fn voice_filter(app: &App, guild_id: GuildId, change_is_state: bool, old_id: Option<ChannelId>, new_id: Option<ChannelId>) -> bool {
    if change_is_state {
        return false;
    }
    let Some(idx) = peek_index(app, &guild_id.to_string()) else { return true };
    let old = old_id.map(|c| c.to_string());
    let new = new_id.map(|c| c.to_string());
    new.as_ref().is_some_and(|n| idx.hubs.contains_key(n) || idx.temps.contains(n)) || old.as_ref().is_some_and(|o| idx.temps.contains(o))
}

#[allow(clippy::too_many_arguments)] // the JS closure's captures, passed explicitly
async fn voice_steps(app: App, guild_id: GuildId, member: Member, old: Option<String>, new: Option<String>, idx: Arc<GuildIndex>, first: Option<BoxFuture<'static, anyhow::Result<()>>>, hub_settings: Option<Option<Settings>>) -> anyhow::Result<()> {
    let mut first = first;
    if let Some(o) = old.clone().filter(|o| idx.temps.contains(o)) {
        let step = match first.take() {
            Some(f) => f,
            None => {
                let (a, m) = (app.clone(), member.clone());
                locked(&app, &format!("ch:{o}"), move || async move { on_leave_temp(&a, guild_id, &o, &m).await }).boxed()
            }
        };
        step.await?;
    }
    if let Some(n) = new.clone().filter(|n| idx.temps.contains(n)) {
        let step = match first.take() {
            Some(f) => f,
            None => {
                let (a, m) = (app.clone(), member.clone());
                locked(&app, &format!("ch:{n}"), move || async move { on_join_temp(&a, guild_id, &n, &m).await }).boxed()
            }
        };
        step.await?;
    }
    if let Some(hub_id) = new.as_ref().and_then(|n| idx.hubs.get(n)).cloned() {
        let settings = match hub_settings {
            Some(s) => s,
            None => app.settings().get_settings_safe(&guild_id.to_string()).await,
        };
        if bot_core::settings::is_module_enabled(settings.as_deref(), Some(bot_config::ModuleKey::Tempvoice)) {
            queue_hub_join(app.clone(), guild_id, hub_id, member, settings).await;
        }
    }
    Ok(())
}

/// `onVoiceStateUpdate(ctx)`: returns at once; the work is queued ([`run_task`]) so the other voice hooks are never
/// delayed. With a fresh memo the first lock place is taken synchronously, so consecutive events of one channel reach
/// `ch:` in gateway order.
pub fn on_voice_state_update(app: &App, guild_id: GuildId, member: Member, old_id: Option<ChannelId>, new_id: Option<ChannelId>, settings: Option<Option<Settings>>) {
    if old_id == new_id {
        return;
    }
    let old = old_id.map(|c| c.to_string());
    let new = new_id.map(|c| c.to_string());
    let a = app.clone();
    match peek_index(app, &guild_id.to_string()) {
        Some(idx) => {
            // Take the first lock place now (gateway order).
            let first: Option<BoxFuture<'static, anyhow::Result<()>>> = if let Some(o) = old.clone().filter(|o| idx.temps.contains(o)) {
                let (a2, m) = (app.clone(), member.clone());
                Some(locked(app, &format!("ch:{o}"), move || async move { on_leave_temp(&a2, guild_id, &o, &m).await }).boxed())
            } else if let Some(n) = new.clone().filter(|n| idx.temps.contains(n)) {
                let (a2, m) = (app.clone(), member.clone());
                Some(locked(app, &format!("ch:{n}"), move || async move { on_join_temp(&a2, guild_id, &n, &m).await }).boxed())
            } else {
                None
            };
            run_task(app, "voice", voice_steps(a, guild_id, member, old, new, idx, first, settings));
        }
        None => {
            run_task(app, "voice", async move {
                let Some(idx) = load_index(&a, &guild_id.to_string()).await else { return Ok(()) };
                voice_steps(a, guild_id, member, old, new, idx, None, settings).await
            });
        }
    }
}

/// `channelDeleteFilter(ctx)`: with a fresh memo only deletions of a hub, temp channel, companion text channel or hub
/// category matter.
pub fn channel_delete_filter(app: &App, guild_id: GuildId, channel_id: ChannelId) -> bool {
    let Some(idx) = peek_index(app, &guild_id.to_string()) else { return true };
    let id = channel_id.to_string();
    idx.hubs.contains_key(&id) || idx.temps.contains(&id) || idx.texts.contains_key(&id) || idx.categories.contains(&id)
}

/// `onChannelDelete(ctx)`: a delete event is authoritative, no REST confirmation is needed. Queued with
/// [`run_task`].
pub fn on_channel_delete(app: &App, guild_id: GuildId, channel_id: ChannelId) {
    let a = app.clone();
    run_task(app, "channelDelete", async move {
        let g = guild_id.to_string();
        let id = channel_id.to_string();
        let Some(idx) = load_index(&a, &g).await else { return Ok(()) };
        if idx.temps.contains(&id) {
            let (a2, id2) = (a.clone(), id.clone());
            locked(&a, &format!("ch:{id}"), move || async move {
                let Some(RowWithHub { row, hub }) = load_row(&a2, &id2).await? else { return Ok::<_, anyhow::Error>(()) };
                if let Some(text) = row.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
                    let _ = delete_discord_channel(&a2, guild_id, text, "Temp voice: channel closed", "delete text").await;
                }
                purge_row(&a2, guild_id, &row, Some(&hub)).await
            })
            .await?;
        }
        if let Some(voice_id) = idx.texts.get(&id).cloned() {
            let (a2, id2, v2, g2) = (a.clone(), id.clone(), voice_id.clone(), g.clone());
            locked(&a, &format!("ch:{voice_id}"), move || async move {
                let sql = r#"UPDATE "TempVoiceChannel" SET "textChannelId" = NULL, "controlMessageId" = NULL, "updatedAt" = $3 WHERE "channelId" = $1 AND "textChannelId" = $2"#;
                sqlx::query(sql).bind(&v2).bind(&id2).bind(bot_db::now_utc().naive_utc()).execute(a2.db()).await.map_err(DbError::from)?;
                invalidate_index(&a2, &g2);
                Ok::<_, anyhow::Error>(())
            })
            .await?;
        }
        if let Some(hub_id) = idx.hubs.get(&id).cloned() {
            let count = hub_channel_count(&a, &hub_id).await?;
            if count == 0 {
                delete_where(&TEMP_VOICE_HUB_INFO, [("id", json!(hub_id))])?.execute(a.db()).await?;
            } else {
                log_info!(format!("tempvoice: hub {hub_id} retired ({count} channel(s) still open)"));
            }
            invalidate_index(&a, &g);
        }
        if idx.categories.contains(&id) {
            let sql = r#"UPDATE "TempVoiceHub" SET "categoryId" = NULL, "updatedAt" = $3 WHERE "guildId" = $1 AND "categoryId" = $2"#;
            sqlx::query(sql).bind(&g).bind(&id).bind(bot_db::now_utc().naive_utc()).execute(a.db()).await.map_err(DbError::from)?;
            invalidate_index(&a, &g);
        }
        Ok(())
    });
}

/// `onGuildReady(ctx)`: queues the startup / join / availability reconciliation of one guild.
pub fn on_guild_ready(app: &App, guild_id: GuildId, settings: Option<Option<Settings>>) {
    let a = app.clone();
    run_task(app, "reconcile", async move {
        reconcile_guild(&a, guild_id, settings).await;
        Ok(())
    });
}

// ── Reconciliation (spec §6.15) ───────────────────────────────────────────────

async fn reconcile_row(app: &App, guild_id: GuildId, channel_id: &str, now: i64) -> anyhow::Result<()> {
    let Some(RowWithHub { row, hub }) = load_row(app, channel_id).await? else { return Ok(()) };
    let chk = confirm_channel(app, guild_id, &row.channel_id).await;
    let channel = match &chk {
        ChannelCheck::Unknown(_) => return Ok(()), // rule 20: keep the row
        ChannelCheck::Present(c) => Some(c.id),
        ChannelCheck::Gone => None,
    };
    let humans = channel.map(|c| count_humans(app, guild_id, c)).unwrap_or(0);
    let decision = reconcile_decision(channel.is_some(), humans, i64::from(hub.keep_alive_minutes), date_ms(&row.empty_since), now as f64);
    match decision {
        ReconcileAction::Purge => {
            if let Some(text) = row.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
                let _ = delete_discord_channel(app, guild_id, text, "Temp voice: channel closed", "delete text").await;
            }
            purge_row(app, guild_id, &row, Some(&hub)).await?;
        }
        ReconcileAction::Delete => {
            if delete_temp_channel(app, guild_id, &row, Some(&hub), "empty after restart").await?.is_err() {
                schedule_delete_retry(app, guild_id, &row.channel_id).await;
            }
        }
        ReconcileAction::Schedule { run_at, set_empty_since } => {
            if set_empty_since {
                update_row(app, &row.channel_id, vec![("emptySince", date_json(now))]).await?;
            }
            app.scheduler()
                .schedule_job(
                    ScheduleJob::new(guild_id.to_string(), JobType::TempvoiceCleanup, ms_to_date(run_at as i64))
                        .payload(json!({ "channelId": row.channel_id }))
                        .dedupe_key(cleanup_key(&row.channel_id))
                        .max_attempts(5)
                        .on_conflict(OnConflict::Keep),
                )
                .await?;
        }
        ReconcileAction::Occupied => {
            let mut data: Vec<(&str, Value)> = Vec::new();
            if row.empty_since.is_some() {
                data.push(("emptySince", Value::Null));
                app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(cleanup_key(&row.channel_id)), ..Default::default() }).await;
            }
            let owner_here = match (channel, parse_user(&row.owner_id)) {
                (Some(c), Some(u)) => with_guild(app, guild_id, |g| connected_to(g, u, c)).unwrap_or(false),
                _ => false,
            };
            if !owner_here && row.owner_left_at.is_none() {
                data.push(("ownerLeftAt", date_json(now)));
            }
            if owner_here && row.owner_left_at.is_some() {
                data.push(("ownerLeftAt", Value::Null));
            }
            let mut text_present = false;
            if let Some(text) = row.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
                match confirm_channel(app, guild_id, text).await {
                    ChannelCheck::Gone => {
                        data.push(("textChannelId", Value::Null));
                        data.push(("controlMessageId", Value::Null));
                    }
                    ChannelCheck::Present(_) => text_present = true,
                    ChannelCheck::Unknown(_) => {}
                }
            }
            if !data.is_empty() {
                update_row(app, &row.channel_id, data).await?;
            }
            if text_present && hub.text_channel_members_only {
                request_text_sync(app, guild_id, &row.channel_id);
            }
        }
    }
    Ok(())
}

/// `reconcileGuild(guild, ctx)`: repairs rows after a restart or join, and creates channels for members already
/// waiting in a hub. `settings`: what the hook context loaded (`None` loads them). Never fails.
pub async fn reconcile_guild(app: &App, guild_id: GuildId, settings: Option<Option<Settings>>) {
    if !guild_available(app, guild_id) {
        return;
    }
    let g = guild_id.to_string();
    let loaded: anyhow::Result<(Vec<TempVoiceHub>, Vec<TempVoiceChannel>)> = async {
        let hubs: Vec<TempVoiceHub> = SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("guildId", json!(g))?.build()?.fetch_all(app.db()).await?;
        let rows: Vec<TempVoiceChannel> = SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO).filter("guildId", json!(g))?.build()?.fetch_all(app.db()).await?;
        Ok((hubs, rows))
    }
    .await;
    let (hubs, rows) = match loaded {
        Ok(v) => v,
        Err(e) => {
            log_error!(format!("tempvoice: reconcile could not read guild {g}"), bot_core::error::log_arg(&e));
            return;
        }
    };
    if hubs.is_empty() && rows.is_empty() {
        invalidate_index(app, &g);
        return;
    }
    let settings = match settings {
        Some(s) => s,
        None => app.settings().get_settings_safe(&g).await,
    };
    let now = now_ms(app);

    let ids: Vec<String> = rows.into_iter().map(|r| r.channel_id).collect();
    let results = bot_util::async_util::map_limit(ids, 2, |id, _| {
        let a = app.clone();
        locked(app, &format!("ch:{id}"), move || async move { reconcile_row(&a, guild_id, &id, now).await })
    })
    .await;
    for r in results {
        if let Err(e) = r {
            log_error!("tempvoice: reconcile of a channel failed", bot_core::error::log_arg(&e));
        }
    }

    for hub in &hubs {
        let res: anyhow::Result<()> = async {
            if retire_hub_if_done(app, guild_id, &hub.id, Some((hub.guild_id.clone(), hub.channel_id.clone()))).await? {
                return Ok(());
            }
            if let Some(cat) = hub.category_id.as_deref().filter(|c| !c.is_empty())
                && cached_channel(app, guild_id, cat).is_none()
                && matches!(confirm_channel(app, guild_id, cat).await, ChannelCheck::Gone)
            {
                let sql = r#"UPDATE "TempVoiceHub" SET "categoryId" = NULL, "updatedAt" = $2 WHERE "id" = $1"#;
                sqlx::query(sql).bind(&hub.id).bind(bot_db::now_utc().naive_utc()).execute(app.db()).await.map_err(DbError::from)?;
                invalidate_index(app, &g);
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            log_error!(format!("tempvoice: reconcile of hub {} failed", hub.id), bot_core::error::log_arg(&e));
        }
    }
    invalidate_index(app, &g);

    // Members already waiting in a hub (they joined while the bot was offline).
    if !bot_core::settings::is_module_enabled(settings.as_deref(), Some(bot_config::ModuleKey::Tempvoice)) {
        return;
    }
    let live: anyhow::Result<Vec<TempVoiceHub>> = async { Ok(SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("guildId", json!(g))?.build()?.fetch_all(app.db()).await?) }.await;
    match live {
        Ok(live) => {
            for hub in live {
                let Some(hub_channel) = cached_channel(app, guild_id, &hub.channel_id) else { continue };
                let waiting: Vec<Member> = with_guild(app, guild_id, |gd| {
                    gd.voice_states
                        .values()
                        .filter(|v| v.channel_id == Some(hub_channel.id))
                        .filter_map(|v| v.member.clone().or_else(|| gd.members.get(&v.user_id).cloned()))
                        .filter(|m| !m.user.bot)
                        .collect()
                })
                .unwrap_or_default();
                for member in waiting {
                    queue_hub_join(app.clone(), guild_id, hub.id.clone(), member, settings.clone()).await;
                }
            }
        }
        Err(e) => log_error!(format!("tempvoice: could not serve the members waiting in hubs of guild {g}"), bot_core::error::log_arg(&e)),
    }
}

// ── /voice clean (spec §6.16) ─────────────────────────────────────────────────

/// `cleanEmptyChannels({ guild, actor, settings })`: deletes every temp channel of the server that has no human
/// members. Moderators only. Returns the embed to answer with.
pub async fn clean_empty_channels(app: &App, guild_id: GuildId, actor: &Member, settings: Option<&bot_db::GuildSettings>) -> anyhow::Result<CreateEmbed> {
    let g = guild_id.to_string();
    let hubs: Vec<TempVoiceHub> = SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("guildId", json!(g))?.build()?.fetch_all(app.db()).await?;
    let allowed = member_level(app, guild_id, actor, settings) >= PermissionLevel::Moderator || hubs.iter().any(|h| has_any_role(&actor.roles, &h.moderator_role_ids));
    if !allowed {
        return Ok(Msg::CleanNotAllowed.plain());
    }
    let rows: Vec<TempVoiceChannel> = SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO).filter("guildId", json!(g))?.build()?.fetch_all(app.db()).await?;
    let ids: Vec<String> = rows.into_iter().map(|r| r.channel_id).collect();
    let results = bot_util::async_util::map_limit(ids, 2, |id, _| {
        let a = app.clone();
        locked(app, &format!("ch:{id}"), move || async move {
            let Some(RowWithHub { row, hub }) = load_row(&a, &id).await? else { return Ok::<bool, anyhow::Error>(false) };
            match confirm_channel(&a, guild_id, &row.channel_id).await {
                ChannelCheck::Gone => {
                    purge_row(&a, guild_id, &row, Some(&hub)).await?; // not counted
                    Ok(false)
                }
                ChannelCheck::Unknown(_) => Ok(false),
                ChannelCheck::Present(c) => {
                    if count_humans(&a, guild_id, c.id) > 0 {
                        return Ok(false);
                    }
                    Ok(delete_temp_channel(&a, guild_id, &row, Some(&hub), "cleaned up by a moderator").await?.is_ok())
                }
            }
        })
    })
    .await;
    let n = results.iter().filter(|r| matches!(r, Ok(true))).count();
    Ok(ok_embed("clean", &OkParams { n: Some(n as i64), ..Default::default() }))
}

// ── Component handlers (spec §6.12) ───────────────────────────────────────────

const IMMEDIATE_ACTIONS: [&str; 5] = ["lock", "unlock", "hide", "reveal", "claim"];

/// `checkControl(...)`: the read-only check before a modal or picker is shown.
pub async fn check_control(app: &App, guild_id: GuildId, actor: &Member, channel_id: &str, action: &str, settings: Option<&bot_db::GuildSettings>) -> anyhow::Result<Result<(RowWithHub, GuildChannel), CreateEmbed>> {
    let Some(rh) = load_row(app, channel_id).await?.filter(|r| r.row.guild_id == guild_id.to_string()) else {
        return Ok(Err(Msg::ChannelGone.plain()));
    };
    let Some(vc) = cached_channel(app, guild_id, channel_id) else { return Ok(Err(Msg::ChannelGone.plain())) }; // no purge
    let is_owner = actor.user.id.to_string() == rh.row.owner_id;
    if !can_control(action, is_owner, is_temp_voice_mod(app, guild_id, actor, Some(&rh.hub), settings)) {
        return Ok(Err(Msg::NotAllowed.plain()));
    }
    Ok(Ok((rh, vc)))
}

// ── /voice-hub: summaries and problems (spec §4.2.7) ──────────────────────────

fn roles_text(ids: &[String]) -> String {
    if ids.is_empty() {
        return "none".into();
    }
    let mentions: Vec<String> = ids.iter().map(|id| format!("<@&{id}>")).collect();
    join_with_limit(&mentions, ", ", 300, bot_util::text::default_more)
}

/// `Math.round(bps / 1000)`.
fn kbps(bps: i32) -> String {
    js::number_to_string(bot_util::text::js_round(f64::from(bps) / 1000.0))
}

/// `hubSummary(hub, guild)`: the multi-line settings summary of a hub.
pub fn hub_summary(hub: &TempVoiceHub, guild: Option<&Guild>) -> String {
    let hub_has_parent = guild
        .and_then(|g| parse_channel(&hub.channel_id).and_then(|c| g.channels.get(&c)))
        .is_some_and(|c| c.parent_id.is_some());
    let where_ = match hub.category_id.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => format!("<#{c}>"),
        None => (if hub_has_parent { "the hub's category" } else { "top level" }).to_string(),
    };
    let text_mode = if !hub.create_text_channel {
        "off"
    } else if hub.text_channel_members_only {
        "private"
    } else {
        "public"
    };
    let bitrate = match hub.bitrate.filter(|b| *b != 0) {
        Some(b) => format!("{} kbps", kbps(b)),
        None => "default".into(),
    };
    [
        format!("Hub: <#{}> · New channels in: {where_}", hub.channel_id),
        format!("Name: `{}`", hub.name_format.replace('`', "ˋ")),
        format!(
            "Limit: {} · Bitrate: {bitrate} · Keep alive: {}",
            if hub.user_limit > 0 { hub.user_limit.to_string() } else { "none".into() },
            if hub.keep_alive_minutes > 0 { format!("{} min", hub.keep_alive_minutes) } else { "delete at once".into() }
        ),
        format!(
            "Ownership lock: {} · Permissions: {}",
            if hub.ownership_lock_minutes > 0 { format!("{} min", hub.ownership_lock_minutes) } else { "none".into() },
            if hub.sync_permissions { "copied from category" } else { "copied from hub channel" }
        ),
        format!("Text channel: {text_mode}"),
        format!("Mod roles: {} · Ignored roles: {}", roles_text(&hub.moderator_role_ids), roles_text(&hub.ignored_role_ids)),
    ]
    .join("\n")
}

/// `hubProblems(hub, guild)`: what currently stops a hub from working (display only).
pub fn hub_problems(app: &App, guild_id: GuildId, hub: &TempVoiceHub) -> Vec<String> {
    let mut problems = Vec::new();
    let hub_channel = cached_channel(app, guild_id, &hub.channel_id);
    if hub_channel.is_none() {
        problems.push("Hub channel deleted — this hub stops creating channels and disappears once its channels close. Use /voice-hub create to set up a new one.".to_string());
    }
    let target_parent = hub
        .category_id
        .as_deref()
        .and_then(|c| cached_channel(app, guild_id, c))
        .or_else(|| hub_channel.as_ref().and_then(|h| h.parent_id).and_then(|p| cached_channel(app, guild_id, &p.to_string())));
    let target = match &target_parent {
        Some(p) => PermTarget::Channel(guild_id, p.id),
        None => PermTarget::Guild(guild_id),
    };
    let missing = missing_bot_permissions(app, target, &REQUIRED_BOT_PERMS);
    if !missing.is_empty() {
        let where_ = match &target_parent {
            Some(p) => format!("**{}**", escape_markdown(&clamp_text(&p.name, 100))),
            None => "the server".into(),
        };
        problems.push(format!("I'm missing {} in {where_}", permission_names(&missing).join(", ")));
    }
    if let Some(h) = &hub_channel {
        let hub_missing = missing_bot_permissions(app, PermTarget::Channel(guild_id, h.id), &HUB_CHANNEL_PERMS);
        if !hub_missing.is_empty() {
            problems.push(format!("I'm missing {} in the hub channel", permission_names(&hub_missing).join(", ")));
        }
    }
    if let Some(p) = &target_parent {
        let children = with_guild(app, guild_id, |g| g.channels.values().filter(|c| c.parent_id == Some(p.id)).count()).unwrap_or(0);
        if children >= 50 {
            problems.push("The category is full (50 channels)".into());
        }
    }
    problems
}

// ── CRUD used by /voice-hub and the dashboard (spec §6.19) ────────────────────

/// `countHubs(guildId)`.
pub async fn count_hubs(db: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "TempVoiceHub" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(db).await.map_err(DbError::from)?;
    Ok(n)
}

/// `findHubByChannel(guildId, channelId)`: the hub of that channel in this guild.
pub async fn find_hub_by_channel(db: &bot_db::PgPool, guild_id: &str, channel_id: &str) -> anyhow::Result<Option<TempVoiceHub>> {
    let hub: Option<TempVoiceHub> = SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("channelId", json!(channel_id))?.build()?.fetch_optional(db).await?;
    Ok(hub.filter(|h| h.guild_id == guild_id))
}

/// `isTempChannel(channelId)`.
pub async fn is_temp_channel(db: &bot_db::PgPool, channel_id: &str) -> anyhow::Result<bool> {
    let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "TempVoiceChannel" WHERE "channelId" = $1"#).bind(channel_id).fetch_one(db).await.map_err(DbError::from)?;
    Ok(n > 0)
}

/// A hub with its number of open temporary channels (`include: { _count: { select: { channels: true } } }`).
#[derive(Debug, Clone, PartialEq)]
pub struct HubWithCount {
    pub hub: TempVoiceHub,
    pub channels: i64,
}

/// `listHubs(guildId)`: oldest first, with the channel counts.
pub async fn list_hubs(db: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<Vec<HubWithCount>> {
    let hubs: Vec<TempVoiceHub> =
        SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("guildId", json!(guild_id))?.order_by("createdAt", true)?.build()?.fetch_all(db).await?;
    let counts: Vec<(String, i64)> = sqlx::query_as(r#"SELECT "hubId", COUNT(*)::bigint FROM "TempVoiceChannel" WHERE "guildId" = $1 GROUP BY "hubId""#)
        .bind(guild_id)
        .fetch_all(db)
        .await
        .map_err(DbError::from)?;
    let counts: HashMap<String, i64> = counts.into_iter().collect();
    Ok(hubs.into_iter().map(|hub| HubWithCount { channels: counts.get(&hub.id).copied().unwrap_or(0), hub }).collect())
}

/// `listHubChannels(hubId)`.
pub async fn list_hub_channels(db: &bot_db::PgPool, hub_id: &str) -> anyhow::Result<Vec<TempVoiceChannel>> {
    Ok(SelectBuilder::new(&TEMP_VOICE_CHANNEL_INFO).filter("hubId", json!(hub_id))?.build()?.fetch_all(db).await?)
}

/// The settable fields of a hub, by Prisma field name (only the given ones; schema defaults apply to the rest).
pub type HubData = Vec<(&'static str, Value)>;

/// `createHub(guildId, data)`: creates a hub under the GuildSettings row lock so the per-guild cap holds under
/// concurrency. Fails with [`HubLimitError`] at the cap; a duplicate channel is a unique violation
/// ([`is_duplicate_hub`]).
pub async fn create_hub(app: &App, guild_id: &str, data: HubData) -> anyhow::Result<TempVoiceHub> {
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let g = guild_id.to_string();
    let hub = bot_db::transaction(app.db(), TxOptions::default(), move |tx| {
        Box::pin(async move {
            sqlx::query(GUILD_LOCK_SQL).bind(&g).execute(&mut *tx).await.map_err(DbError::from)?;
            let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "TempVoiceHub" WHERE "guildId" = $1"#)
                .bind(&g)
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::from)?;
            if n >= limits::TEMPVOICE_HUBS as i64 {
                return Err(anyhow::Error::new(HubLimitError));
            }
            let mut b = InsertBuilder::new(&TEMP_VOICE_HUB_INFO).value("guildId", json!(g))?;
            for (k, v) in data {
                b = b.value(k, v)?;
            }
            let hub: TempVoiceHub = b.build()?.fetch_one(&mut *tx).await?;
            Ok(hub)
        })
    })
    .await?;
    invalidate_index(app, guild_id);
    Ok(hub)
}

/// `updateHub(guildId, hubId, patch)`: the updated hub, or `None` when it does not exist (for this guild).
pub async fn update_hub(app: &App, guild_id: &str, hub_id: &str, patch: HubData) -> anyhow::Result<Option<TempVoiceHub>> {
    let mut b = UpdateBuilder::new(&TEMP_VOICE_HUB_INFO).without_returning();
    for (k, v) in patch {
        b = b.set(k, v)?;
    }
    b.filter("id", json!(hub_id))?.filter("guildId", json!(guild_id))?.build()?.execute(app.db()).await?;
    let hub: Option<TempVoiceHub> =
        SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("id", json!(hub_id))?.filter("guildId", json!(guild_id))?.build()?.fetch_optional(app.db()).await?;
    invalidate_index(app, guild_id);
    Ok(hub)
}

/// The outcome of [`update_hub_role_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleListOutcome {
    Ok,
    Exists,
    Missing,
    Limit,
    /// The hub no longer exists.
    Gone,
}

impl RoleListOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleListOutcome::Ok => "ok",
            RoleListOutcome::Exists => "exists",
            RoleListOutcome::Missing => "missing",
            RoleListOutcome::Limit => "limit",
            RoleListOutcome::Gone => "gone",
        }
    }
}

/// `updateHubRoleList(guildId, hubId, field, op, roleId)`: adds or removes one role id of `ignoredRoleIds` /
/// `moderatorRoleIds` under a row lock.
pub async fn update_hub_role_list(app: &App, guild_id: &str, hub_id: &str, field: &str, op: &str, role_id: &str) -> anyhow::Result<RoleListOutcome> {
    if field != "ignoredRoleIds" && field != "moderatorRoleIds" {
        anyhow::bail!("Invalid role list field \"{field}\"");
    }
    if op != "add" && op != "remove" {
        anyhow::bail!("Invalid role list operation \"{op}\"");
    }
    let (g, h, f, o, r) = (guild_id.to_string(), hub_id.to_string(), field.to_string(), op.to_string(), role_id.to_string());
    let out = bot_db::transaction(app.db(), TxOptions::default(), move |tx| {
        Box::pin(async move {
            sqlx::query(HUB_LOCK_SQL).bind(&h).bind(&g).execute(&mut *tx).await.map_err(DbError::from)?;
            let hub: Option<TempVoiceHub> =
                SelectBuilder::new(&TEMP_VOICE_HUB_INFO).filter("id", json!(h))?.filter("guildId", json!(g))?.build()?.fetch_optional(&mut *tx).await?;
            let Some(hub) = hub else { return Ok::<_, anyhow::Error>(RoleListOutcome::Gone) };
            let list = if f == "ignoredRoleIds" { hub.ignored_role_ids } else { hub.moderator_role_ids };
            let next: Vec<String> = if o == "add" {
                if list.contains(&r) {
                    return Ok(RoleListOutcome::Exists);
                }
                if list.len() >= MAX_HUB_ROLE_LIST {
                    return Ok(RoleListOutcome::Limit);
                }
                let mut l = list;
                l.push(r.clone());
                l
            } else {
                if !list.contains(&r) {
                    return Ok(RoleListOutcome::Missing);
                }
                list.into_iter().filter(|id| *id != r).collect()
            };
            UpdateBuilder::new(&TEMP_VOICE_HUB_INFO)
                .without_returning()
                .set(&f, json!(next))?
                .filter("id", json!(h))?
                .build()?
                .execute(&mut *tx)
                .await?;
            Ok(RoleListOutcome::Ok)
        })
    })
    .await?;
    if out == RoleListOutcome::Ok {
        invalidate_index(app, guild_id);
    }
    Ok(out)
}

/// The result of [`delete_hub`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeleteHubResult {
    pub not_found: bool,
    pub deleted: usize,
    pub failed: usize,
    pub hub_channel_failed: bool,
}

/// `deleteHub(guild, hub, { deleteHubChannel, actor })`: deletes a hub, its temporary channels (voice and text) and
/// optionally the hub channel itself. `actor_tag` is the user's tag for the audit reason.
pub async fn delete_hub(app: &App, guild_id: GuildId, hub: &TempVoiceHub, delete_hub_channel: bool, actor_tag: Option<&str>) -> anyhow::Result<DeleteHubResult> {
    // Under the hub lock, so a channel being created for this hub cannot slip in between the listing and the delete.
    let (a, hub_id, g) = (app.clone(), hub.id.clone(), guild_id.to_string());
    let (rows, count) = locked(app, &format!("hub:{}", hub.id), move || async move {
        let listed = list_hub_channels(a.db(), &hub_id).await?;
        invalidate_index(&a, &g);
        let count = delete_where(&TEMP_VOICE_HUB_INFO, [("id", json!(hub_id)), ("guildId", json!(g))])?.execute(a.db()).await?; // the cascade removes the rows
        invalidate_index(&a, &g);
        Ok::<_, anyhow::Error>((listed, count))
    })
    .await?;
    if count == 0 {
        return Ok(DeleteHubResult { not_found: true, ..Default::default() });
    }
    let reason = clamp_text(&format!("Temp voice hub deleted{}", actor_tag.map(|t| format!(" by {t}")).unwrap_or_default()), 512);
    let total = rows.len();
    let results = bot_util::async_util::map_limit(rows, 2, |row, _| {
        let (a, reason) = (app.clone(), reason.clone());
        locked(app, &format!("ch:{}", row.channel_id), move || async move {
            a.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(cleanup_key(&row.channel_id)), ..Default::default() }).await;
            forget_channel_state(&a, &row.channel_id);
            a.cache().del_one(&rename_key(&row.channel_id)).await;
            let v = delete_discord_channel(&a, guild_id, &row.channel_id, &reason, "delete voice").await; // 10003 counts as deleted
            if let Some(text) = row.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
                let _ = delete_discord_channel(&a, guild_id, text, &reason, "delete text").await;
            }
            Ok::<bool, anyhow::Error>(match v {
                Ok(()) => true,
                Err(e) => e.code == Some(10003),
            })
        })
    })
    .await;
    let deleted = results.iter().filter(|r| matches!(r, Ok(true))).count();
    let mut hub_channel_failed = false;
    if delete_hub_channel
        && let Err(e) = delete_discord_channel(app, guild_id, &hub.channel_id, &reason, "delete hub channel").await
    {
        hub_channel_failed = e.code != Some(10003);
    }
    Ok(DeleteHubResult { not_found: false, deleted, failed: total - deleted, hub_channel_failed })
}

// ── Component handlers (spec §6.12) ───────────────────────────────────────────

async fn ephemeral_text(r: &Responder, content: &str) {
    safe_reply(r, Payload::text(content).ephemeral(true)).await;
}

async fn ephemeral_embed(r: &Responder, embed: CreateEmbed) {
    safe_reply(r, Payload::embed(embed).ephemeral(true)).await;
}

async fn settings_for(app: &App, guild_id: GuildId) -> Option<Settings> {
    let g = guild_id.to_string();
    match app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(&g).await,
    }
}

async fn reply_internal(r: &Responder, label: &str, err: &anyhow::Error) {
    log_error!(format!("tempvoice {label} failed"), bot_core::error::log_arg(err));
    ephemeral_embed(r, Msg::Internal.plain()).await;
}

/// `interaction.inCachedGuild()`: the guild and member of the interaction, when the guild is cached.
fn in_cached_guild(app: &App, guild_id: Option<GuildId>, member: Option<&Member>) -> Option<(GuildId, Member)> {
    let g = guild_id?;
    let m = member?.clone();
    with_guild(app, g, |_| ())?;
    Some((g, m))
}

/// `parseCustomId(customId).params`: `(action, channelId)`.
fn action_and_channel(params: &[String]) -> (String, String) {
    (params.first().cloned().unwrap_or_default(), params.get(1).cloned().unwrap_or_default())
}

/// `handleButton(interaction)`: the `voice:{action}:{channelId}` panel buttons.
pub async fn handle_button(ctx: ComponentCtx) -> bot_core::BotResult {
    if let Err(e) = handle_button_inner(&ctx).await {
        reply_internal(&ctx.responder, "button", &e).await;
    }
    Ok(())
}

async fn handle_button_inner(ctx: &ComponentCtx) -> anyhow::Result<()> {
    let app = &ctx.app;
    let Some((guild_id, member)) = in_cached_guild(app, ctx.guild_id(), ctx.member()) else {
        ephemeral_embed(&ctx.responder, Msg::NotInGuild.plain()).await;
        return Ok(());
    };
    let (_, params) = ctx.parsed();
    let (action, channel_id) = action_and_channel(&params);
    if !BUTTON_ACTIONS.contains(&action.as_str()) || !is_snowflake(&channel_id) {
        ephemeral_text(&ctx.responder, "This button is no longer active.").await;
        return Ok(());
    }

    if IMMEDIATE_ACTIONS.contains(&action.as_str()) {
        if !safe_defer(&ctx.responder, true).await {
            return Ok(());
        }
        let settings = settings_for(app, guild_id).await;
        let r = run_voice_action(app, VoiceActionReq {
            guild_id,
            actor: member,
            channel_id,
            action,
            target_id: None,
            value: ActionValue::None,
            settings,
            source: ActionSource::Button,
        })
        .await;
        safe_reply(&ctx.responder, Payload::embed(r.embed)).await;
        return Ok(());
    }

    // limit / rename / kick / ban / transfer: showModal and reply must be the FIRST response, so the lookups are bounded.
    let g = guild_id.to_string();
    let settings = match app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => {
            let (a, g2) = (app.clone(), g.clone());
            let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&g2).await) };
            bot_util::async_util::with_timeout(load, ctx.ack_budget_ms(), "voice settings").await.ok().flatten()
        }
    };
    let budget = ctx.ack_budget_ms();
    if budget == 0 {
        ephemeral_embed(&ctx.responder, Msg::TryAgain.plain()).await;
        return Ok(());
    }
    let (a, m, ch, act, s) = (app.clone(), member.clone(), channel_id.clone(), action.clone(), settings.clone());
    let pre = async move { check_control(&a, guild_id, &m, &ch, &act, s.as_deref()).await };
    let pre = match bot_util::async_util::with_timeout(pre, budget, "voice precheck").await {
        Ok(p) => p,
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            ephemeral_embed(&ctx.responder, Msg::TryAgain.plain()).await;
            return Ok(());
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => return Err(e),
    };
    let (rh, vc) = match pre {
        Ok(v) => v,
        Err(embed) => {
            ephemeral_embed(&ctx.responder, embed).await;
            return Ok(());
        }
    };
    let sent = if action == "limit" || action == "rename" {
        let current = if action == "limit" { vc.user_limit.unwrap_or(0).to_string() } else { vc.name.clone() };
        ctx.show_modal(build_value_modal(&action, &channel_id, &current)).await
    } else {
        ctx.reply(build_member_picker(&action, &channel_id, &rh.row.banned_user_ids).to_reply()).await
    };
    if let Err(e) = sent
        && !is_interaction_gone(&e)
    {
        return Err(anyhow::anyhow!(e.to_string()));
    }
    Ok(())
}

/// `handleSelect(interaction)`: the `voice-select:{action}:{channelId}` member pickers.
pub async fn handle_select(ctx: ComponentCtx) -> bot_core::BotResult {
    if let Err(e) = handle_select_inner(&ctx).await {
        reply_internal(&ctx.responder, "select", &e).await;
    }
    Ok(())
}

async fn handle_select_inner(ctx: &ComponentCtx) -> anyhow::Result<()> {
    let app = &ctx.app;
    let Some((guild_id, member)) = in_cached_guild(app, ctx.guild_id(), ctx.member()) else {
        ephemeral_embed(&ctx.responder, Msg::NotInGuild.plain()).await;
        return Ok(());
    };
    let (_, params) = ctx.parsed();
    let (action, channel_id) = action_and_channel(&params);
    if !SELECT_ACTIONS.contains(&action.as_str()) || !is_snowflake(&channel_id) {
        ephemeral_text(&ctx.responder, "This menu is no longer active.").await;
        return Ok(());
    }
    if let Err(e) = ctx.defer_update().await {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        return Err(anyhow::anyhow!(e.to_string()));
    }
    let settings = settings_for(app, guild_id).await;
    let r = run_voice_action(app, VoiceActionReq {
        guild_id,
        actor: member,
        channel_id,
        action,
        target_id: ctx.values().first().cloned(),
        value: ActionValue::None,
        settings,
        source: ActionSource::Select,
    })
    .await;
    let edit = serenity::builder::EditInteractionResponse::new().content("").embeds(vec![r.embed]).components(Vec::new());
    if let Err(e) = ctx.edit_reply(edit).await
        && !is_interaction_gone(&e)
    {
        log_warn!(format!("tempvoice: could not update the picker: {e}"));
    }
    Ok(())
}

/// `handleModal(interaction)`: the `voice-modal:{limit|rename}:{channelId}` value modal.
pub async fn handle_modal(ctx: ModalCtx) -> bot_core::BotResult {
    if let Err(e) = handle_modal_inner(&ctx).await {
        reply_internal(&ctx.responder, "modal", &e).await;
    }
    Ok(())
}

async fn handle_modal_inner(ctx: &ModalCtx) -> anyhow::Result<()> {
    let app = &ctx.app;
    let Some((guild_id, member)) = in_cached_guild(app, ctx.guild_id(), ctx.member()) else {
        ephemeral_embed(&ctx.responder, Msg::NotInGuild.plain()).await;
        return Ok(());
    };
    let (_, params) = ctx.parsed();
    let (action, channel_id) = action_and_channel(&params);
    if !MODAL_ACTIONS.contains(&action.as_str()) || !is_snowflake(&channel_id) {
        ephemeral_text(&ctx.responder, "This form has expired. Please run the command again.").await;
        return Ok(());
    }
    let raw = ctx.value(&cid("voice-value", &[])).unwrap_or_default();
    let value = if action == "limit" {
        match parse_limit_input(&raw) {
            Some(n) => ActionValue::Int(n),
            None => {
                ephemeral_embed(&ctx.responder, Msg::LimitInvalid.plain()).await;
                return Ok(());
            }
        }
    } else {
        ActionValue::Text(raw)
    };
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let settings = settings_for(app, guild_id).await;
    let r = run_voice_action(app, VoiceActionReq {
        guild_id,
        actor: member,
        channel_id,
        action,
        target_id: None,
        value,
        settings,
        source: ActionSource::Modal,
    })
    .await;
    safe_reply(&ctx.responder, Payload::embed(r.embed)).await;
    Ok(())
}
