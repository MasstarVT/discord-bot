//! Port of `src/services/moderationService.js`: case allocation, [`execute_mod_action`] / [`execute_unban`] /
//! [`execute_unmute`], immunity, DMs and the case log, time-windowed escalation, ban markers, ban import, expiry
//! reconciliation, the scheduler job handlers, the external ban / unban handlers and the reply helpers.
//!
//! Contract (the same as the JS, for the crates that build on it: automod, captcha, anti-raid, the dashboard):
//! * [`execute_mod_action`] never fails: the Discord action runs FIRST, then the case is recorded, the target DMed
//!   (per `modDmTargets`), the case logged and the expiry scheduled. Every failure is a [`ModError`].
//! * [`execute_unban`] / [`execute_unmute`] never fail; relief actions (no immunity check).
//! * [`is_immune`] never fails.
//! * [`create_infraction`] records only (no Discord action); it fails only on a database error.
//!
//! The JS took discord.js objects; here every function takes the [`App`] and ids, reads the cached guild through
//! [`crate::guild_ops::load_guild`] and calls Discord through [`crate::guild_ops::rest`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::{CHANNEL_POST_NEEDS, Interactive, Payload, RoleCheck, check_channel_usable, is_interaction_gone};
use bot_core::permission_checker::resolve_permission_level;
use bot_core::prelude::*;
use bot_core::scheduler::JobOutcome;
use bot_db::{GuildSettings, INFRACTION_INFO, Infraction, InfractionType, InsertBuilder, JobStatus, ModVisibility, SqlValue};
use bot_util::text::clamp_text;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{ChannelId, CreateAllowedMentions, CreateEmbed, CreateEmbedFooter, CreateMessage, GuildId, Member, MessageId, Permissions, RoleId, Timestamp, User, UserId};

use crate::guild_ops::{AuditEntry, AuditKind, DiscordError, GuildView, LoadedGuild, MemberView, ModRest, RoleView, UserRef, load_guild, rest, snowflake};

// ── Constants (§4.1) ──────────────────────────────────────────────────────────

/// `MOD_ACTIONS`.
pub const MOD_ACTIONS: [&str; 7] = ["WARN", "TIMEOUT", "MUTE", "KICK", "BAN", "TEMPBAN", "SOFTBAN"];
/// `MOD_SOURCES`.
pub const MOD_SOURCES: [&str; 6] = ["command", "automod", "escalation", "captcha", "dashboard", "antiraid"];
/// `MOD_ERROR_CODES`.
pub const MOD_ERROR_CODES: [&str; 7] = ["NOT_PERMITTED", "HIERARCHY", "IMMUNE", "NOT_IN_GUILD", "ALREADY", "INVALID", "DISCORD_ERROR"];
/// `TIER_ACTIONS`.
pub const TIER_ACTIONS: [&str; 5] = ["TIMEOUT", "MUTE", "KICK", "TEMPBAN", "BAN"];
/// 28 days (Discord maximum).
pub const TIMEOUT_MAX_MS: i64 = 2_419_200_000;
/// 1 minute.
pub const TIMED_ACTION_MIN_MS: i64 = 60_000;
/// 365 days.
pub const TIMED_ACTION_MAX_MS: i64 = 31_536_000_000;
/// 7 days.
pub const DELETE_MESSAGES_MAX_S: i64 = 604_800;
/// Stored reason / proof.
pub const REASON_MAX: usize = 1000;
/// Discord audit-log reason limit.
pub const AUDIT_REASON_MAX: usize = 512;
/// Fallback `countFrom` for a legacy escalation tier without one.
pub const LEGACY_ESCALATION_SINCE: &str = "2026-10-10T12:00:00.000Z";
/// Temp bans overdue by more than this are not lifted automatically.
pub const OVERDUE_BAN_LIFT_LIMIT_MS: i64 = 30 * 86_400_000;

/// Sources where a person picked the action (a slash command, or the dashboard acting for its user).
fn by_person(source: &str) -> bool {
    source == "command" || source == "dashboard"
}

/// `ACTION_PERMISSION`: the `PermissionFlagsBits` key the bot needs for an action.
pub fn action_permission(action: &str) -> Option<&'static str> {
    match action {
        "TIMEOUT" => Some("ModerateMembers"),
        "MUTE" => Some("ManageRoles"),
        "KICK" => Some("KickMembers"),
        "BAN" | "TEMPBAN" | "SOFTBAN" => Some("BanMembers"),
        _ => None,
    }
}

/// `SOURCE_LABEL`.
pub fn source_label(source: &str) -> Option<&'static str> {
    Some(match source {
        "command" => "Command",
        "automod" => "Automod",
        "escalation" => "Escalation",
        "automation" => "Automation",
        "captcha" => "Captcha",
        "antiraid" => "Anti-Raid",
        "expiry" => "Expiry",
        "external" => "External",
        "dashboard" => "Dashboard",
        _ => return None,
    })
}

/// One `ACTION_META` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionMeta {
    pub emoji: &'static str,
    pub color: u32,
    pub label: &'static str,
    pub past: &'static str,
    pub active: bool,
}

/// `ACTION_META[type]` (TEMPBAN is display only).
pub fn action_meta(t: &str) -> Option<ActionMeta> {
    let m = |emoji, color, label, past, active| Some(ActionMeta { emoji, color, label, past, active });
    match t {
        "WARN" => m("⚠️", 0xfee75c, "Warn", "warned", true),
        "TIMEOUT" => m("⏱️", 0xff9800, "Timeout", "timed out", true),
        "MUTE" => m("🔇", 0xff9800, "Mute", "muted", true),
        "KICK" => m("👢", 0xff5722, "Kick", "kicked", false),
        "SOFTBAN" => m("🔨", 0xf44336, "Softban", "softbanned", false),
        "BAN" => m("🔨", 0xd32f2f, "Ban", "banned", true),
        "TEMPBAN" => m("⏳", 0xd32f2f, "Temporary ban", "temporarily banned", true),
        "UNBAN" => m("✅", 0x57f287, "Unban", "unbanned", false),
        "UNMUTE" => m("🔊", 0x57f287, "Unmute", "unmuted", false),
        "NOTE" => m("📝", 0x5865f2, "Note", "noted", false),
        _ => None,
    }
}

fn meta_or_note(t: &str) -> ActionMeta {
    action_meta(t).unwrap_or(ActionMeta { emoji: "📝", color: 0x5865f2, label: "Note", past: "noted", active: false })
}

/// `TIER_ACTION_LABEL`.
pub fn tier_action_label(action: &str) -> Option<&'static str> {
    Some(match action {
        "TIMEOUT" => "Timeout",
        "MUTE" => "Mute (mute role)",
        "KICK" => "Kick",
        "TEMPBAN" => "Temporary ban",
        "BAN" => "Permanent ban",
        _ => return None,
    })
}

/// `TIER_SEVERITY`.
pub fn tier_severity(action: &str) -> i64 {
    match action {
        "TIMEOUT" => 1,
        "MUTE" => 2,
        "KICK" => 3,
        "TEMPBAN" => 4,
        "BAN" => 5,
        _ => 0,
    }
}

/// `displayType(inf)`: a BAN with an expiry is shown as TEMPBAN.
pub fn display_type(inf: &Infraction) -> &'static str {
    display_type_of(inf.r#type, inf.expires_at.is_some())
}

/// [`display_type`] from the parts.
pub fn display_type_of(t: InfractionType, has_expiry: bool) -> &'static str {
    if t == InfractionType::Ban && has_expiry { "TEMPBAN" } else { t.as_str() }
}

/// The user-facing messages (`MSG`), verbatim.
pub mod msg {
    pub const BOT_SELF: &str = "I can't moderate myself.";
    pub const OWNER: &str = "You can't moderate the server owner.";
    pub const SELF: &str = "You can't moderate yourself.";
    pub const NOT_MEMBER: &str = "That user is not in this server.";
    pub const BOT_ROLE: &str = "I can't moderate that member: their highest role is above or equal to mine.";
    pub const EXEC_ROLE: &str = "You can't moderate that member: their highest role is above or equal to yours.";
    pub const ADMIN_TIMEOUT: &str = "Discord doesn't allow timing out members with the Administrator permission.";
    pub const IMMUNE_AUTO: &str = "That member is immune to moderation (server owner, administrator or immune role).";
    pub const IMMUNE_ROLE: &str = "That member has an immune role (see `/modconfig immune list`).";
    pub const IMMUNE_ADMIN: &str = "You can't moderate an administrator at or above your own permission level.";
    pub const ALREADY_BANNED: &str = "That user is already banned.";
    pub const SOFTBAN_BANNED: &str = "That user is already banned. A softban would lift their ban, so it was not run.";
    pub const NO_ME: &str = "I couldn't load my own server member. Try again in a moment.";
    pub const NO_EXECUTOR: &str = "I couldn't verify your roles. Please try again.";
    pub const BAD_PROOF: &str = "Proof must be an http:// or https:// link (max 1000 characters).";
    pub const MUTE_ROLE_GONE: &str = "The mute role no longer exists. Set a new one with `/modconfig muterole`.";
    pub const INTERNAL: &str = "Something went wrong while moderating. Please try again.";
}

/// `/^\d{17,20}$/`.
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

fn not_permitted_for(perm: &str) -> String {
    format!("I need the **{}** permission to do that.", bot_core::permissions::permission_names(&[perm]).first().cloned().unwrap_or_default())
}

// ── Error codes and results ──────────────────────────────────────────────────

/// `MOD_ERROR_CODES` as a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModCode {
    NotPermitted,
    Hierarchy,
    Immune,
    NotInGuild,
    Already,
    Invalid,
    DiscordError,
}

impl ModCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotPermitted => "NOT_PERMITTED",
            Self::Hierarchy => "HIERARCHY",
            Self::Immune => "IMMUNE",
            Self::NotInGuild => "NOT_IN_GUILD",
            Self::Already => "ALREADY",
            Self::Invalid => "INVALID",
            Self::DiscordError => "DISCORD_ERROR",
        }
    }

    /// `ERROR_TITLES[code]`.
    pub fn title(self) -> &'static str {
        match self {
            Self::NotPermitted => "Missing Permissions",
            Self::Hierarchy => "Not Allowed",
            Self::Immune => "Member Is Immune",
            Self::NotInGuild => "Not Found",
            Self::Already => "Nothing To Do",
            Self::Invalid => "Invalid Input",
            Self::DiscordError => "Action Failed",
        }
    }
}

/// `{ ok: false, code, error, applied? }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModError {
    pub code: ModCode,
    pub error: String,
    /// The Discord action was applied but the case could not be saved.
    pub applied: bool,
}

impl ModError {
    pub fn new(code: ModCode, error: impl Into<String>) -> Self {
        Self { code, error: error.into(), applied: false }
    }

    fn applied(error: impl Into<String>) -> Self {
        Self { code: ModCode::DiscordError, error: error.into(), applied: true }
    }

    fn invalid(error: impl Into<String>) -> Self {
        Self::new(ModCode::Invalid, error)
    }
}

impl std::fmt::Display for ModError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.code.as_str(), self.error)
    }
}

/// `mapDiscordError(err)`: a Discord error as `{ code, error }`.
pub fn map_discord_error(err: Option<&DiscordError>) -> ModError {
    let message = err.map(|e| e.message.as_str());
    match err.and_then(|e| e.code) {
        Some(50013) => ModError::new(ModCode::NotPermitted, "Discord refused: I'm missing a permission or my role is too low."),
        Some(50001) => ModError::new(ModCode::NotPermitted, "Discord refused: I can't access that."),
        Some(10007) => ModError::new(ModCode::NotInGuild, msg::NOT_MEMBER),
        Some(10013) => ModError::invalid("Unknown user."),
        Some(10011) => ModError::invalid("That role no longer exists."),
        Some(10026) => ModError::new(ModCode::Already, "That user is not banned."),
        Some(50035) => ModError::invalid(format!("Discord rejected the request: {}", clamp_text(message.unwrap_or(""), 200))),
        _ => ModError::new(ModCode::DiscordError, format!("Discord error: {}", clamp_text(message.unwrap_or("unknown"), 200))),
    }
}

fn map_err(e: &DiscordError) -> ModError {
    map_discord_error(Some(e))
}

/// `buildAuditReason(reason, moderator, source)`: at most 512 characters.
pub fn build_audit_reason(reason: &str, moderator: &UserRef, source: &str) -> String {
    let by = moderator.username.as_deref().or(moderator.tag.as_deref()).unwrap_or(if moderator.id.is_empty() { "unknown" } else { &moderator.id });
    let text = match source {
        "command" => format!("{reason} | by {by}"),
        "dashboard" => format!("[Dashboard] {reason} | by {by}"),
        "escalation" => reason.to_string(),
        _ => {
            let label = source_label(source).unwrap_or(source);
            if reason.starts_with(&format!("[{label}]")) || reason.starts_with(&format!("{label}:")) { reason.to_string() } else { format!("[{label}] {reason}") }
        }
    };
    clamp_text(&text, AUDIT_REASON_MAX)
}

/// The action that will really run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAction {
    pub action: &'static str,
    pub duration_ms: Option<i64>,
    pub note: Option<&'static str>,
}

/// `resolveEffectiveAction({ action, durationMs, muteRoleId })`.
pub fn resolve_effective_action(action: &str, duration_ms: Option<f64>, mute_role_id: Option<&str>) -> Result<EffectiveAction, ModError> {
    let bad = |e: &str| Err(ModError::invalid(e));
    let Some(action) = MOD_ACTIONS.iter().copied().find(|a| *a == action) else { return bad("Unknown moderation action.") };
    let mut dur = None;
    if let Some(d) = duration_ms {
        if !d.is_finite() || d <= 0.0 {
            return bad("Invalid duration.");
        }
        dur = Some(bot_util::text::js_round(d) as i64);
    }
    let mute_role = mute_role_id.filter(|r| !r.is_empty());
    let ok = |action, duration_ms, note| Ok(EffectiveAction { action, duration_ms, note });
    let act = if action == "BAN" && dur.is_some() { "TEMPBAN" } else { action };
    match act {
        "TEMPBAN" => match dur {
            Some(d) if (TIMED_ACTION_MIN_MS..=TIMED_ACTION_MAX_MS).contains(&d) => ok("TEMPBAN", Some(d), None),
            _ => bad("Temporary bans need a duration between 1 minute and 1 year."),
        },
        "TIMEOUT" => {
            let Some(d) = dur else { return bad("Timeouts need a duration.") };
            if d < TIMED_ACTION_MIN_MS {
                return bad("Durations must be at least 1 minute.");
            }
            if d > TIMEOUT_MAX_MS {
                if mute_role.is_some() {
                    if d <= TIMED_ACTION_MAX_MS {
                        return ok("MUTE", Some(d), Some("Longer than 28 days, so the mute role was used instead of a timeout."));
                    }
                    return bad("Mutes can last at most 1 year.");
                }
                return bad("Timeouts can last at most 28 days. Set a mute role with `/modconfig muterole` for longer mutes.");
            }
            ok("TIMEOUT", Some(d), None)
        }
        "MUTE" => {
            if let Some(d) = dur {
                if d < TIMED_ACTION_MIN_MS {
                    return bad("Durations must be at least 1 minute.");
                }
                if d > TIMED_ACTION_MAX_MS {
                    return bad("Mutes can last at most 1 year.");
                }
            }
            if mute_role.is_some() {
                return ok("MUTE", dur, None);
            }
            if let Some(d) = dur.filter(|d| *d <= TIMEOUT_MAX_MS) {
                return ok("TIMEOUT", Some(d), Some("No mute role is set, so a timeout was used."));
            }
            bad("No mute role is configured. Set one with `/modconfig muterole`, or use a duration of 28 days or less.")
        }
        "BAN" => ok("BAN", None, None),
        other => ok(other, None, None),
    }
}

// ── Immunity (§4.4) ───────────────────────────────────────────────────────────

/// `hasImmuneRole(member, settings)`.
pub fn has_immune_role(member: Option<&MemberView>, settings: Option<&GuildSettings>) -> bool {
    let (Some(m), Some(s)) = (member, settings) else { return false };
    s.immune_role_ids.iter().any(|id| m.has_role(id))
}

/// `isImmune(member, settings)`: the guild owner, ADMIN level (or `BOT_OWNER_ID`) and members with an immune role.
/// `owner_id` is the guild's owner.
pub fn is_immune(member: Option<&MemberView>, owner_id: &str, settings: Option<&GuildSettings>, bot_owner_id: Option<&str>) -> bool {
    let Some(m) = member else { return false };
    if m.id() == owner_id {
        return true;
    }
    if resolve_permission_level(Some(&m.access(owner_id)), settings, bot_owner_id) >= PermissionLevel::Admin {
        return true;
    }
    has_immune_role(member, settings)
}

/// [`is_immune`] for a member of a cached guild (automod, anti-raid). False when the guild is not cached.
pub fn is_immune_member(app: &App, guild_id: GuildId, member: &Member, settings: Option<&GuildSettings>) -> bool {
    let Some(g) = load_guild(app, guild_id) else { return false };
    let view = g.member_view(member);
    is_immune(Some(&view), &g.view.owner_id, settings, app.config().bot_owner_id.as_deref())
}

// ── Preflight (§4.5) ──────────────────────────────────────────────────────────

/// What [`preflight_mod_action`] checks. `action` is the EFFECTIVE action.
#[derive(Debug, Clone, Copy)]
pub struct Preflight<'a> {
    pub action: &'a str,
    pub guild: &'a GuildView,
    pub me: Option<&'a MemberView>,
    pub member: Option<&'a MemberView>,
    pub target_id: &'a str,
    pub executor: Option<&'a MemberView>,
    pub settings: Option<&'a GuildSettings>,
    pub source: &'a str,
    pub mute_role: Option<&'a RoleView>,
    /// `BOT_OWNER_ID`.
    pub bot_owner_id: Option<&'a str>,
}

/// Checks 1-3: the bot itself, the owner, the invoker.
fn check_identity(guild: &GuildView, me: Option<&MemberView>, target_id: &str, executor: Option<&MemberView>, source: &str) -> Option<ModError> {
    if me.is_some_and(|m| m.id() == target_id) {
        return Some(ModError::new(ModCode::Hierarchy, msg::BOT_SELF));
    }
    if target_id == guild.owner_id {
        return Some(ModError::new(ModCode::Hierarchy, msg::OWNER));
    }
    if by_person(source) && executor.is_some_and(|e| e.id() == target_id) {
        return Some(ModError::invalid(msg::SELF));
    }
    None
}

/// Check 8: the invoker must outrank the target (the owner always does).
fn check_executor_rank(guild: &GuildView, member: Option<&MemberView>, executor: Option<&MemberView>, source: &str) -> Option<ModError> {
    if let (true, Some(ex), Some(m)) = (by_person(source), executor, member)
        && ex.id() != guild.owner_id
        && m.highest >= ex.highest
    {
        return Some(ModError::new(ModCode::Hierarchy, msg::EXEC_ROLE));
    }
    None
}

/// `preflightModAction(...)`: `None` when the action is allowed.
pub fn preflight_mod_action(p: &Preflight<'_>) -> Option<ModError> {
    if let Some(e) = check_identity(p.guild, p.me, p.target_id, p.executor, p.source) {
        return Some(e);
    }
    if ["WARN", "TIMEOUT", "MUTE", "KICK"].contains(&p.action) && p.member.is_none() {
        return Some(ModError::new(ModCode::NotInGuild, msg::NOT_MEMBER));
    }
    if let Some(perm) = action_permission(p.action) {
        let flag = bot_core::permissions::flag(perm).unwrap_or(Permissions::empty());
        if !p.me.is_some_and(|m| m.has(flag)) {
            return Some(ModError::new(ModCode::NotPermitted, not_permitted_for(perm)));
        }
    }
    if let Some(member) = p.member {
        if !by_person(p.source) {
            if is_immune(Some(member), &p.guild.owner_id, p.settings, p.bot_owner_id) {
                return Some(ModError::new(ModCode::Immune, msg::IMMUNE_AUTO));
            }
        } else if !p.executor.is_some_and(|e| e.id() == p.guild.owner_id) {
            if has_immune_role(Some(member), p.settings) {
                return Some(ModError::new(ModCode::Immune, msg::IMMUNE_ROLE));
            }
            let target_level = resolve_permission_level(Some(&member.access(&p.guild.owner_id)), p.settings, p.bot_owner_id);
            let executor_level = match p.executor {
                Some(e) => resolve_permission_level(Some(&e.access(&p.guild.owner_id)), p.settings, p.bot_owner_id),
                None => PermissionLevel::Member,
            };
            if target_level >= PermissionLevel::Admin && target_level >= executor_level {
                return Some(ModError::new(ModCode::Immune, msg::IMMUNE_ADMIN));
            }
        }
    }
    if p.action == "TIMEOUT" && p.member.is_some_and(|m| m.has(Permissions::ADMINISTRATOR)) {
        return Some(ModError::new(ModCode::Hierarchy, msg::ADMIN_TIMEOUT));
    }
    if let Some(m) = p.member {
        let refused = match p.action {
            "TIMEOUT" => !m.moderatable,
            "KICK" => !m.kickable,
            "BAN" | "TEMPBAN" | "SOFTBAN" => !m.bannable,
            "MUTE" => !m.manageable,
            _ => false,
        };
        if refused {
            return Some(ModError::new(ModCode::Hierarchy, msg::BOT_ROLE));
        }
    }
    if p.action == "MUTE" {
        let Some(role) = p.mute_role else { return Some(ModError::invalid(msg::MUTE_ROLE_GONE)) };
        let chk = p.guild.check_assignable_role(Some(role), None, false);
        if !chk.ok {
            return Some(ModError::invalid(format!("Mute role problem: {}", chk.reason.unwrap_or_default())));
        }
    }
    check_executor_rank(p.guild, p.member, p.executor, p.source)
}

/// `assertHierarchy(guild, targetMember, executorMember)` (legacy commands): checks 1-3, the ban semantics and 8;
/// `Err` carries the user-facing message. A missing target member is fine (banning by id).
pub fn assert_hierarchy(guild: &GuildView, target: Option<&MemberView>, executor: Option<&MemberView>) -> Result<(), String> {
    let Some(t) = target else { return Ok(()) };
    if let Some(e) = check_identity(guild, guild.me.as_ref(), t.id(), executor, "command") {
        return Err(e.error);
    }
    if !t.bannable {
        return Err(msg::BOT_ROLE.to_string());
    }
    if let Some(e) = check_executor_rank(guild, Some(t), executor, "command") {
        return Err(e.error);
    }
    Ok(())
}

// ── In-process state (§3.5) ──────────────────────────────────────────────────

/// The JS module-level state, one per app: the keyed mutexes, the warn throttles and the ban markers.
pub struct ModState {
    case_mutex: bot_core::sync::KeyedMutex,
    target_mutex: bot_core::sync::KeyedMutex,
    escalation_mutex: bot_core::sync::KeyedMutex,
    mod_log_warn: bot_util::async_util::Throttle,
    mute_warn: bot_util::async_util::Throttle,
    marks: Mutex<BanMarks>,
}

impl Default for ModState {
    fn default() -> Self {
        Self {
            case_mutex: Default::default(),
            target_mutex: Default::default(),
            escalation_mutex: Default::default(),
            mod_log_warn: bot_util::async_util::create_throttle(600_000),
            mute_warn: bot_util::async_util::create_throttle(600_000),
            marks: Mutex::new(BanMarks::default()),
        }
    }
}

fn state(app: &App) -> Arc<ModState> {
    app.ext::<ModState>()
}

/// `botBanMarks`: key -> expiry, evicted oldest first past [`BOT_MARK_CAP`].
#[derive(Default)]
struct BanMarks {
    by_key: HashMap<String, (i64, u64)>,
    order: BTreeMap<u64, String>,
    seq: u64,
}

/// At most this many ban markers are kept.
pub const BOT_MARK_CAP: usize = 10_000;

// ── Small pure helpers ────────────────────────────────────────────────────────

/// `maxSnowflake(ids)`: the largest id compared as integers; `None` for an empty input.
pub fn max_snowflake<S: AsRef<str>>(ids: &[S]) -> Option<String> {
    ids.iter().filter_map(|s| s.as_ref().parse::<u128>().ok()).max().map(|v| v.to_string())
}

fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

fn date_of(ms: i64) -> DateTime<Utc> {
    bot_db::serde_js::from_epoch_ms(ms).unwrap_or_else(bot_db::now_utc)
}

fn reason_text(reason: Option<&str>) -> String {
    let r = bot_util::js::trim(reason.unwrap_or(""));
    clamp_text(if r.is_empty() { "No reason provided" } else { r }, REASON_MAX)
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// `lockCasesSql(guildId)`: the per-guild case lock, namespaced `infraction:{guildId}`.
pub const LOCK_CASES_SQL: &str = "SELECT pg_advisory_xact_lock(hashtext($1))";

/// `RAW_SQL`: every raw statement of this file, with a sample parameter (no timestamps are used).
pub const RAW_SQL: &[(&str, &str, &str)] = &[("moderation.lockCases", LOCK_CASES_SQL, "infraction:0")];

/// The advisory-lock key of a guild's cases.
pub fn case_lock_key(guild_id: &str) -> String {
    format!("infraction:{guild_id}")
}

// ── Case allocation (§4.2) ────────────────────────────────────────────────────

fn db_err(e: impl Into<bot_db::DbError>) -> anyhow::Error {
    anyhow::Error::new(e.into())
}

/// P2002 / P2028 / P2034: a unique violation, a transaction-API failure or a write conflict.
fn retryable(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<bot_db::DbError>() {
        Some(e) => e.is_unique_violation() || matches!(e, bot_db::DbError::Timeout(_)) || matches!(e.sqlstate().as_deref(), Some("40001") | Some("40P01")),
        None => false,
    }
}

/// `withDbRetry(fn)`: retries once on a unique-violation / transaction-conflict style error.
async fn with_db_retry<T, F, Fut>(f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    match f().await {
        Ok(v) => Ok(v),
        Err(e) if retryable(&e) => f().await,
        Err(e) => Err(e),
    }
}

/// `TX_OPTS`.
fn tx_opts() -> bot_db::TxOptions {
    bot_db::TxOptions::new(10_000, 15_000)
}

/// A row for [`record_infraction`].
#[derive(Debug, Clone)]
pub struct NewInfraction {
    pub guild_id: String,
    pub target_user_id: String,
    pub moderator_id: String,
    pub r#type: InfractionType,
    pub reason: Option<String>,
    pub proof_url: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// `None`: `ACTION_META[type].active`.
    pub active: Option<bool>,
    /// Only runEscalation passes it.
    pub escalation_tier_id: Option<i32>,
}

impl NewInfraction {
    pub fn new(guild_id: impl Into<String>, target_user_id: impl Into<String>, moderator_id: impl Into<String>, t: InfractionType) -> Self {
        Self {
            guild_id: guild_id.into(),
            target_user_id: target_user_id.into(),
            moderator_id: moderator_id.into(),
            r#type: t,
            reason: None,
            proof_url: None,
            expires_at: None,
            active: None,
            escalation_tier_id: None,
        }
    }
}

async fn max_case_id(tx: &mut sqlx::PgConnection, guild_id: &str) -> anyhow::Result<i64> {
    let row: (Option<i32>,) =
        sqlx::query_as(r#"SELECT MAX("caseId") FROM "Infraction" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(&mut *tx).await.map_err(db_err)?;
    Ok(i64::from(row.0.unwrap_or(0)))
}

async fn lock_cases(tx: &mut sqlx::PgConnection, guild_id: &str) -> anyhow::Result<()> {
    sqlx::query(LOCK_CASES_SQL).bind(case_lock_key(guild_id)).execute(&mut *tx).await.map_err(db_err)?;
    Ok(())
}

fn insert_row(case_id: i64, row: &NewInfraction) -> anyhow::Result<bot_db::BuiltQuery> {
    let reason = reason_text(row.reason.as_deref());
    let proof = row.proof_url.as_deref().filter(|p| !p.is_empty()).map(|p| clamp_text(p, REASON_MAX));
    let active = row.active.unwrap_or_else(|| meta_or_note(row.r#type.as_str()).active);
    let tier = row.escalation_tier_id.filter(|t| *t >= 1);
    let mut b = InsertBuilder::new(&INFRACTION_INFO)
        .value("caseId", json!(bot_db::serde_js::clamp_i32(case_id)))?
        .value("guildId", json!(row.guild_id))?
        .value("targetUserId", json!(row.target_user_id))?
        .value("moderatorId", json!(row.moderator_id))?
        .value("type", json!(row.r#type.as_str()))?
        .value("reason", json!(reason))?
        .value("proofUrl", json!(proof))?
        .value("active", json!(active))?
        .value("escalationTierId", json!(tier))?;
    b = match row.expires_at {
        Some(at) => b.typed("expiresAt", SqlValue::timestamp(at))?,
        None => b.value("expiresAt", Value::Null)?,
    };
    Ok(b.build()?)
}

/// `recordInfraction(row)`: inserts one infraction with the next per-guild `caseId` (advisory lock, no Discord call
/// inside the transaction). Fails only on a database error.
pub async fn record_infraction(app: &App, row: NewInfraction) -> anyhow::Result<Infraction> {
    app.settings().ensure_guild_settings(&row.guild_id).await?;
    let (pool, gid) = (app.db().clone(), row.guild_id.clone());
    let row = Arc::new(row);
    state(app)
        .case_mutex
        .run(&gid, move || async move {
            with_db_retry(|| {
                let (pool, row) = (pool.clone(), row.clone());
                async move {
                    bot_db::transaction(&pool, tx_opts(), move |tx| {
                        Box::pin(async move {
                            lock_cases(tx, &row.guild_id).await?;
                            let next = max_case_id(tx, &row.guild_id).await? + 1;
                            let q = insert_row(next, &row)?;
                            let inf: Infraction = q.fetch_one(&mut *tx).await?;
                            Ok::<_, anyhow::Error>(inf)
                        })
                    })
                    .await
                }
            })
            .await
        })
        .await
}

/// One row of [`record_infractions_bulk`] (ban import).
#[derive(Debug, Clone)]
pub struct BulkInfraction {
    pub target_user_id: String,
    pub moderator_id: String,
    pub r#type: InfractionType,
    pub reason: Option<String>,
    pub active: Option<bool>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// `recordInfractionsBulk(guildId, rows)`: inserts the rows in one locked transaction with the case block
/// max+1 ... max+n. Callers pass at most 500 rows. Returns the rows inserted.
pub async fn record_infractions_bulk(app: &App, guild_id: &str, rows: Vec<BulkInfraction>) -> anyhow::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    app.settings().ensure_guild_settings(guild_id).await?;
    let (pool, gid) = (app.db().clone(), guild_id.to_string());
    let rows = Arc::new(rows);
    state(app)
        .case_mutex
        .run(guild_id, move || async move {
            with_db_retry(|| {
                let (pool, rows, gid) = (pool.clone(), rows.clone(), gid.clone());
                async move {
                    bot_db::transaction(&pool, tx_opts(), move |tx| {
                        Box::pin(async move {
                            lock_cases(tx, &gid).await?;
                            let mut next = max_case_id(tx, &gid).await? + 1;
                            let mut count = 0;
                            for r in rows.iter() {
                                let row = NewInfraction {
                                    guild_id: gid.clone(),
                                    target_user_id: r.target_user_id.clone(),
                                    moderator_id: r.moderator_id.clone(),
                                    r#type: r.r#type,
                                    reason: r.reason.clone(),
                                    proof_url: None,
                                    expires_at: r.expires_at,
                                    active: r.active,
                                    escalation_tier_id: None,
                                };
                                count += insert_row(next, &row)?.execute(&mut *tx).await?;
                                next += 1;
                            }
                            Ok::<_, anyhow::Error>(count)
                        })
                    })
                    .await
                }
            })
            .await
        })
        .await
}

fn type_names(types: &[InfractionType]) -> Vec<String> {
    types.iter().map(|t| t.as_str().to_string()).collect()
}

/// `resolveInfractions(guildId, userId, types)`: marks the matching active rows inactive; returns how many.
pub async fn resolve_infractions(app: &App, guild_id: &str, user_id: &str, types: &[InfractionType]) -> anyhow::Result<u64> {
    let sql = format!(
        r#"UPDATE "Infraction" SET "active" = false, "updatedAt" = {} WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = ANY($3::"InfractionType"[]) AND "active" = true"#,
        bot_db::SQL_NOW
    );
    let r = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guild_id).bind(user_id).bind(type_names(types)).execute(app.db()).await.map_err(db_err)?;
    Ok(r.rows_affected())
}

/// Marks the rows with these ids inactive (only those still active); returns how many changed.
pub async fn close_infractions(app: &App, ids: &[i32]) -> anyhow::Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let sql = format!(r#"UPDATE "Infraction" SET "active" = false, "updatedAt" = {} WHERE "id" = ANY($1::int[]) AND "active" = true"#, bot_db::SQL_NOW);
    let r = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ids.to_vec()).execute(app.db()).await.map_err(db_err)?;
    Ok(r.rows_affected())
}

/// One infraction by its id.
pub async fn find_infraction(app: &App, id: i32) -> anyhow::Result<Option<Infraction>> {
    sqlx::query_as::<_, Infraction>(r#"SELECT * FROM "Infraction" WHERE "id" = $1"#).bind(id).fetch_optional(app.db()).await.map_err(db_err)
}

/// One case by its per-guild number (`guildId_caseId`).
pub async fn find_case(app: &App, guild_id: &str, case_id: i64) -> anyhow::Result<Option<Infraction>> {
    if !(1..=i64::from(i32::MAX)).contains(&case_id) {
        return Ok(None);
    }
    sqlx::query_as::<_, Infraction>(r#"SELECT * FROM "Infraction" WHERE "guildId" = $1 AND "caseId" = $2"#)
        .bind(guild_id)
        .bind(case_id as i32)
        .fetch_optional(app.db())
        .await
        .map_err(db_err)
}

/// The active warnings of a member (`/warn`'s count).
pub async fn count_active_warns(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<i64> {
    let r: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = 'WARN' AND "active" = true"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_one(app.db())
        .await
        .map_err(db_err)?;
    Ok(r.0)
}

// ── Ban markers (§4.9) ────────────────────────────────────────────────────────

/// `markBotBanAction(guildId, userId, kind, ttlMs = 60000)`: remembers that the bot itself is about to ban or unban
/// a user, so the ban hooks skip it.
pub fn mark_bot_ban_action(app: &App, guild_id: &str, user_id: &str, kind: &str, ttl_ms: Option<i64>) {
    let key = format!("{guild_id}:{user_id}:{kind}");
    let until = now_ms(app) + ttl_ms.unwrap_or(60_000);
    let st = state(app);
    let mut m = st.marks.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, seq)) = m.by_key.remove(&key) {
        m.order.remove(&seq);
    }
    m.seq += 1;
    let seq = m.seq;
    m.by_key.insert(key.clone(), (until, seq));
    m.order.insert(seq, key);
    while m.by_key.len() > BOT_MARK_CAP {
        let Some((&oldest, _)) = m.order.iter().next() else { break };
        if let Some(k) = m.order.remove(&oldest) {
            m.by_key.remove(&k);
        }
    }
}

/// `wasBotBanAction(guildId, userId, kind)`: non-destructive; an expired marker is dropped lazily.
pub fn was_bot_ban_action(app: &App, guild_id: &str, user_id: &str, kind: &str) -> bool {
    let key = format!("{guild_id}:{user_id}:{kind}");
    let now = now_ms(app);
    let st = state(app);
    let mut m = st.marks.lock().unwrap_or_else(|e| e.into_inner());
    let Some(&(until, seq)) = m.by_key.get(&key) else { return false };
    if until <= now {
        m.by_key.remove(&key);
        m.order.remove(&seq);
        return false;
    }
    true
}

// ── DM and mod log (§4.8) ─────────────────────────────────────────────────────

/// What [`notify_target`] tells the user.
#[derive(Debug, Clone, Default)]
pub struct Notify {
    pub guild_name: String,
    pub action: String,
    pub reason: String,
    pub duration_ms: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub case_id: Option<i32>,
    pub moderator: Option<UserRef>,
    pub show_moderator: bool,
}

/// The DM embed of [`notify_target`].
pub fn build_notify_embed(n: &Notify) -> CreateEmbed {
    let meta = action_meta(&n.action).unwrap_or_else(|| meta_or_note("NOTE"));
    let title = clamp_text(&format!("{} You were {} in {}", meta.emoji, meta.past, n.guild_name), 256);
    let mut embed = neutral_embed(Some(&title), None, None).color(meta.color);
    if n.action == "SOFTBAN" {
        embed = embed.description("Your recent messages were removed. You can rejoin with a new invite.");
    }
    let reason = if n.reason.is_empty() { "No reason provided" } else { &n.reason };
    let mut fields = vec![("Reason".to_string(), clamp_text(reason, 1024), false)];
    if let Some(d) = n.duration_ms.filter(|d| *d != 0) {
        fields.push(("Duration".into(), clamp_text(&bot_util::parse_duration::format_duration_long(d as f64), 1024), true));
    } else if n.action == "MUTE" {
        fields.push(("Duration".into(), "Until a moderator unmutes you".into(), true));
    }
    if let Some(at) = n.expires_at {
        let v = format!("{} ({})", bot_util::time::discord_timestamp(&at, "f"), bot_util::time::discord_timestamp(&at, "R"));
        fields.push(("Ends".into(), clamp_text(&v, 1024), true));
    }
    if n.show_moderator
        && let Some(u) = n.moderator.as_ref().and_then(|m| m.username.clone()).filter(|u| !u.is_empty())
    {
        fields.push(("Moderator".into(), clamp_text(&u, 1024), true));
    }
    embed = embed.fields(fields);
    if let Some(c) = n.case_id.filter(|c| *c != 0) {
        embed = embed.footer(CreateEmbedFooter::new(format!("Case #{c}")));
    }
    embed
}

/// `notifyTarget(...)`: best-effort DM to the target. Returns where it landed, or `None` (DMs closed, timeout,
/// unknown user). Never fails.
pub async fn notify_target(app: &App, user_id: &str, n: &Notify) -> Option<(ChannelId, MessageId)> {
    let uid = UserId::new(snowflake(user_id)?);
    let message = CreateMessage::new().embed(build_notify_embed(n)).allowed_mentions(CreateAllowedMentions::new());
    let r = rest(app);
    bot_util::async_util::with_timeout(r.send_dm(uid, message), 5000, "moderation DM").await.ok()
}

fn tick(s: &str) -> String {
    s.replace('`', "'")
}

/// The case embed's input.
#[derive(Debug, Clone)]
pub struct ModLogInput<'a> {
    pub infraction: &'a Infraction,
    pub target: Option<&'a UserRef>,
    pub moderator: Option<&'a UserRef>,
    pub source: &'a str,
    /// At most 3 are shown.
    pub log_fields: &'a [(String, String)],
}

/// `buildModLogEmbed(...)`: the case embed.
pub fn build_mod_log_embed(i: &ModLogInput<'_>) -> CreateEmbed {
    let inf = i.infraction;
    let meta = meta_or_note(display_type(inf));
    let title = clamp_text(&format!("{} {} | Case #{}", meta.emoji, meta.label, inf.case_id), 256);
    let mut embed = neutral_embed(Some(&title), None, None).color(meta.color);
    if let Some(avatar) = i.target.and_then(|t| t.avatar_url.clone()) {
        embed = embed.thumbnail(avatar);
    }
    let target_label = i.target.and_then(|t| t.tag.clone().or(t.username.clone())).unwrap_or_else(|| "Unknown user".into());
    let mod_label = i.moderator.and_then(|m| m.tag.clone().or(m.username.clone())).unwrap_or_else(|| inf.moderator_id.clone());
    let mut fields = vec![
        ("Target".to_string(), format!("<@{}> `{}`", inf.target_user_id, tick(&clamp_text(&target_label, 100))), true),
        ("Moderator".to_string(), format!("<@{}> `{}`", inf.moderator_id, tick(&clamp_text(&mod_label, 100))), true),
    ];
    if i.source != "command" {
        fields.push(("Source".into(), source_label(i.source).unwrap_or(i.source).to_string(), true));
    }
    let reason = inf.reason.as_deref().filter(|r| !r.is_empty()).unwrap_or("No reason provided");
    fields.push(("Reason".into(), clamp_text(reason, 1024), false));
    if let Some(at) = inf.expires_at {
        fields.push((
            "Expires".into(),
            format!("{} ({})", bot_util::time::discord_timestamp(&at, "f"), bot_util::time::discord_timestamp(&at, "R")),
            false,
        ));
    }
    if let Some(p) = inf.proof_url.as_deref().filter(|p| !p.is_empty()) {
        fields.push(("Proof".into(), clamp_text(p, 1024), false));
    }
    for (name, value) in i.log_fields.iter().take(3) {
        let n = clamp_text(name, 256);
        let v = clamp_text(value, 1024);
        fields.push((if n.is_empty() { "—".into() } else { n }, if v.is_empty() { "—".into() } else { v }, false));
    }
    embed = embed.fields(fields);
    embed = embed.footer(CreateEmbedFooter::new(format!("User ID: {}", inf.target_user_id)));
    embed.timestamp(Timestamp::from(inf.created_at))
}

/// `postModLog(guild, embed)`: posts to the case-log channel; a no-op without `modLogChannelId`. Independent of
/// `loggingEnabled`. Never fails.
pub async fn post_mod_log(app: &App, guild_id: GuildId, embed: CreateEmbed) {
    let g = guild_id.to_string();
    let Some(settings) = app.settings().get_settings_safe(&g).await else { return };
    let Some(cid) = settings.mod_log_channel_id.as_deref().and_then(snowflake).map(ChannelId::new) else { return };
    // `guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null)`.
    let u = bot_core::interaction_helpers::check_channel_usable_fetch(app, guild_id, Some(cid), &CHANNEL_POST_NEEDS).await;
    if !u.ok {
        if state(app).mod_log_warn.allow(&g) {
            let why = u.reason.clone().unwrap_or_else(|| u.missing.join(", "));
            bot_util::log_warn!(format!("Mod log channel unusable in {g}: {why}"));
        }
        return;
    }
    let message = CreateMessage::new().embed(embed).allowed_mentions(CreateAllowedMentions::new());
    if let Err(e) = rest(app).send_message(cid, message).await {
        bot_util::log_warn!(format!("Mod log post failed in {g}: {}", e.message));
    }
}

/// `sendModLog(guild, infraction, target, moderator, client, { source, logFields })`: builds and posts the case
/// embed. Never fails.
pub async fn send_mod_log(app: &App, guild_id: GuildId, infraction: &Infraction, target: &UserRef, moderator: &UserRef, source: &str, log_fields: &[(String, String)]) {
    let embed = build_mod_log_embed(&ModLogInput { infraction, target: Some(target), moderator: Some(moderator), source, log_fields });
    post_mod_log(app, guild_id, embed).await;
}

/// The bot's own user (`client.user`) as a [`UserRef`]; `None` offline.
pub fn bot_user(app: &App) -> Option<UserRef> {
    let d = app.discord()?;
    let me = d.cache.current_user().clone();
    Some(UserRef::of(&me))
}

/// The bot's user id as a string (`client.user.id`), or `""` offline.
pub fn bot_user_id(app: &App) -> String {
    app.discord().map(|d| d.bot_user_id().to_string()).unwrap_or_default()
}

// ── createInfraction (§4.3) ───────────────────────────────────────────────────

/// The options of [`create_infraction`].
#[derive(Debug, Clone, Default)]
pub struct CreateOpts {
    pub reason: Option<String>,
    pub proof_url: Option<String>,
    pub duration_ms: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub skip_escalation: bool,
    pub silent: bool,
    /// Default `command`.
    pub source: Option<String>,
    pub log_fields: Vec<(String, String)>,
}

const DM_TYPES: [&str; 6] = ["WARN", "TIMEOUT", "MUTE", "KICK", "SOFTBAN", "BAN"];

/// `createInfraction(guild, target, moderator, type, opts)`: records only (no Discord action), then DMs and mod-logs
/// best-effort, then escalation for WARN. Used by the expiry jobs, the external ban hooks, `/case clear-history`'s
/// NOTE and the legacy automod path. Fails only on a database error.
pub async fn create_infraction(app: &App, guild_id: GuildId, target: &UserRef, moderator: &UserRef, t: InfractionType, opts: CreateOpts) -> anyhow::Result<Infraction> {
    let g = guild_id.to_string();
    let now = now_ms(app);
    let expires_at = opts.expires_at.or_else(|| opts.duration_ms.filter(|d| *d != 0).map(|d| date_of(now + d)));
    let infraction = record_infraction(
        app,
        NewInfraction {
            reason: opts.reason.clone(),
            proof_url: opts.proof_url.clone(),
            expires_at,
            ..NewInfraction::new(g.clone(), target.id.clone(), moderator.id.clone(), t)
        },
    )
    .await?;

    let source = opts.source.clone().unwrap_or_else(|| "command".into());
    if !opts.silent && DM_TYPES.contains(&t.as_str()) {
        let dm_on = app.settings().get_settings_safe(&g).await.map(|s| s.mod_dm_targets);
        if dm_on != Some(false) {
            let guild_name = load_guild(app, guild_id).map(|l| l.view.name).unwrap_or_default();
            let remaining = expires_at.map(|e| (e.timestamp_millis() - now_ms(app)).max(0)).filter(|d| *d != 0);
            let n = Notify {
                guild_name,
                action: display_type(&infraction).to_string(),
                reason: infraction.reason.clone().unwrap_or_default(),
                duration_ms: remaining,
                expires_at,
                case_id: Some(infraction.case_id),
                moderator: Some(moderator.clone()),
                show_moderator: false,
            };
            notify_target(app, &target.id, &n).await;
        }
    }
    let resolved = if target.has_name() {
        target.clone()
    } else {
        match snowflake(&target.id) {
            Some(id) => rest(app).get_user(UserId::new(id)).await.map(|u| UserRef::of(&u)).unwrap_or_else(|_| UserRef::id(target.id.clone())),
            None => UserRef::id(target.id.clone()),
        }
    };
    send_mod_log(app, guild_id, &infraction, &resolved, moderator, &source, &opts.log_fields).await;
    if t == InfractionType::Warn && !opts.skip_escalation {
        run_escalation(app, guild_id, &target.id).await;
    }
    Ok(infraction)
}

// ── Escalation (§4.10) ────────────────────────────────────────────────────────

/// One v2 escalation tier (`GuildSettings.infractionEscalation[]`), in the JS key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tier {
    pub id: i64,
    pub warn_count: i64,
    pub within_seconds: Option<i64>,
    pub action: String,
    pub duration_seconds: Option<i64>,
    pub count_from: Option<String>,
}

/// JS `Number(v)` for a JSON value (`undefined` -> NaN).
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = bot_util::js::trim(s);
            if t.is_empty() {
                0.0
            } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN)
            } else if t == "Infinity" || t == "+Infinity" {
                f64::INFINITY
            } else if t == "-Infinity" {
                f64::NEG_INFINITY
            } else if t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
                t.parse::<f64>().unwrap_or(f64::NAN)
            } else {
                f64::NAN
            }
        }
        Some(Value::Array(a)) => match a.len() {
            0 => 0.0,
            1 => js_number(a.first()),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `Number.isInteger(v)` for a JSON value.
fn js_integer(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_f64()?;
    (n.is_finite() && n.fract() == 0.0).then_some(n as i64)
}

/// `toPosInt(v)`.
fn to_pos_int(v: Option<&Value>) -> Option<i64> {
    let n = js_number(v);
    (n.is_finite() && n > 0.0).then(|| n.floor() as i64)
}

/// `String(v ?? '')`.
fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `normalizeEscalationTiers(raw)`: v2 tiers, sorted, deduped, capped, ids stable, `countFrom` kept. Never fails.
pub fn normalize_escalation_tiers(raw: Option<&Value>) -> Vec<Tier> {
    let Some(Value::Array(list)) = raw else { return Vec::new() };
    struct Entry {
        id: Option<i64>,
        tier: Tier,
    }
    let mut entries: Vec<Entry> = Vec::new();
    for e in list {
        let Value::Object(o) = e else { continue };
        let legacy = !o.contains_key("withinSeconds") && !o.contains_key("durationSeconds");
        let Some(warn_count) = js_integer(o.get("warnCount")).filter(|w| (1..=100).contains(w)) else { continue };
        let mut action = js_string(o.get("action")).to_uppercase();
        if !TIER_ACTIONS.contains(&action.as_str()) {
            continue;
        }
        let mut dur = to_pos_int(if legacy { o.get("duration") } else { o.get("durationSeconds") });
        let mut within = None;
        if !legacy {
            let w = js_number(o.get("withinSeconds"));
            if w.is_finite() && w.fract() == 0.0 && (60.0..=31_536_000.0).contains(&w) {
                within = Some(w as i64);
            }
        }
        if legacy && action == "BAN" && dur.is_some() {
            action = "TEMPBAN".into();
        }
        match action.as_str() {
            "TIMEOUT" => {
                let Some(d) = dur else { continue };
                dur = Some(d.clamp(60, 2_419_200));
            }
            "TEMPBAN" => {
                let Some(d) = dur else { continue };
                dur = Some(d.clamp(60, 31_536_000));
            }
            "MUTE" => dur = dur.map(|d| d.clamp(60, 31_536_000)),
            _ => dur = None,
        }
        let mut count_from = match o.get("countFrom") {
            Some(Value::String(s)) if bot_db::parse_js_date(s).is_some() => Some(s.clone()),
            _ => None,
        };
        if legacy && count_from.is_none() {
            count_from = Some(LEGACY_ESCALATION_SINCE.into());
        }
        let id = js_integer(o.get("id")).filter(|i| *i >= 1);
        entries.push(Entry { id, tier: Tier { id: 0, warn_count, within_seconds: within, action, duration_seconds: dur, count_from } });
    }

    // Ids: missing ones and later duplicates get max+1, +2, ... in the original order.
    let mut taken: HashSet<i64> = HashSet::new();
    let mut next = entries.iter().filter_map(|e| e.id).max().unwrap_or(0).max(0);
    for e in entries.iter_mut() {
        if let Some(id) = e.id
            && !taken.contains(&id)
        {
            taken.insert(id);
            e.tier.id = id;
            continue;
        }
        loop {
            next += 1;
            if !taken.contains(&next) {
                break;
            }
        }
        e.tier.id = next;
        taken.insert(next);
    }

    // Dedupe on (warnCount, withinSeconds): the last occurrence wins.
    let mut by_key: Vec<((i64, Option<i64>), Tier)> = Vec::new();
    for e in entries {
        let key = (e.tier.warn_count, e.tier.within_seconds);
        match by_key.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = e.tier,
            None => by_key.push((key, e.tier)),
        }
    }
    let mut list: Vec<Tier> = by_key.into_iter().map(|(_, t)| t).collect();
    list.sort_by(|a, b| {
        a.warn_count
            .cmp(&b.warn_count)
            .then(a.within_seconds.unwrap_or(i64::MAX).cmp(&b.within_seconds.unwrap_or(i64::MAX)))
            .then(a.id.cmp(&b.id))
    });
    list.truncate(bot_config::limits::ESCALATION_TIERS);
    list
}

/// `describeTier(t)`: "T3: 5 warnings within 1 week → Temporary ban for 3 days" (at most 100 characters).
pub fn describe_tier(t: &Tier) -> String {
    let mut s = format!("T{}: {} warning{}", t.id, t.warn_count, if t.warn_count == 1 { "" } else { "s" });
    if let Some(w) = t.within_seconds.filter(|w| *w != 0) {
        s.push_str(&format!(" within {}", bot_util::parse_duration::format_duration_long((w * 1000) as f64)));
    }
    s.push_str(&format!(" → {}", tier_action_label(&t.action).unwrap_or(&t.action)));
    if let Some(d) = t.duration_seconds.filter(|d| *d != 0) {
        s.push_str(&format!(" for {}", bot_util::parse_duration::format_duration_long((d * 1000) as f64)));
    } else if t.action == "MUTE" {
        s.push_str(" until unmuted");
    }
    clamp_text(&s, 100)
}

/// Start of a tier's counting window, or `None` when no warn is eligible for a no-limit tier.
fn tier_window_start(tier: &Tier, sorted: &[i64], now: i64) -> Option<i64> {
    let floor = tier.count_from.as_deref().and_then(bot_db::parse_js_date).map(|d| d.timestamp_millis()).unwrap_or(i64::MIN);
    if let Some(w) = tier.within_seconds.filter(|w| *w != 0) {
        return Some((now - w * 1000).max(floor));
    }
    sorted.iter().copied().find(|t| *t >= floor)
}

/// A prior escalation case: which tier and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorEscalation {
    pub tier_id: i64,
    pub at: i64,
}

/// `selectEscalationTier(tiers, warnTimes, prior, now)`: the tier to fire and how many warns it counted, or `None`.
pub fn select_escalation_tier(tiers: &[Tier], warn_times: &[i64], prior: &[PriorEscalation], now: i64) -> Option<(Tier, usize)> {
    let mut sorted = warn_times.to_vec();
    sorted.sort_unstable();
    if sorted.is_empty() {
        return None;
    }
    let mut candidates: Vec<(Tier, usize)> = Vec::new();
    for tier in tiers {
        let Some(start) = tier_window_start(tier, &sorted, now) else { continue };
        let count = sorted.iter().filter(|t| **t >= start).count();
        if (count as i64) < tier.warn_count {
            continue;
        }
        if prior.iter().any(|p| p.tier_id == tier.id && p.at >= start) {
            continue;
        }
        candidates.push((tier.clone(), count));
    }
    let dur = |t: &Tier| -> f64 {
        match t.duration_seconds {
            Some(d) => d as f64,
            None if t.action == "MUTE" => f64::INFINITY,
            None => 0.0,
        }
    };
    candidates.sort_by(|(a, _), (b, _)| {
        tier_severity(&b.action)
            .cmp(&tier_severity(&a.action))
            .then(dur(b).partial_cmp(&dur(a)).unwrap_or(std::cmp::Ordering::Equal))
            .then(b.warn_count.cmp(&a.warn_count))
            .then(a.id.cmp(&b.id))
    });
    candidates.into_iter().next()
}

/// `runEscalation`'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationOutcome {
    pub tier_id: i64,
    pub action: String,
    pub ok: bool,
    pub case_id: Option<i32>,
    pub error: Option<String>,
}

/// The escalation reason: `[Escalation T1] Reached 3 active warnings within 1 day`.
pub fn escalation_reason(tier: &Tier, count: usize) -> String {
    let within = match tier.within_seconds.filter(|w| *w != 0) {
        Some(w) => format!(" within {}", bot_util::parse_duration::format_duration_long((w * 1000) as f64)),
        None => String::new(),
    };
    clamp_text(&format!("[Escalation T{}] Reached {count} active warning{}{within}", tier.id, if count == 1 { "" } else { "s" }), AUDIT_REASON_MAX)
}

/// `runEscalation(guild, userId)`: time-windowed escalation after a warn. Nothing is recorded on failure, so the next
/// warn retries. Never fails.
pub fn run_escalation(app: &App, guild_id: GuildId, user_id: &str) -> BoxFuture<Option<EscalationOutcome>> {
    let (app, user_id) = (app.clone(), user_id.to_string());
    Box::pin(async move {
        let g = guild_id.to_string();
        let settings = app.settings().get_settings_safe(&g).await;
        if !is_module_enabled(settings.as_deref(), Some(ModuleKey::Moderation)) {
            return None;
        }
        let tiers = normalize_escalation_tiers(settings.as_ref().and_then(|s| s.infraction_escalation.as_ref()));
        if tiers.is_empty() {
            return None;
        }
        let key = format!("{g}:{user_id}");
        let a = app.clone();
        let r = state(&app).escalation_mutex.run(&key, move || async move { escalate_locked(&a, guild_id, &user_id, &tiers).await }).await;
        match r {
            Ok(o) => o,
            Err(e) => {
                bot_util::log_error!(format!("runEscalation failed in {guild_id}"), bot_core::error::log_arg(&e));
                None
            }
        }
    })
}

async fn escalate_locked(app: &App, guild_id: GuildId, user_id: &str, tiers: &[Tier]) -> anyhow::Result<Option<EscalationOutcome>> {
    let g = guild_id.to_string();
    let warns: Vec<(chrono::NaiveDateTime,)> = sqlx::query_as(
        r#"SELECT "createdAt" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = 'WARN' AND "active" = true ORDER BY "createdAt" DESC LIMIT 500"#,
    )
    .bind(&g)
    .bind(user_id)
    .fetch_all(app.db())
    .await
    .map_err(db_err)?;
    if warns.is_empty() {
        return Ok(None);
    }
    let warn_times: Vec<i64> = warns.iter().map(|w| w.0.and_utc().timestamp_millis()).collect();
    let mut sorted = warn_times.clone();
    sorted.sort_unstable();
    let now = now_ms(app);
    let Some(min_start) = tiers.iter().filter_map(|t| tier_window_start(t, &sorted, now)).min() else { return Ok(None) };
    let rows: Vec<(Option<i32>, chrono::NaiveDateTime)> = sqlx::query_as(
        r#"SELECT "escalationTierId", "createdAt" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "escalationTierId" IS NOT NULL AND "createdAt" >= $3"#,
    )
    .bind(&g)
    .bind(user_id)
    .bind(date_of(min_start.max(0)).naive_utc())
    .fetch_all(app.db())
    .await
    .map_err(db_err)?;
    let prior: Vec<PriorEscalation> =
        rows.iter().filter_map(|(t, at)| t.map(|t| PriorEscalation { tier_id: i64::from(t), at: at.and_utc().timestamp_millis() })).collect();
    let Some((tier, count)) = select_escalation_tier(tiers, &warn_times, &prior, now) else { return Ok(None) };
    let reason = escalation_reason(&tier, count);
    let moderator = bot_user(app).unwrap_or_else(|| UserRef::id(bot_user_id(app)));
    let input = ModActionInput {
        duration_ms: tier.duration_seconds.filter(|d| *d != 0).map(|d| (d * 1000) as f64),
        reason: Some(reason),
        source: "escalation".into(),
        skip_escalation: true,
        escalation_tier_id: i32::try_from(tier.id).ok(),
        ..ModActionInput::new(g.clone(), user_id, moderator, &tier.action)
    };
    let res = execute_mod_action(app, input).await;
    match &res {
        Ok(_) => bot_util::log_info!(format!("Escalation T{} applied {} to {user_id} in {g}", tier.id, tier.action)),
        Err(e) => bot_util::log_info!(format!("Escalation T{} ({}) not applied for {user_id} in {g}: {} {}", tier.id, tier.action, e.code.as_str(), e.error)),
    }
    Ok(Some(EscalationOutcome {
        tier_id: tier.id,
        action: tier.action.clone(),
        ok: res.is_ok(),
        case_id: res.as_ref().ok().map(|s| s.infraction.case_id),
        error: res.err().map(|e| e.error),
    }))
}

// ── executeModAction (§4.6) ───────────────────────────────────────────────────

/// The input of [`execute_mod_action`].
#[derive(Debug, Clone)]
pub struct ModActionInput {
    pub guild_id: String,
    pub target_id: String,
    /// The target's member object when the caller has it (`target` was a GuildMember of this guild).
    pub target_member: Option<Member>,
    /// The target's user object when the caller has it.
    pub target_user: Option<User>,
    /// `client.user` for automated actions.
    pub moderator: UserRef,
    /// `WARN | TIMEOUT | MUTE | KICK | BAN | TEMPBAN | SOFTBAN`.
    pub action: String,
    pub reason: Option<String>,
    pub duration_ms: Option<f64>,
    /// 0..=604800.
    pub delete_message_seconds: Option<i64>,
    pub proof_url: Option<String>,
    /// `command | automod | escalation | captcha | dashboard | antiraid`.
    pub source: String,
    /// No DMs.
    pub silent: bool,
    pub skip_escalation: bool,
    /// runEscalation only (ignored unless `source` is `escalation` and the moderator is the bot).
    pub escalation_tier_id: Option<i32>,
    /// At most 3, appended to the case embed.
    pub log_fields: Vec<(String, String)>,
}

impl ModActionInput {
    /// The required fields; `source` is `command`.
    pub fn new(guild_id: impl Into<String>, target_id: impl Into<String>, moderator: UserRef, action: &str) -> Self {
        Self {
            guild_id: guild_id.into(),
            target_id: target_id.into(),
            target_member: None,
            target_user: None,
            moderator,
            action: action.to_string(),
            reason: None,
            duration_ms: None,
            delete_message_seconds: None,
            proof_url: None,
            source: "command".into(),
            silent: false,
            skip_escalation: false,
            escalation_tier_id: None,
            log_fields: Vec::new(),
        }
    }
}

/// Whether the target was DMed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmStatus {
    Sent,
    Failed,
    Skipped,
}

impl DmStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// `{ ok: true, infraction, action, dm, warnings, escalation }`.
#[derive(Debug, Clone)]
pub struct ModSuccess {
    pub infraction: Infraction,
    /// The effective action (`TEMPBAN`, or `MUTE` for a long timeout).
    pub action: String,
    pub dm: DmStatus,
    pub warnings: Vec<String>,
    pub escalation: Option<EscalationOutcome>,
}

/// `guild.members.me ?? fetchMe()`.
async fn get_me(guild: &LoadedGuild, r: &dyn ModRest) -> Option<MemberView> {
    if let Some(me) = guild.view.me.clone() {
        return Some(me);
    }
    r.get_member(guild.id, guild.bot_id).await.ok().map(|m| guild.member_view(&m))
}

/// `executeModAction(input)`: the Discord action FIRST, then the record, the DM (per `modDmTargets`), the case log and
/// the expiry job. Never fails; every failure is a [`ModError`]. Does not refuse when the module is off.
pub fn execute_mod_action(app: &App, input: ModActionInput) -> BoxFuture<Result<ModSuccess, ModError>> {
    let app = app.clone();
    Box::pin(async move {
        let (guild_id, target_id, source, skip) = (input.guild_id.clone(), input.target_id.clone(), input.source.clone(), input.skip_escalation);
        let mut result = match execute_inner(&app, input).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("executeModAction failed".to_string(), bot_core::error::log_arg(&e));
                return Err(ModError::new(ModCode::DiscordError, msg::INTERNAL));
            }
        };
        // Step 4: escalation, outside the target mutex.
        if let Ok(ok) = &mut result
            && ok.infraction.r#type == InfractionType::Warn
            && !skip
            && source != "escalation"
            && let Some(g) = snowflake(&guild_id)
        {
            ok.escalation = run_escalation(&app, GuildId::new(g), &target_id).await;
        }
        result
    })
}

async fn execute_inner(app: &App, input: ModActionInput) -> anyhow::Result<Result<ModSuccess, ModError>> {
    let invalid = |e: &str| Ok(Err(ModError::invalid(e)));
    // Step 0: validate the input.
    let Some(gid) = snowflake(&input.guild_id).map(GuildId::new) else { return invalid("Missing guild.") };
    if !is_snowflake(&input.target_id) {
        return invalid("Invalid target user.");
    }
    if input.moderator.id.is_empty() {
        return invalid("Missing moderator.");
    }
    if !MOD_SOURCES.contains(&input.source.as_str()) {
        return invalid("Unknown moderation source.");
    }
    if let Some(p) = input.proof_url.as_deref().filter(|p| !p.is_empty())
        && (!bot_core::custom_embed::is_http_url(p) || bot_util::js::utf16_len(p) > REASON_MAX)
    {
        return invalid(msg::BAD_PROOF);
    }
    if let Some(d) = input.delete_message_seconds
        && !(0..=DELETE_MESSAGES_MAX_S).contains(&d)
    {
        return invalid("Message deletion must be between 0 and 7 days.");
    }
    let reason = reason_text(input.reason.as_deref());
    let bot_id = bot_user_id(app);
    let tier_id = if input.source == "escalation" && input.moderator.id == bot_id { input.escalation_tier_id } else { None };

    // Step 1: settings (None means the schema defaults).
    let g = gid.to_string();
    let settings = app.settings().get_settings_safe(&g).await;

    // Step 2: the effective action.
    let eff = match resolve_effective_action(&input.action, input.duration_ms, settings.as_ref().and_then(|s| s.mute_role_id.as_deref())) {
        Ok(e) => e,
        Err(e) => return Ok(Err(e)),
    };

    // Step 3: perform, serialised per target.
    let key = format!("{g}:{}", input.target_id);
    let a = app.clone();
    let r = state(app)
        .target_mutex
        .run(&key, move || async move { perform(&a, gid, input, settings, eff, reason, tier_id).await })
        .await;
    Ok(r)
}

#[allow(clippy::too_many_arguments)]
async fn perform(
    app: &App,
    gid: GuildId,
    input: ModActionInput,
    settings: Option<Settings>,
    eff: EffectiveAction,
    reason: String,
    tier_id: Option<i32>,
) -> Result<ModSuccess, ModError> {
    let g = gid.to_string();
    let r = rest(app);
    let target_id = input.target_id.clone();
    let tuid = UserId::new(snowflake(&target_id).unwrap_or(1));
    // a. the bot member
    let Some(guild) = load_guild(app, gid) else { return Err(ModError::new(ModCode::DiscordError, msg::NO_ME)) };
    let Some(me) = get_me(&guild, r.as_ref()).await else { return Err(ModError::new(ModCode::DiscordError, msg::NO_ME)) };
    // b. the target member, d. the executor (a failed lookup is an error, never "not in the server")
    let member = match input.target_member.as_ref().filter(|m| m.guild_id == gid) {
        Some(m) => Some(guild.member_view(m)),
        None => guild.member_or_none(r.as_ref(), tuid).await.map_err(|e| map_err(&e))?,
    };
    let mut executor = None;
    if by_person(&input.source) && input.moderator.id != guild.view.bot_id {
        let Some(mid) = snowflake(&input.moderator.id) else { return Err(ModError::invalid(msg::NO_EXECUTOR)) };
        match guild.member_or_none(r.as_ref(), UserId::new(mid)).await {
            Ok(Some(e)) => executor = Some(e),
            Ok(None) => return Err(ModError::invalid(msg::NO_EXECUTOR)),
            Err(e) => return Err(map_err(&e)),
        }
    }
    // c. the target user
    let user: Option<UserRef> = match (&member, &input.target_user) {
        (Some(m), _) => Some(m.user.clone()),
        (None, Some(u)) => Some(UserRef::of(u)),
        (None, None) => r.get_user(tuid).await.ok().map(|u| UserRef::of(&u)),
    };
    // e. the mute role
    let mute_role_id = settings.as_ref().and_then(|s| s.mute_role_id.clone());
    let mute_role = if eff.action == "MUTE" { mute_role_id.as_deref().and_then(|id| guild.view.role(id)).cloned() } else { None };
    // f. preflight
    let bot_owner = app.config().bot_owner_id.clone();
    let pre = Preflight {
        action: eff.action,
        guild: &guild.view,
        me: Some(&me),
        member: member.as_ref(),
        target_id: &target_id,
        executor: executor.as_ref(),
        settings: settings.as_deref(),
        source: &input.source,
        mute_role: mute_role.as_ref(),
        bot_owner_id: bot_owner.as_deref(),
    };
    if let Some(e) = preflight_mod_action(&pre) {
        return Err(e);
    }

    let banning = matches!(eff.action, "BAN" | "TEMPBAN" | "SOFTBAN");
    // g. the ban state
    if banning {
        match r.get_ban(gid, tuid).await {
            Ok(Some(_)) => {
                return Err(ModError::new(ModCode::Already, if eff.action == "SOFTBAN" { msg::SOFTBAN_BANNED } else { msg::ALREADY_BANNED }));
            }
            Ok(None) => {}
            Err(e) if matches!(e.code, Some(50013) | Some(50001)) => {
                return Err(ModError::new(ModCode::NotPermitted, "I need the **Ban Members** permission to check bans."));
            }
            Err(e) => return Err(ModError::new(ModCode::DiscordError, format!("Couldn't check the ban list: {}", clamp_text(&e.message, 200)))),
        }
    }

    // h. delivery settings
    let visibility = settings.as_ref().map(|s| s.mod_visibility).unwrap_or(ModVisibility::Public);
    let show_moderator = by_person(&input.source) && visibility == ModVisibility::Public;
    let dm_enabled = !input.silent && settings.as_ref().map(|s| s.mod_dm_targets).unwrap_or(true);
    let now = now_ms(app);
    let expires_at = eff.duration_ms.filter(|d| *d != 0).map(|d| date_of(now + d));
    let mut warnings: Vec<String> = eff.note.map(|n| vec![n.to_string()]).unwrap_or_default();
    let mut dm = DmStatus::Skipped;
    let notify = |case_id: Option<i32>| Notify {
        guild_name: guild.view.name.clone(),
        action: eff.action.to_string(),
        reason: reason.clone(),
        duration_ms: eff.duration_ms,
        expires_at,
        case_id,
        moderator: Some(input.moderator.clone()),
        show_moderator,
    };

    // i. DM before the action (the user usually cannot receive it once they share no guild with the bot)
    let mut dm_message = None;
    if matches!(eff.action, "KICK" | "BAN" | "TEMPBAN" | "SOFTBAN") && dm_enabled {
        dm_message = match &user {
            Some(u) => notify_target(app, &u.id, &notify(None)).await,
            None => None,
        };
        dm = if dm_message.is_some() { DmStatus::Sent } else { DmStatus::Failed };
    }

    // j. the audit reason
    let audit = build_audit_reason(&reason, &input.moderator, &input.source);

    // k. markers
    if banning {
        mark_bot_ban_action(app, &g, &target_id, "ban", None);
    }
    if eff.action == "SOFTBAN" {
        mark_bot_ban_action(app, &g, &target_id, "unban", None);
    }

    // l. the Discord action
    let mut softban_stuck = false;
    let delete_secs = |default: i64| u32::try_from(input.delete_message_seconds.unwrap_or(default)).unwrap_or(0);
    let done = match eff.action {
        "TIMEOUT" => r.timeout(gid, tuid, expires_at, audit.clone()).await,
        "MUTE" => match &mute_role {
            Some(role) => r.add_role(gid, tuid, RoleId::new(snowflake(&role.id).unwrap_or(1)), audit.clone()).await,
            None => Ok(()),
        },
        "KICK" => r.kick(gid, tuid, audit.clone()).await,
        "BAN" | "TEMPBAN" => r.ban(gid, tuid, delete_secs(0), audit.clone()).await,
        "SOFTBAN" => match r.ban(gid, tuid, delete_secs(DELETE_MESSAGES_MAX_S), audit.clone()).await {
            Err(e) => Err(e),
            Ok(()) => {
                let unban_reason = clamp_text("Softban: immediate unban", 512);
                if r.unban(gid, tuid, unban_reason.clone()).await.is_err() {
                    bot_util::async_util::sleep(1000).await;
                    if let Err(e2) = r.unban(gid, tuid, unban_reason).await {
                        softban_stuck = true;
                        warnings.push("The ban was applied, but the automatic unban failed, so the user is still banned. Use `/unban` to lift it.".into());
                        bot_util::log_warn!(format!("Softban unban failed for {target_id} in {g}: {}", e2.message));
                    }
                }
                Ok(())
            }
        },
        _ => Ok(()),
    };
    if let Err(e) = done {
        if let Some((c, m)) = dm_message {
            let _ = r.delete_message(c, m).await; // take back the false DM
        }
        return Err(map_err(&e));
    }

    // m. record the case
    let t = if eff.action == "TEMPBAN" || (eff.action == "SOFTBAN" && softban_stuck) {
        InfractionType::Ban
    } else {
        InfractionType::parse(eff.action).unwrap_or(InfractionType::Warn)
    };
    let rec_reason = if softban_stuck { clamp_text(&format!("[Softban: unban failed] {reason}"), REASON_MAX) } else { reason.clone() };
    let recorded = record_infraction(
        app,
        NewInfraction {
            reason: Some(rec_reason),
            proof_url: input.proof_url.clone(),
            expires_at: if t == InfractionType::Ban && eff.action != "TEMPBAN" { None } else { expires_at },
            escalation_tier_id: tier_id,
            ..NewInfraction::new(g.clone(), target_id.clone(), input.moderator.id.clone(), t)
        },
    )
    .await;
    let infraction = match recorded {
        Ok(i) => i,
        Err(e) => {
            bot_util::log_error!(format!("Could not record {} of {target_id} in {g}", eff.action), bot_core::error::log_arg(&e));
            if eff.action == "TEMPBAN"
                && let Some(at) = expires_at
            {
                let job = ScheduleJob::new(g.clone(), JobType::ModUnban, at)
                    .payload(json!({ "userId": target_id, "infractionId": null }))
                    .dedupe_key(unban_key(&g, &target_id))
                    .max_attempts(20)
                    .on_conflict(OnConflict::Replace);
                if let Err(e2) = app.scheduler().schedule_job(job).await {
                    bot_util::log_error!("Could not schedule the temp-ban expiry".to_string(), bot_core::error::log_arg(&e2));
                }
            }
            let label = meta_or_note(eff.action).label.to_lowercase();
            return Err(ModError::applied(format!("The {label} was applied, but saving the case failed. Please record it manually.")));
        }
    };

    // n. supersede older TIMEOUT / MUTE rows (best-effort)
    if (t == InfractionType::Timeout || t == InfractionType::Mute)
        && let Err(e) = supersede(app, &g, &target_id, t, infraction.id).await
    {
        bot_util::log_warn!(format!("Could not supersede older {} rows for {target_id} in {g}: {e}", t.as_str()));
    }

    // o. schedule the expiry (best-effort)
    let scheduled: anyhow::Result<()> = async {
        if eff.action == "TEMPBAN" {
            if let Some(at) = expires_at {
                app.scheduler().schedule_job(unban_job(&g, &target_id, Some(infraction.id), at)).await?;
            }
        } else if t == InfractionType::Ban {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(unban_key(&g, &target_id)), ..Default::default() }).await;
        } else if (t == InfractionType::Timeout || t == InfractionType::Mute)
            && let Some(at) = expires_at
        {
            let role = if t == InfractionType::Mute { mute_role.as_ref().map(|r| r.id.clone()) } else { None };
            app.scheduler().schedule_job(expire_job(&g, infraction.id, role.as_deref(), at)).await?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = scheduled {
        bot_util::log_error!(format!("Could not schedule the expiry of case #{} in {g}", infraction.case_id), bot_core::error::log_arg(&e));
        warnings.push("The expiry could not be scheduled now; it will be retried when the bot restarts.".into());
    }

    // p. DM after the action
    if matches!(eff.action, "WARN" | "TIMEOUT" | "MUTE") && dm_enabled {
        let sent = match &user {
            Some(u) => notify_target(app, &u.id, &notify(Some(infraction.case_id))).await,
            None => None,
        };
        dm = if sent.is_some() { DmStatus::Sent } else { DmStatus::Failed };
    }

    // q. mod log
    let shown = user.clone().unwrap_or_else(|| UserRef::id(target_id.clone()));
    send_mod_log(app, gid, &infraction, &shown, &input.moderator, &input.source, &input.log_fields).await;

    Ok(ModSuccess { infraction, action: eff.action.to_string(), dm, warnings, escalation: None })
}

async fn supersede(app: &App, guild_id: &str, user_id: &str, t: InfractionType, keep: i32) -> anyhow::Result<()> {
    let old: Vec<(i32,)> = sqlx::query_as(
        r#"SELECT "id" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = $3::"InfractionType" AND "active" = true AND "id" <> $4"#,
    )
    .bind(guild_id)
    .bind(user_id)
    .bind(t.as_str())
    .bind(keep)
    .fetch_all(app.db())
    .await
    .map_err(db_err)?;
    if !old.is_empty() {
        let ids: Vec<i32> = old.iter().map(|r| r.0).collect();
        close_all(app, &ids).await?;
        for id in ids {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(expire_key(id)), ..Default::default() }).await;
        }
    }
    Ok(())
}

/// `updateMany({ where: { id: { in } }, data: { active: false } })` (no `active` filter).
async fn close_all(app: &App, ids: &[i32]) -> anyhow::Result<u64> {
    let sql = format!(r#"UPDATE "Infraction" SET "active" = false, "updatedAt" = {} WHERE "id" = ANY($1::int[])"#, bot_db::SQL_NOW);
    let r = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ids.to_vec()).execute(app.db()).await.map_err(db_err)?;
    Ok(r.rows_affected())
}

/// `mod.unban:{guildId}:{userId}`.
pub fn unban_key(guild_id: &str, user_id: &str) -> String {
    format!("mod.unban:{guild_id}:{user_id}")
}

/// `mod.expire:{infractionId}`.
pub fn expire_key(infraction_id: i32) -> String {
    format!("mod.expire:{infraction_id}")
}

/// The `mod.unban` job: payload `{ userId, infractionId }`, 20 attempts, replace.
pub fn unban_job(guild_id: &str, user_id: &str, infraction_id: Option<i32>, run_at: DateTime<Utc>) -> ScheduleJob {
    ScheduleJob::new(guild_id, JobType::ModUnban, run_at)
        .payload(json!({ "userId": user_id, "infractionId": infraction_id }))
        .dedupe_key(unban_key(guild_id, user_id))
        .max_attempts(20)
        .on_conflict(OnConflict::Replace)
}

/// The `mod.expire` job: payload `{ infractionId }` plus `roleId` for a role mute, 20 attempts, replace.
pub fn expire_job(guild_id: &str, infraction_id: i32, role_id: Option<&str>, run_at: DateTime<Utc>) -> ScheduleJob {
    let payload = match role_id {
        Some(r) => json!({ "infractionId": infraction_id, "roleId": r }),
        None => json!({ "infractionId": infraction_id }),
    };
    ScheduleJob::new(guild_id, JobType::ModExpire, run_at).payload(payload).dedupe_key(expire_key(infraction_id)).max_attempts(20).on_conflict(OnConflict::Replace)
}

// ── executeUnban / executeUnmute (§4.7) ───────────────────────────────────────

/// `executeUnban`'s success.
#[derive(Debug, Clone)]
pub struct UnbanSuccess {
    pub infraction: Infraction,
    /// `ban.user`.
    pub user: Option<UserRef>,
    /// Ban rows closed.
    pub cleared: u64,
}

/// `executeUnban({ guild, userId, moderator, reason, source })`: lifts a ban by user id. Relief action: no immunity
/// check. Never fails.
pub async fn execute_unban(app: &App, guild_id: &str, user_id: &str, moderator: &UserRef, reason: Option<&str>, source: &str) -> Result<UnbanSuccess, ModError> {
    let Some(gid) = snowflake(guild_id).map(GuildId::new) else { return Err(ModError::invalid("Missing guild or moderator.")) };
    if moderator.id.is_empty() {
        return Err(ModError::invalid("Missing guild or moderator."));
    }
    if !is_snowflake(user_id) {
        return Err(ModError::invalid("Give a user ID (17–20 digits) or a mention."));
    }
    let why = reason_text(reason);
    let key = format!("{gid}:{user_id}");
    let (a, uid, m, src) = (app.clone(), user_id.to_string(), moderator.clone(), source.to_string());
    let r = state(app).target_mutex.run(&key, move || async move { unban_locked(&a, gid, &uid, &m, why, &src).await }).await;
    r.unwrap_or_else(|e| {
        bot_util::log_error!("executeUnban failed".to_string(), bot_core::error::log_arg(&e));
        Err(ModError::new(ModCode::DiscordError, msg::INTERNAL))
    })
}

async fn unban_locked(app: &App, gid: GuildId, user_id: &str, moderator: &UserRef, why: String, source: &str) -> anyhow::Result<Result<UnbanSuccess, ModError>> {
    let g = gid.to_string();
    let r = rest(app);
    let uid = UserId::new(snowflake(user_id).unwrap_or(1));
    let Some(guild) = load_guild(app, gid) else { return Ok(Err(ModError::new(ModCode::DiscordError, msg::NO_ME))) };
    let Some(me) = get_me(&guild, r.as_ref()).await else { return Ok(Err(ModError::new(ModCode::DiscordError, msg::NO_ME))) };
    if !me.has(Permissions::BAN_MEMBERS) {
        return Ok(Err(ModError::new(ModCode::NotPermitted, not_permitted_for("BanMembers"))));
    }
    let ban = match r.get_ban(gid, uid).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            let cleared = resolve_infractions(app, &g, user_id, &[InfractionType::Ban]).await?;
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(unban_key(&g, user_id)), ..Default::default() }).await;
            let error = if cleared > 0 {
                format!("That user is not banned. {cleared} stale ban record(s) were closed.")
            } else {
                "That user is not banned.".to_string()
            };
            return Ok(Err(ModError::new(ModCode::Already, error)));
        }
        Err(e) if matches!(e.code, Some(50013) | Some(50001)) => return Ok(Err(ModError::new(ModCode::NotPermitted, not_permitted_for("BanMembers")))),
        Err(e) => return Ok(Err(map_err(&e))),
    };
    mark_bot_ban_action(app, &g, user_id, "unban", None);
    if let Err(e) = r.unban(gid, uid, build_audit_reason(&why, moderator, source)).await
        && e.code != Some(10026)
    {
        return Ok(Err(map_err(&e)));
    }
    let cleared = resolve_infractions(app, &g, user_id, &[InfractionType::Ban]).await?;
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(unban_key(&g, user_id)), ..Default::default() }).await;
    let infraction = match record_infraction(app, NewInfraction { reason: Some(why), ..NewInfraction::new(g.clone(), user_id, moderator.id.clone(), InfractionType::Unban) }).await {
        Ok(i) => i,
        Err(e) => {
            bot_util::log_error!(format!("Could not record the unban of {user_id} in {g}"), bot_core::error::log_arg(&e));
            return Ok(Err(ModError::applied("The unban was applied, but saving the case failed. Please record it manually.")));
        }
    };
    let user = UserRef::of(&ban.user);
    send_mod_log(app, gid, &infraction, &user, moderator, source, &[]).await;
    Ok(Ok(UnbanSuccess { infraction, user: Some(user), cleared }))
}

/// `executeUnmute`'s success.
#[derive(Debug, Clone)]
pub struct UnmuteSuccess {
    pub infraction: Infraction,
    /// `timeout`, `mute role`.
    pub removed: Vec<String>,
    pub cleared: u64,
    pub note: Option<String>,
}

/// `executeUnmute({ guild, target, moderator, reason, source })`: removes a member's timeout and/or mute role and
/// closes stale records. Relief action: no immunity check. Never fails.
pub async fn execute_unmute(app: &App, guild_id: &str, target: &UserRef, target_member: Option<Member>, moderator: &UserRef, reason: Option<&str>, source: &str) -> Result<UnmuteSuccess, ModError> {
    let gid = snowflake(guild_id).map(GuildId::new);
    let (Some(gid), false, false) = (gid, target.id.is_empty(), moderator.id.is_empty()) else {
        return Err(ModError::invalid("Missing guild, target or moderator."));
    };
    let why = reason_text(reason);
    let key = format!("{gid}:{}", target.id);
    let (a, t, m, src) = (app.clone(), target.clone(), moderator.clone(), source.to_string());
    let r = state(app).target_mutex.run(&key, move || async move { unmute_locked(&a, gid, &t, target_member, &m, why, &src).await }).await;
    r.unwrap_or_else(|e| {
        bot_util::log_error!("executeUnmute failed".to_string(), bot_core::error::log_arg(&e));
        Err(ModError::new(ModCode::DiscordError, msg::INTERNAL))
    })
}

async fn unmute_locked(
    app: &App,
    gid: GuildId,
    target: &UserRef,
    target_member: Option<Member>,
    moderator: &UserRef,
    why: String,
    source: &str,
) -> anyhow::Result<Result<UnmuteSuccess, ModError>> {
    let g = gid.to_string();
    let r = rest(app);
    let Some(guild) = load_guild(app, gid) else { return Ok(Err(ModError::new(ModCode::DiscordError, msg::NO_ME))) };
    let Some(me) = get_me(&guild, r.as_ref()).await else { return Ok(Err(ModError::new(ModCode::DiscordError, msg::NO_ME))) };
    let settings = app.settings().get_settings_safe(&g).await;
    let Some(tid) = snowflake(&target.id).map(UserId::new) else { return Ok(Err(map_err(&DiscordError::code(10013, "Unknown User")))) };
    let member = match target_member.as_ref().filter(|m| m.guild_id == gid) {
        Some(m) => Some(guild.member_view(m)),
        None => match guild.member_or_none(r.as_ref(), tid).await {
            Ok(m) => m,
            // Only 10007 means "not in the server": any other failure must not close records and leave the mute.
            Err(e) => return Ok(Err(map_err(&e))),
        },
    };
    let mut executor = None;
    if by_person(source) && moderator.id != guild.view.bot_id {
        match snowflake(&moderator.id).map(UserId::new) {
            Some(mid) => match guild.member_or_none(r.as_ref(), mid).await {
                Ok(Some(e)) => executor = Some(e),
                Ok(None) => return Ok(Err(ModError::invalid(msg::NO_EXECUTOR))),
                Err(e) => return Ok(Err(map_err(&e))),
            },
            None => return Ok(Err(ModError::invalid(msg::NO_EXECUTOR))),
        }
    }
    let rows: Vec<(i32,)> = sqlx::query_as(
        r#"SELECT "id" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" IN ('TIMEOUT', 'MUTE') AND "active" = true"#,
    )
    .bind(&g)
    .bind(&target.id)
    .fetch_all(app.db())
    .await
    .map_err(db_err)?;
    let ids: Vec<i32> = rows.iter().map(|r| r.0).collect();
    let close_rows = || async {
        if ids.is_empty() {
            return Ok::<u64, anyhow::Error>(0);
        }
        close_all(app, &ids).await?;
        for id in &ids {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(expire_key(*id)), ..Default::default() }).await;
        }
        Ok(ids.len() as u64)
    };
    let record = |text: String| async move {
        match record_infraction(app, NewInfraction { reason: Some(text), ..NewInfraction::new(g.clone(), target.id.clone(), moderator.id.clone(), InfractionType::Unmute) }).await {
            Ok(i) => Some(i),
            Err(e) => {
                bot_util::log_error!(format!("Could not record the unmute of {} in {gid}", target.id), bot_core::error::log_arg(&e));
                None
            }
        }
    };
    let user = member.as_ref().map(|m| m.user.clone()).unwrap_or_else(|| target.clone());

    // Not in the guild: only the records can be closed.
    let Some(member) = member else {
        if ids.is_empty() {
            return Ok(Err(ModError::new(ModCode::NotInGuild, msg::NOT_MEMBER)));
        }
        let cleared = close_rows().await?;
        let Some(infraction) = record(clamp_text(&format!("{why} (records only)"), REASON_MAX)).await else {
            return Ok(Err(ModError::applied("The records were closed, but saving the case failed. Please record it manually.")));
        };
        send_mod_log(app, gid, &infraction, &user, moderator, source, &[]).await;
        return Ok(Ok(UnmuteSuccess { infraction, removed: Vec::new(), cleared, note: Some("The user is not in the server, so only the records were closed.".into()) }));
    };

    // Preflight checks 1-3 and 8; immunity is not checked.
    if let Some(e) = check_identity(&guild.view, Some(&me), &target.id, executor.as_ref(), source)
        .or_else(|| check_executor_rank(&guild.view, Some(&member), executor.as_ref(), source))
    {
        return Ok(Err(e));
    }
    let timed_out = member.is_communication_disabled(now_ms(app));
    let role_id = settings.as_ref().and_then(|s| s.mute_role_id.clone()).filter(|r| !r.is_empty());
    let has_role = role_id.as_deref().is_some_and(|r| member.has_role(r));
    if !timed_out && !has_role && ids.is_empty() {
        return Ok(Err(ModError::new(ModCode::Already, "That member is not timed out or muted.")));
    }
    // Check everything before acting.
    if timed_out {
        if !me.has(Permissions::MODERATE_MEMBERS) {
            return Ok(Err(ModError::new(ModCode::NotPermitted, not_permitted_for("ModerateMembers"))));
        }
        if !member.moderatable {
            return Ok(Err(ModError::new(ModCode::Hierarchy, msg::BOT_ROLE)));
        }
    }
    if has_role {
        if !me.has(Permissions::MANAGE_ROLES) {
            return Ok(Err(ModError::new(ModCode::NotPermitted, not_permitted_for("ManageRoles"))));
        }
        let role = role_id.as_deref().and_then(|id| guild.view.role(id));
        let chk = guild.view.check_assignable_role(role, None, true);
        if !chk.ok {
            return Ok(Err(ModError::new(ModCode::Hierarchy, chk.reason.unwrap_or_default())));
        }
    }
    let audit = build_audit_reason(&why, moderator, source);
    let mut removed = Vec::new();
    if timed_out {
        if let Err(e) = r.timeout(gid, tid, None, audit.clone()).await {
            return Ok(Err(map_err(&e)));
        }
        removed.push("timeout".to_string());
    }
    if has_role && let Some(rid) = role_id.as_deref().and_then(snowflake) {
        if let Err(e) = r.remove_role(gid, tid, RoleId::new(rid), audit.clone()).await {
            return Ok(Err(map_err(&e)));
        }
        removed.push("mute role".to_string());
    }
    let cleared = close_rows().await?;
    let Some(infraction) = record(why).await else {
        return Ok(Err(ModError::applied("The unmute was applied, but saving the case failed. Please record it manually.")));
    };
    send_mod_log(app, gid, &infraction, &user, moderator, source, &[]).await;
    Ok(Ok(UnmuteSuccess { infraction, removed, cleared, note: None }))
}

// ── Ban import and reconciliation (§4.11) ─────────────────────────────────────

const RECONCILE_MARGIN_MS: i64 = 2_000;

/// `syncGuildBans`'s result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BanSync {
    pub imported: u64,
    pub closed: u64,
    pub complete: bool,
}

/// The `[Imported]` reason of a ban the bot did not record.
pub fn imported_reason(reason: Option<&str>) -> String {
    match reason.filter(|r| !r.is_empty()) {
        Some(r) => clamp_text(&format!("[Imported] {r}"), REASON_MAX),
        None => clamp_text("[Imported] Pre-bot ban — reason unknown", REASON_MAX),
    }
}

/// `syncGuildBans(guild, botClientId)`: imports Discord bans that have no active BAN row and closes rows whose ban is
/// gone. The reconcile half runs only when the whole ban list was read and the guild is available. `None`: no
/// permission, or a failure. Never fails.
pub async fn sync_guild_bans(app: &App, guild_id: GuildId, bot_client_id: &str) -> Option<BanSync> {
    match sync_inner(app, guild_id, bot_client_id).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("Ban sync failed for {guild_id}"), bot_core::error::log_arg(&e));
            None
        }
    }
}

async fn sync_inner(app: &App, gid: GuildId, bot_client_id: &str) -> anyhow::Result<Option<BanSync>> {
    let r = rest(app);
    let Some(guild) = load_guild(app, gid) else { return Ok(None) };
    let me = get_me(&guild, r.as_ref()).await;
    if !me.is_some_and(|m| m.has(Permissions::BAN_MEMBERS)) {
        return Ok(None);
    }
    let g = gid.to_string();
    let started_at = now_ms(app);
    let mut banned: Vec<(String, Option<String>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut after: Option<UserId> = None;
    let mut complete = false;
    for _ in 0..50 {
        let page = match bot_util::async_util::with_timeout(r.get_bans(gid, after, 1000), 30_000, "ban sync page").await {
            Ok(p) => p,
            Err(e) => {
                bot_util::log_warn!(format!("Ban sync for {g} could not read the ban list: {e}"));
                return Ok(None);
            }
        };
        let ids: Vec<String> = page.iter().map(|b| b.user.id.to_string()).collect();
        for b in &page {
            let id = b.user.id.to_string();
            if seen.insert(id.clone()) {
                banned.push((id, b.reason.clone()));
            } else if let Some(slot) = banned.iter_mut().find(|(i, _)| *i == id) {
                slot.1 = b.reason.clone();
            }
        }
        if page.len() < 1000 {
            complete = true;
            break;
        }
        after = max_snowflake(&ids).and_then(|s| snowflake(&s)).map(UserId::new);
    }

    let active: Vec<(i32, String, chrono::NaiveDateTime)> =
        sqlx::query_as(r#"SELECT "id", "targetUserId", "createdAt" FROM "Infraction" WHERE "guildId" = $1 AND "type" = 'BAN' AND "active" = true"#)
            .bind(&g)
            .fetch_all(app.db())
            .await
            .map_err(db_err)?;
    let known: HashSet<&str> = active.iter().map(|r| r.1.as_str()).collect();

    // Import
    let to_import: Vec<BulkInfraction> = banned
        .iter()
        .filter(|(id, _)| !known.contains(id.as_str()))
        .map(|(id, reason)| BulkInfraction {
            target_user_id: id.clone(),
            moderator_id: bot_client_id.to_string(),
            r#type: InfractionType::Ban,
            reason: Some(imported_reason(reason.as_deref())),
            active: Some(true),
            expires_at: None,
        })
        .collect();
    let mut imported = 0;
    for chunk in to_import.chunks(500) {
        imported += record_infractions_bulk(app, &g, chunk.to_vec()).await?;
    }

    // Reconcile
    let mut closed = 0;
    if complete && guild.view.available {
        // A row younger than the fetch (plus a margin for clock skew) is never closed.
        let gone: Vec<&(i32, String, chrono::NaiveDateTime)> =
            active.iter().filter(|r| r.2.and_utc().timestamp_millis() < started_at - RECONCILE_MARGIN_MS && !seen.contains(&r.1)).collect();
        if !gone.is_empty() {
            let ids: Vec<i32> = gone.iter().map(|r| r.0).collect();
            closed = close_all(app, &ids).await?;
            let mut users: Vec<&str> = gone.iter().map(|r| r.1.as_str()).collect();
            users.dedup();
            let users: HashSet<&str> = users.into_iter().collect();
            for u in users {
                app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(unban_key(&g, u)), ..Default::default() }).await;
            }
        }
    }
    let partial = if complete { "" } else { " (partial: reconcile skipped)" };
    bot_util::log_info!(format!("Ban sync for \"{}\" ({g}): imported {imported}, closed {closed}{partial}", guild.view.name));
    Ok(Some(BanSync { imported, closed, complete }))
}

/// `countOverdueBans(guildId)`: active BAN rows whose expiry passed more than 30 days ago. Never fails (0).
pub async fn count_overdue_bans(app: &App, guild_id: &str) -> i64 {
    let cutoff = date_of(now_ms(app) - OVERDUE_BAN_LIFT_LIMIT_MS);
    let r: Result<(i64,), _> = sqlx::query_as(
        r#"SELECT COUNT(*) FROM "Infraction" WHERE "guildId" = $1 AND "type" = 'BAN' AND "active" = true AND "expiresAt" IS NOT NULL AND "expiresAt" < $2"#,
    )
    .bind(guild_id)
    .bind(cutoff.naive_utc())
    .fetch_one(app.db())
    .await;
    match r {
        Ok(v) => v.0,
        Err(e) => {
            bot_util::log_warn!(format!("countOverdueBans failed for {guild_id}: {e}"));
            0
        }
    }
}

/// `reconcileExpiries`'s result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub scheduled: u64,
    pub expired: u64,
    pub overdue: i64,
}

/// `reconcileExpiries(guild)`: closes expired TIMEOUT rows and (re)schedules the expiry jobs of every active timed
/// BAN / TIMEOUT / MUTE row. Missing and FAILED jobs are revived. Temp bans overdue by more than 30 days are not
/// scheduled. Never fails.
pub async fn reconcile_expiries(app: &App, guild_id: &str) -> Reconciled {
    let mut out = Reconciled::default();
    if let Err(e) = reconcile_inner(app, guild_id, &mut out).await {
        bot_util::log_error!(format!("reconcileExpiries failed for {guild_id}"), bot_core::error::log_arg(&e));
    }
    out
}

async fn reconcile_inner(app: &App, g: &str, out: &mut Reconciled) -> anyhow::Result<()> {
    let now = now_ms(app);
    let sql = format!(
        r#"UPDATE "Infraction" SET "active" = false, "updatedAt" = {} WHERE "guildId" = $1 AND "type" = 'TIMEOUT' AND "active" = true AND "expiresAt" <= $2"#,
        bot_db::SQL_NOW
    );
    out.expired = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(g).bind(date_of(now).naive_utc()).execute(app.db()).await.map_err(db_err)?.rows_affected();

    out.overdue = count_overdue_bans(app, g).await;
    if out.overdue > 0 {
        bot_util::log_info!(format!("Guild {g}: {} temporary ban(s) expired more than 30 days ago were not lifted automatically", out.overdue));
    }

    // The overdue bans are excluded in the query so they can never fill the 500 slots.
    let cutoff = date_of(now - OVERDUE_BAN_LIFT_LIMIT_MS);
    let rows: Vec<(i32, InfractionType, String, chrono::NaiveDateTime)> = sqlx::query_as(
        r#"SELECT "id", "type", "targetUserId", "expiresAt" FROM "Infraction" WHERE "guildId" = $1 AND "active" = true AND "expiresAt" IS NOT NULL AND ("type" IN ('TIMEOUT', 'MUTE') OR ("type" = 'BAN' AND "expiresAt" >= $2)) ORDER BY "expiresAt" ASC LIMIT 500"#,
    )
    .bind(g)
    .bind(cutoff.naive_utc())
    .fetch_all(app.db())
    .await
    .map_err(db_err)?;
    let key_of = |r: &(i32, InfractionType, String, chrono::NaiveDateTime)| if r.1 == InfractionType::Ban { unban_key(g, &r.2) } else { expire_key(r.0) };
    let jobs = if rows.is_empty() {
        HashMap::new()
    } else {
        let keys: Vec<String> = rows.iter().map(key_of).collect();
        match app.scheduler().get_jobs_by_keys(&keys).await {
            Some(j) => j,
            None => return Ok(()),
        }
    };
    for row in &rows {
        let key = key_of(row);
        if jobs.get(&key).is_some_and(|j| j.status != JobStatus::Failed) {
            continue; // pending or running
        }
        let at = row.3.and_utc();
        let job = if row.1 == InfractionType::Ban { unban_job(g, &row.2, Some(row.0), at) } else { expire_job(g, row.0, None, at) };
        match app.scheduler().schedule_job(job).await {
            Ok(_) => out.scheduled += 1,
            Err(e) => bot_util::log_error!(format!("reconcileExpiries: could not schedule case row {} in {g}", row.0), bot_core::error::log_arg(&e)),
        }
    }
    bot_util::log_debug!(format!("reconcileExpiries {g}: scheduled {}, expired {}, overdue {}", out.scheduled, out.expired, out.overdue));
    Ok(())
}

// ── Job handlers (§4.12) ──────────────────────────────────────────────────────

const PERM_RETRY_MS: i64 = 3_600_000;
const PERM_RETRY_MAX: i64 = 168;

/// Hourly back-off while a permission is missing; a permanent failure after 7 days.
fn perm_backoff(app: &App, job: &Job, what: &str) -> BotResult<JobOutcome> {
    let retries = js_number(job.payload.get("permRetries"));
    let retries = if retries.is_finite() && retries != 0.0 { retries as i64 } else { 0 };
    if retries >= PERM_RETRY_MAX {
        return Err(PermanentJobError::new(format!("{}: missing {what} for 7 days", job.job_type)).into());
    }
    bot_util::log_warn!(format!(
        "{} for guild {}: missing {what}; retrying in 1 hour ({}/{PERM_RETRY_MAX})",
        job.job_type,
        job.guild_id,
        retries + 1
    ));
    let mut payload = match &job.payload {
        Value::Object(o) => o.clone(),
        _ => serde_json::Map::new(),
    };
    payload.insert("permRetries".into(), json!(retries + 1));
    Ok(JobOutcome::Reschedule { next_run_at: date_of(now_ms(app) + PERM_RETRY_MS), payload: Some(Value::Object(payload)) })
}

fn bot_moderator(app: &App) -> UserRef {
    bot_user(app).unwrap_or_else(|| UserRef::id(bot_user_id(app)))
}

/// `runUnbanJob(job, ctx)`: the `mod.unban` handler. Lifts a temporary ban at its expiry, whatever the module toggle
/// says. Idempotent. `Err` retries; a [`PermanentJobError`] ends the job.
pub async fn run_unban_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let app = ctx.app.clone();
    let user_id = js_string(job.payload.get("userId"));
    if !is_snowflake(&user_id) {
        return Err(PermanentJobError::new("mod.unban: bad payload").into());
    }
    let gid = GuildId::new(snowflake(&ctx.guild_id).ok_or_else(|| PermanentJobError::new("mod.unban: bad guild"))?);
    let mut inf = None;
    if let Some(v) = job.payload.get("infractionId").filter(|v| !v.is_null()) {
        let Some(id) = js_integer(Some(v)).and_then(|i| i32::try_from(i).ok()) else { return Ok(JobOutcome::Done) };
        let row = find_infraction(&app, id).await?;
        let Some(row) = row.filter(|r| r.guild_id == ctx.guild_id && r.active) else { return Ok(JobOutcome::Done) };
        if let Some(at) = row.expires_at
            && at.timestamp_millis() > now_ms(&app) + 5_000
        {
            return Ok(JobOutcome::Reschedule { next_run_at: at, payload: None }); // ran early
        }
        inf = Some(row);
    }
    let r = rest(&app);
    let me = match load_guild(&app, gid) {
        Some(g) => get_me(&g, r.as_ref()).await,
        None => None,
    };
    let Some(me) = me else { anyhow::bail!("mod.unban: could not load the bot member") };
    if !me.has(Permissions::BAN_MEMBERS) {
        return perm_backoff(&app, &job, "Ban Members");
    }
    mark_bot_ban_action(&app, &ctx.guild_id, &user_id, "unban", None);
    let text = match &inf {
        Some(i) => format!("Temporary ban expired (case #{})", i.case_id),
        None => "Temporary ban expired".to_string(),
    };
    if let Err(e) = r.unban(gid, UserId::new(snowflake(&user_id).unwrap_or(1)), clamp_text(&text, 512)).await {
        if matches!(e.code, Some(50013) | Some(50001)) {
            return perm_backoff(&app, &job, "Ban Members");
        }
        if e.code != Some(10026) {
            return Err(e.into()); // 10026: already unbanned, carry on
        }
    }
    resolve_infractions(&app, &ctx.guild_id, &user_id, &[InfractionType::Ban]).await?;
    let opts = CreateOpts { reason: Some(text), silent: true, skip_escalation: true, source: Some("expiry".into()), ..Default::default() };
    create_infraction(&app, gid, &UserRef::id(user_id.clone()), &bot_moderator(&app), InfractionType::Unban, opts).await?;
    Ok(JobOutcome::Done)
}

/// `runExpireJob(job, ctx)`: the `mod.expire` handler. Closes an expired TIMEOUT / MUTE row (removing the mute role for
/// MUTE), whatever the module toggle says. Idempotent. `Err` retries.
pub async fn run_expire_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let app = ctx.app.clone();
    let Some(id) = js_integer(job.payload.get("infractionId")).and_then(|i| i32::try_from(i).ok()) else {
        return Err(PermanentJobError::new("mod.expire: bad payload").into());
    };
    let role_id = match job.payload.get("roleId") {
        Some(Value::Null) | None => None,
        Some(v) => Some(js_string(Some(v))),
    };
    let Some(inf) = find_infraction(&app, id).await?.filter(|r| r.guild_id == ctx.guild_id && r.active) else { return Ok(JobOutcome::Done) };
    if let Some(at) = inf.expires_at
        && at.timestamp_millis() > now_ms(&app) + 5_000
    {
        return Ok(JobOutcome::Reschedule { next_run_at: at, payload: None }); // early, or the mute was extended
    }
    let close_one = |app: App| async move {
        let sql = format!(r#"UPDATE "Infraction" SET "active" = false, "updatedAt" = {} WHERE "id" = $1 AND "active" = true"#, bot_db::SQL_NOW);
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).execute(app.db()).await.map(|r| r.rows_affected()).map_err(db_err)
    };
    match inf.r#type {
        InfractionType::Timeout => {
            // Discord lifts the timeout itself; no new case.
            close_one(app.clone()).await?;
            Ok(JobOutcome::Done)
        }
        InfractionType::Mute => {
            let rid = match role_id {
                Some(r) => Some(r),
                None => app.settings().get_settings_safe(&ctx.guild_id).await.and_then(|s| s.mute_role_id.clone()),
            }
            .filter(|r| !r.is_empty());
            let gid = GuildId::new(snowflake(&ctx.guild_id).ok_or_else(|| PermanentJobError::new("mod.expire: bad guild"))?);
            let r = rest(&app);
            let Some(guild) = load_guild(&app, gid) else { anyhow::bail!("mod.expire: could not load the bot member") };
            let target = UserId::new(snowflake(&inf.target_user_id).unwrap_or(1));
            // None only for 10007 / 10013; any other error retries (the member may still hold the role)
            let member = guild.member_or_none(r.as_ref(), target).await?;
            if let (Some(member), Some(rid)) = (&member, &rid)
                && member.has_role(rid)
            {
                let Some(me) = get_me(&guild, r.as_ref()).await else { anyhow::bail!("mod.expire: could not load the bot member") };
                let role = guild.view.role(rid);
                if !me.has(Permissions::MANAGE_ROLES) || (role.is_some() && !guild.view.check_assignable_role(role, None, true).ok) {
                    return perm_backoff(&app, &job, "Manage Roles (or a role above the mute role)");
                }
                let reason = clamp_text(&format!("Mute expired (case #{})", inf.case_id), 512);
                if let Err(e) = r.remove_role(gid, target, RoleId::new(snowflake(rid).unwrap_or(1)), reason).await
                    && !matches!(e.code, Some(10011) | Some(10007))
                {
                    return Err(e.into()); // role deleted / member left: continue
                }
            }
            let count = close_one(app.clone()).await?;
            if count == 1 {
                let opts = CreateOpts {
                    reason: Some(format!("Mute expired (case #{})", inf.case_id)),
                    silent: true,
                    skip_escalation: true,
                    source: Some("expiry".into()),
                    ..Default::default()
                };
                create_infraction(&app, gid, &UserRef::id(inf.target_user_id.clone()), &bot_moderator(&app), InfractionType::Unmute, opts).await?;
            }
            Ok(JobOutcome::Done)
        }
        _ => {
            close_one(app.clone()).await?;
            Ok(JobOutcome::Done)
        }
    }
}

// ── Mute re-apply on rejoin ───────────────────────────────────────────────────

/// `reapplyMuteOnJoin(ctx)`: gives a rejoining member their mute role back while a role mute is still active. No new
/// case, no DM, no mod-log post. Never fails.
pub async fn reapply_mute_on_join(app: &App, guild_id: GuildId, user_id: UserId, settings: Option<&GuildSettings>) {
    let Some(rid) = settings.and_then(|s| s.mute_role_id.clone()).filter(|r| !r.is_empty()) else { return };
    let Some(guild) = load_guild(app, guild_id) else { return };
    let Some(role) = guild.view.role(&rid).cloned() else { return };
    let g = guild_id.to_string();
    let found: Result<Option<(i32,)>, _> = sqlx::query_as(
        r#"SELECT "caseId" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = 'MUTE' AND "active" = true AND ("expiresAt" IS NULL OR "expiresAt" > $3) ORDER BY "createdAt" DESC LIMIT 1"#,
    )
    .bind(&g)
    .bind(user_id.to_string())
    .bind(date_of(now_ms(app)).naive_utc())
    .fetch_optional(app.db())
    .await;
    let case_id = match found {
        Ok(Some(r)) => r.0,
        Ok(None) => return,
        Err(e) => {
            bot_util::log_warn!(format!("reapplyMuteOnJoin failed: {e}"));
            return;
        }
    };
    let chk = guild.view.check_assignable_role(Some(&role), None, false);
    if !chk.ok {
        if state(app).mute_warn.allow(&g) {
            bot_util::log_warn!(format!("Could not re-apply the mute role in {g}: {}", chk.reason.unwrap_or_default()));
        }
        return;
    }
    let reason = clamp_text(&format!("Re-applied mute after rejoin (case #{case_id})"), 512);
    if let Err(e) = rest(app).add_role(guild_id, user_id, RoleId::new(snowflake(&rid).unwrap_or(1)), reason).await {
        bot_util::log_warn!(format!("reapplyMuteOnJoin failed: {}", e.message));
    }
}

// ── External ban / unban hooks (§4.13) ────────────────────────────────────────

/// How long [`find_audit_entry`] waits for the entry to appear (audit entries lag the gateway event).
pub const AUDIT_DELAY_MS: u64 = 1500;

/// `findAuditEntry(guild, type, targetId, windowMs = 30000)`: a recent audit-log entry for a target. Never fails.
pub async fn find_audit_entry(app: &App, guild_id: GuildId, kind: AuditKind, target_id: &str, window_ms: i64) -> Option<AuditEntry> {
    let guild = load_guild(app, guild_id)?;
    if !guild.view.me.as_ref().is_some_and(|m| m.has(Permissions::VIEW_AUDIT_LOG)) {
        return None;
    }
    let delay = app.ext::<AuditDelay>().get();
    bot_util::async_util::sleep(delay).await;
    let entries = rest(app).audit_entries(guild_id, kind, 6).await.ok()?;
    let now = now_ms(app);
    entries.into_iter().find(|e| e.target_id.as_deref() == Some(target_id) && now - e.created_ms <= window_ms)
}

/// The per-app audit-log delay (tests shorten it).
pub struct AuditDelay(std::sync::atomic::AtomicU64);

impl Default for AuditDelay {
    fn default() -> Self {
        Self(std::sync::atomic::AtomicU64::new(AUDIT_DELAY_MS))
    }
}

impl AuditDelay {
    pub fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn has_active_ban_row(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<bool> {
    let r: Option<(i32,)> =
        sqlx::query_as(r#"SELECT "id" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = 'BAN' AND "active" = true LIMIT 1"#)
            .bind(guild_id)
            .bind(user_id)
            .fetch_optional(app.db())
            .await
            .map_err(db_err)?;
    Ok(r.is_some())
}

fn moderator_from(app: &App, entry: Option<&AuditEntry>) -> UserRef {
    match entry {
        Some(AuditEntry { executor: Some(u), .. }) => UserRef::of(u),
        Some(AuditEntry { executor_id: Some(id), .. }) => UserRef::id(id.clone()),
        _ => bot_moderator(app),
    }
}

/// `recordExternalBan({ client, guild, ban, user })`: records a ban made outside the bot as an `[External]` BAN case.
/// The cheap checks run inline; the audit-log lookup and the record are detached (the handle is returned so tests can
/// await it). Never fails.
pub async fn record_external_ban(app: &App, guild_id: GuildId, user: &User, ban_reason: Option<String>) -> Option<tokio::task::JoinHandle<()>> {
    let g = guild_id.to_string();
    let uid = user.id.to_string();
    if was_bot_ban_action(app, &g, &uid, "ban") {
        return None;
    }
    match has_active_ban_row(app, &g, &uid).await {
        Ok(true) => return None,
        Ok(false) => {}
        Err(e) => {
            bot_util::log_error!("recordExternalBan failed".to_string(), bot_core::error::log_arg(&e));
            return None;
        }
    }
    let (app, user) = (app.clone(), user.clone());
    Some(tokio::spawn(async move {
        let run = async {
            let entry = find_audit_entry(&app, guild_id, AuditKind::BanAdd, &uid, 30_000).await;
            if entry.as_ref().and_then(|e| e.executor_id.as_deref()) == Some(bot_user_id(&app).as_str()) {
                return Ok(()); // our own ban, e.g. after a restart lost the marker
            }
            if has_active_ban_row(&app, &g, &uid).await? {
                return Ok(()); // our own record may have landed during the delay
            }
            let reason = match entry.as_ref().and_then(|e| e.reason.clone()) {
                Some(r) => Some(r),
                None => match ban_reason {
                    Some(r) => Some(r),
                    None => rest(&app).get_ban(guild_id, user.id).await.ok().flatten().and_then(|b| b.reason),
                },
            };
            let text = format!("[External] {}{}", reason.as_deref().unwrap_or("No reason provided"), if entry.is_some() { "" } else { " (moderator unknown)" });
            let moderator = moderator_from(&app, entry.as_ref());
            let opts = CreateOpts { reason: Some(text), silent: true, skip_escalation: true, source: Some("external".into()), ..Default::default() };
            create_infraction(&app, guild_id, &UserRef::of(&user), &moderator, InfractionType::Ban, opts).await?;
            Ok::<_, anyhow::Error>(())
        };
        if let Err(e) = run.await {
            bot_util::log_error!("recordExternalBan failed".to_string(), bot_core::error::log_arg(&e));
        }
    }))
}

/// `handleExternalUnban({ client, guild, user, settings })`: closes BAN rows and cancels the temp-ban job (always, even
/// when the module is off), and records an `[External]` UNBAN case when the module is on. The record is detached (the
/// handle is returned). Never fails.
pub async fn handle_external_unban(app: &App, guild_id: GuildId, user: &User, settings: Option<&GuildSettings>) -> Option<tokio::task::JoinHandle<()>> {
    let g = guild_id.to_string();
    let uid = user.id.to_string();
    let count = match resolve_infractions(app, &g, &uid, &[InfractionType::Ban]).await {
        Ok(c) => c,
        Err(e) => {
            bot_util::log_error!("handleExternalUnban failed".to_string(), bot_core::error::log_arg(&e));
            return None;
        }
    };
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(unban_key(&g, &uid)), ..Default::default() }).await;
    if count == 0 || was_bot_ban_action(app, &g, &uid, "unban") || !is_module_enabled(settings, Some(ModuleKey::Moderation)) {
        return None;
    }
    let (app, user) = (app.clone(), user.clone());
    Some(tokio::spawn(async move {
        let entry = find_audit_entry(&app, guild_id, AuditKind::BanRemove, &uid, 30_000).await;
        if entry.as_ref().and_then(|e| e.executor_id.as_deref()) == Some(bot_user_id(&app).as_str()) {
            return;
        }
        let moderator = moderator_from(&app, entry.as_ref());
        let text = format!(
            "[External] {}{}",
            entry.as_ref().and_then(|e| e.reason.clone()).unwrap_or_else(|| "No reason provided".into()),
            if entry.is_some() { "" } else { " (moderator unknown)" }
        );
        let opts = CreateOpts { reason: Some(text), silent: true, skip_escalation: true, source: Some("external".into()), ..Default::default() };
        if let Err(e) = create_infraction(&app, guild_id, &UserRef::of(&user), &moderator, InfractionType::Unban, opts).await {
            bot_util::log_error!("handleExternalUnban failed".to_string(), bot_core::error::log_arg(&e));
        }
    }))
}

// ── Reply helpers (§4.14) ─────────────────────────────────────────────────────

/// What [`deliver_mod_reply`] posts.
#[derive(Debug, Clone, Default)]
pub struct ModReply {
    pub title: String,
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
}

fn log_unless_gone(what: &str, err: &bot_core::interaction::InteractionError) {
    if !is_interaction_gone(err) {
        bot_util::log_error!(format!("{what} failed"), bot_util::logger::LogArg::error(err));
    }
}

/// `deliverModReply(interaction, settings, { title, lines, warnings })`: applies `modVisibility` to a successful action
/// reply. The commands defer ephemerally first. Never fails.
pub async fn deliver_mod_reply(ctx: &impl Interactive, settings: Option<&GuildSettings>, reply: ModReply) {
    let body = clamp_text(&reply.lines.join("\n"), 4000);
    let with_warnings = |extra: &str| {
        let mut parts = vec![body.clone()];
        parts.extend(reply.warnings.iter().map(|w| format!("⚠️ {w}")));
        clamp_text(&(parts.join("\n") + extra), 4096)
    };
    let vis = settings.map(|s| s.mod_visibility).unwrap_or(ModVisibility::Public);
    let r = ctx.responder();
    let result = match vis {
        ModVisibility::Private => r.edit_reply(Payload::embed(success_embed(&reply.title, Some(&with_warnings("")), None)).to_edit()).await.map(|_| ()),
        ModVisibility::Anonymous => {
            let app = ctx.app();
            let usable = ctx.guild_id().is_some_and(|g| check_channel_usable(app, g, Some(ctx.channel_id()), &CHANNEL_POST_NEEDS).ok);
            if usable {
                let message = CreateMessage::new().embed(success_embed(&reply.title, Some(&body), None)).allowed_mentions(CreateAllowedMentions::new());
                match rest(app).send_message(ctx.channel_id(), message).await {
                    Ok(()) => {
                        let text = with_warnings("\n\n*Posted in this channel without your name.*");
                        r.edit_reply(Payload::embed(success_embed(&reply.title, Some(&text), None)).to_edit()).await.map(|_| ())
                    }
                    Err(_) => {
                        let mut more: Vec<String> = reply.warnings.clone();
                        more.push("I couldn't post the public notice here.".into());
                        let mut parts = vec![body.clone()];
                        parts.extend(more.iter().map(|w| format!("⚠️ {w}")));
                        let text = clamp_text(&parts.join("\n"), 4096);
                        r.edit_reply(Payload::embed(success_embed(&reply.title, Some(&text), None)).to_edit()).await.map(|_| ())
                    }
                }
            } else {
                let text = with_warnings("\n\n*I couldn't post the public notice here (missing permissions).*");
                r.edit_reply(Payload::embed(success_embed(&reply.title, Some(&text), None)).to_edit()).await.map(|_| ())
            }
        }
        ModVisibility::Public => {
            let _ = r.delete_reply().await;
            let first = r.follow_up(Payload::embed(success_embed(&reply.title, Some(&body), None)).no_mentions().to_follow_up()).await.map(|_| ());
            match first {
                Ok(()) if !reply.warnings.is_empty() => {
                    let notes = clamp_text(&reply.warnings.iter().map(|w| format!("• {w}")).collect::<Vec<_>>().join("\n"), 4000);
                    r.follow_up(Payload::embed(warn_embed("Note", Some(&notes), None)).ephemeral(true).to_follow_up()).await.map(|_| ())
                }
                other => other,
            }
        }
    };
    if let Err(e) = result {
        log_unless_gone("deliverModReply", &e);
    }
}

/// `deliverLookup(interaction, settings, payload)`: PUBLIC -> deleteReply + followUp; ANONYMOUS / PRIVATE ->
/// editReply (the deferred reply is ephemeral). Never fails.
pub async fn deliver_lookup(ctx: &impl Interactive, settings: Option<&GuildSettings>, payload: Payload) {
    let r = ctx.responder();
    let result = if settings.map(|s| s.mod_visibility).unwrap_or(ModVisibility::Public) == ModVisibility::Public {
        let _ = r.delete_reply().await;
        r.follow_up(payload.to_follow_up()).await.map(|_| ())
    } else {
        r.edit_reply(payload.to_edit()).await.map(|_| ())
    };
    if let Err(e) = result {
        log_unless_gone("deliverLookup", &e);
    }
}

/// The embed of [`reply_mod_error`].
pub fn mod_error_embed(res: &ModError) -> CreateEmbed {
    if res.applied {
        warn_embed("Done, But Not Recorded", Some(&res.error), None)
    } else {
        error_embed(res.code.title(), Some(&res.error), None)
    }
}

/// `replyModError(interaction, res)`: editReply an error result (always ephemeral: the commands defer ephemerally).
/// Never fails.
pub async fn reply_mod_error(ctx: &impl Interactive, res: &ModError) {
    if let Err(e) = ctx.responder().edit_reply(Payload::embed(mod_error_embed(res)).to_edit()).await {
        log_unless_gone("replyModError", &e);
    }
}

/// `RoleCheck` re-exported for callers of [`GuildView::check_assignable_role`].
pub type AssignableRole = RoleCheck;
