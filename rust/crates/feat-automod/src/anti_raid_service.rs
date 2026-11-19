//! Port of `src/services/antiRaidService.js`: spots a join flood, runs a lockdown (invites paused, @everyone's send
//! permissions taken, new joiners kicked or banned) and lifts it again. The state of a lockdown is the
//! `AntiRaidState` row (one per guild), so it survives restarts; the join windows and a short memo of "is a lockdown
//! on" are in memory per app. What happens to a joining member is [`crate::plugins::antiraid`]; `/antiraid` is
//! [`crate::commands::antiraid`].
//!
//! Chat lock: only the [`SEND_MASK`] bits (never View Channels) are taken from @everyone, and only the ones the bot
//! also holds through a role of its own. The bits taken are saved in `AntiRaidState.priorEveryoneSend` BEFORE the role
//! is edited, and ending ORs exactly those bits back into whatever the role has then.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_config::colors;
use bot_core::interaction_helpers::{PermTarget, check_channel_usable_for, missing_bot_permissions};
use bot_core::prelude::*;
use bot_db::{ANTI_RAID_STATE_INFO, AntiRaidState, GuildSettings, InsertBuilder, SelectBuilder, SqlValue, UpdateBuilder};
use bot_util::text::clamp_text;
use chrono::{DateTime, TimeZone, Utc};
use feat_moderation::moderation_service::{MOD_SOURCES, ModActionInput};
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, ChannelId, GuildId, Member, Message, Permissions, ReactionType, RoleId};
use serenity::builder::{CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateEmbedFooter, CreateMessage};

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

/// An inclusive integer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min: i64,
    pub max: i64,
}

/// `RAID`: the ranges of the settings and the timings.
pub mod raid {
    use super::Range;
    pub const THRESHOLD: Range = Range { min: 3, max: 100 };
    pub const SECONDS: Range = Range { min: 3, max: 300 };
    pub const MINUTES: Range = Range { min: 5, max: 1440 };
    pub const AGE_DAYS: Range = Range { min: 0, max: 365 };
    pub const DEFAULT_MINUTES: i64 = 30;
    pub const REASON_MAX: usize = 200;
    /// A "no lockdown" answer is trusted this long before the table is asked again.
    pub const NONE_MEMO_MS: i64 = 30_000;
    /// New-account alerts: at most one per guild per interval.
    pub const YOUNG_ALERT_EVERY_MS: u64 = 60_000;
    /// Kicks and bans of one raid wave run this many at a time.
    pub const ACTION_CONCURRENCY: usize = 4;
    /// Messages of a banned raider from the last hour go with the ban.
    pub const BAN_DELETE_SECONDS: i64 = 3600;
    /// Discord pauses invites for at most 24 hours; stay a minute under.
    pub const INVITE_CAP_MS: i64 = super::DAY_MS - super::MINUTE_MS;
    /// A pause ending this close to `AntiRaidState.invitesPausedUntil` is the one we set.
    pub const INVITE_MATCH_MS: i64 = 5000;
    /// A job that fires this much before `lockedUntil` is not due yet.
    pub const DUE_SLACK_MS: i64 = 5000;
    pub const UNLOCK_JOB: &str = "antiraid.unlock";
    pub const AUDIT_REASON_MAX: usize = 512;
}

/// The @everyone permissions a chat lock takes (View Channels is never one of them).
pub const SEND_MASK: u64 = Permissions::SEND_MESSAGES.bits()
    | Permissions::SEND_MESSAGES_IN_THREADS.bits()
    | Permissions::CREATE_PUBLIC_THREADS.bits()
    | Permissions::CREATE_PRIVATE_THREADS.bits();

/// `ACTION_LABEL[action]`.
pub fn action_label(action: &str) -> Option<&'static str> {
    Some(match action {
        "ALERT" => "Only alert the moderators",
        "KICK" => "Kick them",
        "BAN" => "Ban them",
        _ => return None,
    })
}

fn action_rank(a: Option<&str>) -> i32 {
    match a {
        Some("ALERT") => 0,
        Some("KICK") => 1,
        Some("BAN") => 2,
        _ => -1,
    }
}

const ALERT_GONE_CODES: [u64; 3] = [10003, 50001, 50013];

// ── Memory ────────────────────────────────────────────────────────────────────

/// `{ active, until, at }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Memo {
    active: bool,
    until: i64,
    at: i64,
}

/// What a lockdown did to its joiners, for its closing note (not kept across restarts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub kicked: u32,
    pub banned: u32,
    pub failed: u32,
    pub flagged: u32,
}

/// The newest new account seen since the last alert.
#[derive(Debug, Clone)]
pub struct YoungReport {
    pub user_id: String,
    pub user_tag: String,
    pub age_days: i64,
    pub min_days: i64,
    pub action: String,
}

/// `(invitesDisabledUntil, dmsDisabledUntil)` in ms.
type IncidentPair = (Option<i64>, Option<i64>);

#[derive(Debug, Clone, Copy)]
struct Incidents {
    before: IncidentPair,
    answer: IncidentPair,
    at: i64,
}

#[derive(Default)]
struct Inner {
    windows: HashMap<String, Vec<i64>>,
    memo: HashMap<String, Memo>,
    tallies: HashMap<String, Tally>,
    young_pending: HashMap<String, (u32, YoungReport)>,
    last_sweep: i64,
    /// The incident actions the bot set last: what the cache showed before the call and Discord's answer. The answer
    /// is used only while the cache still shows the old value (Discord's GUILD_UPDATE has not arrived yet).
    incidents: HashMap<String, Incidents>,
}

/// The per-app memory of anti-raid (the JS module-level maps, mutex, semaphore and throttles).
pub struct RaidMemory {
    inner: Mutex<Inner>,
    mutex: bot_core::sync::KeyedMutex,
    action_slots: bot_util::async_util::Semaphore,
    young_throttle: bot_util::async_util::Throttle,
    no_alert_throttle: bot_util::async_util::Throttle,
}

impl Default for RaidMemory {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            mutex: bot_core::sync::KeyedMutex::new(),
            action_slots: bot_util::async_util::create_semaphore(raid::ACTION_CONCURRENCY),
            young_throttle: bot_util::async_util::create_throttle(raid::YOUNG_ALERT_EVERY_MS),
            no_alert_throttle: bot_util::async_util::create_throttle(10 * MINUTE_MS as u64),
        }
    }
}

fn mem(app: &App) -> Arc<RaidMemory> {
    app.ext::<RaidMemory>()
}

fn lock(m: &RaidMemory) -> std::sync::MutexGuard<'_, Inner> {
    m.inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clears every in-memory window, memo and tally (tests).
pub fn reset_memory(app: &App) {
    let m = mem(app);
    let mut i = lock(&m);
    i.windows.clear();
    i.memo.clear();
    i.tallies.clear();
    i.young_pending.clear();
    i.incidents.clear();
}

fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

fn no_alert(app: &App, key: &str) -> bool {
    mem(app).no_alert_throttle.allow_at(key, now_ms(app))
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// An integer inside `range`, or `fallback` for anything else (`clampInt`).
pub fn clamp_int(value: Option<f64>, range: Range, fallback: i64) -> i64 {
    match value {
        Some(n) if n.is_finite() => (n.trunc() as i64).clamp(range.min, range.max),
        _ => fallback,
    }
}

/// The harsher of two actions (BAN > KICK > ALERT); `None`/unknown counts as no action.
pub fn harsher<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    let (ra, rb) = (action_rank(a), action_rank(b));
    if ra < 0 && rb < 0 {
        return None;
    }
    if rb > ra { b } else { a }
}

/// The moderation source of the cases: its own once moderationService lists it, else the closest ('automod').
pub fn moderation_source() -> &'static str {
    if MOD_SOURCES.contains(&"antiraid") { "antiraid" } else { "automod" }
}

/// Splits an @everyone permission bitfield into the send bits a chat lock takes and what is left.
pub fn split_send(bits: u64) -> (u64, u64) {
    (bits & SEND_MASK, bits & !SEND_MASK)
}

/// `BigInt(String(text))` modulo 2^64 (two's complement), or `None` when it is not a BigInt literal.
fn parse_bigint(text: &str) -> Option<u64> {
    let s = bot_util::js::trim(text);
    if s.is_empty() {
        return Some(0);
    }
    let radix_body = |body: &str, radix: u32| -> Option<u64> {
        if body.is_empty() {
            return None;
        }
        let mut v: u64 = 0;
        for c in body.chars() {
            let d = c.to_digit(radix)? as u64;
            v = v.wrapping_mul(radix as u64).wrapping_add(d);
        }
        Some(v)
    };
    let lower = s.to_ascii_lowercase();
    if let Some(b) = lower.strip_prefix("0x") {
        return radix_body(b, 16);
    }
    if let Some(b) = lower.strip_prefix("0o") {
        return radix_body(b, 8);
    }
    if let Some(b) = lower.strip_prefix("0b") {
        return radix_body(b, 2);
    }
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let v = radix_body(digits, 10)?;
    Some(if neg { v.wrapping_neg() } else { v })
}

/// The saved bits; `None` for no value, 0 for one that is not a number. Never more than the send bits.
pub fn parse_prior(text: Option<&str>) -> Option<u64> {
    let t = text?;
    Some(parse_bigint(t).unwrap_or(0) & SEND_MASK)
}

/// What @everyone gets when a lock ends: its current permissions plus exactly the saved send bits.
pub fn restored_bits(current: u64, prior: Option<&str>) -> u64 {
    current | parse_prior(prior).unwrap_or(0)
}

/// The send bits the bot can give back to @everyone: the ones it holds through a role other than @everyone (all of
/// them with Administrator). Nothing when the bot's roles cannot be read.
pub fn bot_send_bits(app: &App, guild_id: GuildId) -> u64 {
    let Some(d) = app.discord() else { return 0 };
    let bot = d.cache.current_user().id;
    bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| {
        let Some(me) = bot_core::permissions::me(g, bot) else { return 0 };
        let perms = bot_core::permissions::guild_permissions(g, bot, &me.roles);
        if perms.contains(Permissions::ADMINISTRATOR) {
            return SEND_MASK;
        }
        let mut bits = 0u64;
        for r in &me.roles {
            if r.get() != guild_id.get()
                && let Some(role) = g.roles.get(r)
            {
                bits |= role.permissions.bits();
            }
        }
        bits & SEND_MASK
    })
    .unwrap_or(0)
}

/// @everyone's permission bits from the cache.
pub fn everyone_bits(app: &App, guild_id: GuildId) -> Option<u64> {
    bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| g.roles.get(&RoleId::new(guild_id.get())).map(|r| r.permissions.bits())).flatten()
}

/// The send bits @everyone has that the bot could not give back (a chat lock leaves these alone).
fn blocked_send_bits(app: &App, guild_id: GuildId) -> u64 {
    let Some(bits) = everyone_bits(app, guild_id) else { return 0 };
    split_send(bits).0 & !bot_send_bits(app, guild_id)
}

fn many(bits: u64) -> bool {
    bits & bits.wrapping_sub(1) != 0
}

/// Why a chat lock leaves bits with @everyone (the same words in the alert and in /antiraid status).
pub fn blocked_chat_text(blocked: u64) -> String {
    let m = many(blocked);
    format!(
        "Locking chat: none of my roles has **{}**, so I cannot give {} back and @everyone keeps {}. Give one of my roles {} to lock all of chat.",
        bit_names(blocked),
        if m { "them" } else { "it" },
        if m { "them" } else { "it" },
        if m { "those permissions" } else { "that permission" }
    )
}

/// Display names of the permissions in a bitfield ("Send Messages, Create Public Threads").
pub fn bit_names(bits: u64) -> String {
    let names = bot_core::permissions::permission_names_of_bits(Permissions::from_bits_truncate(bits));
    if names.is_empty() { "none".into() } else { names.join(", ") }
}

/// The result of [`record_join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinWindow {
    pub count: usize,
    pub tripped: bool,
}

fn record_join_in(i: &mut Inner, guild_id: &str, at: i64, now: i64, threshold: i64, seconds: i64) -> JoinWindow {
    let since = now - seconds * 1000;
    let mut list: Vec<i64> = i.windows.get(guild_id).map(|l| l.iter().copied().filter(|t| *t > since).collect()).unwrap_or_default();
    let joined = at.min(now);
    if joined > since {
        list.push(joined);
    }
    let max = raid::THRESHOLD.max as usize;
    if list.len() > max {
        list.drain(0..list.len() - max);
    }
    if list.len() as i64 >= threshold {
        i.windows.remove(guild_id);
        return JoinWindow { count: list.len(), tripped: true };
    }
    let count = list.len();
    if list.is_empty() {
        i.windows.remove(guild_id);
    } else {
        i.windows.insert(guild_id.to_string(), list);
    }
    if i.windows.len() > 2000 && now - i.last_sweep > 60_000 {
        i.last_sweep = now;
        let since = now - raid::SECONDS.max * 1000;
        i.windows.retain(|_, l| l.iter().copied().max().is_some_and(|m| m > since));
    }
    JoinWindow { count, tripped: false }
}

/// Records one join in the guild's sliding window. When `threshold` joins sit inside `seconds` the window is cleared
/// and `tripped` is true, so one burst starts one lockdown.
pub fn record_join(app: &App, guild_id: &str, at: i64, now: i64, threshold: i64, seconds: i64) -> JoinWindow {
    let m = mem(app);
    let mut i = lock(&m);
    record_join_in(&mut i, guild_id, at, now, threshold, seconds)
}

/// The detection settings, clamped to their ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub threshold: i64,
    pub seconds: i64,
    pub minutes: i64,
}

/// `detectionOf(settings)`.
pub fn detection_of(settings: Option<&GuildSettings>) -> Detection {
    Detection {
        threshold: clamp_int(settings.map(|s| f64::from(s.anti_raid_join_threshold)), raid::THRESHOLD, 10),
        seconds: clamp_int(settings.map(|s| f64::from(s.anti_raid_join_seconds)), raid::SECONDS, 10),
        minutes: clamp_int(settings.map(|s| f64::from(s.anti_raid_lockdown_minutes)), raid::MINUTES, raid::DEFAULT_MINUTES),
    }
}

/// `{ ageDays, minDays }` of a too-young account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Young {
    pub age_days: i64,
    pub min_days: i64,
}

/// Whether the account is younger than `antiRaidMinAccountAgeDays` (0 = off). `None` when the check is off or the
/// account is old enough.
pub fn young_account(created_ms: Option<f64>, settings: Option<&GuildSettings>, now: i64) -> Option<Young> {
    let min_days = clamp_int(settings.map(|s| f64::from(s.anti_raid_min_account_age_days)), raid::AGE_DAYS, 0);
    let created = created_ms.filter(|c| c.is_finite())?;
    if min_days <= 0 {
        return None;
    }
    let age = now as f64 - created;
    if age >= (min_days * DAY_MS) as f64 {
        return None;
    }
    Some(Young { age_days: (age / DAY_MS as f64).floor().max(0.0) as i64, min_days })
}

/// True while the row describes a lockdown that has not run out.
pub fn is_active(row: Option<&AntiRaidState>, now: i64) -> bool {
    row.is_some_and(|r| r.locked_at.is_some() && r.locked_until.is_some_and(|u| u.timestamp_millis() > now))
}

/// What the bot lacks for what the settings ask of it.
pub fn permission_problems(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>) -> Vec<String> {
    let need = |perm: &str, why: &str| -> Option<String> {
        if missing_bot_permissions(app, PermTarget::Guild(guild_id), &[perm]).is_empty() {
            return None;
        }
        let name = bot_core::permissions::permission_names(&[perm]).into_iter().next().unwrap_or_default();
        Some(format!("{why}: I need **{name}**."))
    };
    let mut out = Vec::new();
    let Some(s) = settings else { return out };
    if s.anti_raid_pause_invites {
        out.extend(need("ManageGuild", "Pausing invites"));
    }
    if s.anti_raid_lock_chat {
        out.extend(need("ManageRoles", "Locking chat"));
        let blocked = blocked_send_bits(app, guild_id);
        if blocked != 0 {
            out.push(blocked_chat_text(blocked));
        }
    }
    let mut actions = vec![s.anti_raid_action.as_str()];
    if clamp_int(Some(f64::from(s.anti_raid_min_account_age_days)), raid::AGE_DAYS, 0) > 0 {
        actions.push(s.anti_raid_young_action.as_str());
    }
    if actions.contains(&"KICK") {
        out.extend(need("KickMembers", "Kicking members"));
    }
    if actions.contains(&"BAN") {
        out.extend(need("BanMembers", "Banning members"));
    }
    out
}

/// The settings as lines for `/antiraid status` and `config`.
pub fn config_lines(settings: &GuildSettings) -> Vec<String> {
    let d = detection_of(Some(settings));
    let age = clamp_int(Some(f64::from(settings.anti_raid_min_account_age_days)), raid::AGE_DAYS, 0);
    let alerts = match (&settings.anti_raid_alert_channel_id, &settings.mod_log_channel_id) {
        (Some(a), _) if !a.is_empty() => format!("<#{a}>"),
        (_, Some(m)) if !m.is_empty() => format!("<#{m}> (moderation log)"),
        _ => "no channel set".into(),
    };
    vec![
        format!("**Detection:** {} joins within {} seconds", d.threshold, d.seconds),
        format!(
            "**Members who join during a raid:** {}",
            action_label(settings.anti_raid_action.as_str()).unwrap_or("Only alert the moderators")
        ),
        format!(
            "**Lockdown:** {}, invites {}, chat {}",
            bot_util::parse_duration::format_duration_long((d.minutes * MINUTE_MS) as f64),
            if settings.anti_raid_pause_invites { "paused" } else { "left open" },
            if settings.anti_raid_lock_chat { "locked" } else { "left open" }
        ),
        format!(
            "**New accounts:** {}",
            if age > 0 {
                format!(
                    "younger than {age} day{}: {}",
                    if age == 1 { "" } else { "s" },
                    bot_util::js::lower(action_label(settings.anti_raid_young_action.as_str()).unwrap_or(""))
                )
            } else {
                "not checked".into()
            }
        ),
        format!("**Alerts:** {alerts}"),
    ]
}

fn audit_reason(text: &str) -> String {
    clamp_text(&format!("Anti-Raid: {text}"), raid::AUDIT_REASON_MAX)
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

// ── State ─────────────────────────────────────────────────────────────────────

/// The guild's `AntiRaidState` row. Fails on a database failure.
pub async fn load_state(pool: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<Option<AntiRaidState>> {
    Ok(SelectBuilder::new(&ANTI_RAID_STATE_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_optional(pool).await?)
}

/// The counters of the guild's lockdown, created at zero.
pub fn tally_of(app: &App, guild_id: &str) -> Tally {
    let m = mem(app);
    *lock(&m).tallies.entry(guild_id.to_string()).or_default()
}

fn tally_add(app: &App, guild_id: &str, f: impl FnOnce(&mut Tally)) {
    let m = mem(app);
    f(lock(&m).tallies.entry(guild_id.to_string()).or_default());
}

/// Sets the memo to "a lockdown is on until `until`".
pub fn mark_active(app: &App, guild_id: &str, until: i64) {
    let now = now_ms(app);
    let m = mem(app);
    lock(&m).memo.insert(guild_id.to_string(), Memo { active: true, until, at: now });
}

fn mark_none(app: &App, guild_id: &str) {
    let now = now_ms(app);
    let m = mem(app);
    lock(&m).memo.insert(guild_id.to_string(), Memo { active: false, until: 0, at: now });
}

fn memo_delete(app: &App, guild_id: &str) {
    let m = mem(app);
    lock(&m).memo.remove(guild_id);
}

fn peek_in(i: &Inner, guild_id: &str, now: i64) -> Option<bool> {
    let m = i.memo.get(guild_id)?;
    if m.active {
        return (m.until > now).then_some(true);
    }
    (now - m.at < raid::NONE_MEMO_MS).then_some(false)
}

/// Sync answer from the memo: `Some(true/false)`, or `None` when the table has to be asked.
pub fn peek_locked(app: &App, guild_id: &str, now: i64) -> Option<bool> {
    let m = mem(app);
    let i = lock(&m);
    peek_in(&i, guild_id, now)
}

/// Whether a lockdown is on. Memo first; one table read otherwise. A row that ran out without being ended is ended
/// in the background. Never fails: a failed read counts as "no lockdown".
pub async fn is_locked(app: &App, guild_id: GuildId, now: i64) -> bool {
    let g = guild_id.to_string();
    if let Some(fast) = peek_locked(app, &g, now) {
        return fast;
    }
    match load_state(app.db(), &g).await {
        Ok(row) => {
            if is_active(row.as_ref(), now) {
                mark_active(app, &g, row.as_ref().and_then(|r| r.locked_until).map(|u| u.timestamp_millis()).unwrap_or(now));
                return true;
            }
            // A raid may have tripped while the read was in flight: its "active" memo must not be overwritten.
            {
                let m = mem(app);
                let mut i = lock(&m);
                if let Some(memo) = i.memo.get(&g)
                    && memo.active
                    && memo.until > now
                {
                    return true;
                }
                i.memo.insert(g.clone(), Memo { active: false, until: 0, at: now_ms(app) });
            }
            if row.as_ref().is_some_and(|r| r.locked_at.is_some()) {
                let a = app.clone();
                tokio::spawn(async move {
                    match end_lockdown(&a, guild_id, true).await {
                        Ok(res) if res.was_active => {
                            announce_end(&a, guild_id, None, &res, EndAnnounce { how: "expired", ended_by_id: None, except_channel_id: None }).await;
                        }
                        Ok(_) => {}
                        Err(e) => bot_util::log_warn!(format!("[antiraid] {guild_id}: could not end an expired lockdown: {e}")),
                    }
                });
            }
            false
        }
        Err(e) => {
            if no_alert(app, &format!("{g}:state")) {
                bot_util::log_warn!(format!("[antiraid] {g}: could not read the lockdown state: {e}"));
            }
            false
        }
    }
}

/// The in-memory half of a join: peeks the memo, counts the join and, when it trips, marks the lockdown active, all
/// under one lock so a burst of joins starts exactly one lockdown. `(locked, Some(window) when it tripped)`.
pub fn count_join(app: &App, guild_id: &str, joined_at: i64, now: i64, detection: Detection) -> (bool, Option<JoinWindow>) {
    let m = mem(app);
    let mut i = lock(&m);
    if peek_in(&i, guild_id, now) == Some(true) {
        return (true, None);
    }
    let win = record_join_in(&mut i, guild_id, joined_at, now, detection.threshold, detection.seconds);
    if win.tripped {
        i.memo.insert(guild_id.to_string(), Memo { active: true, until: now + detection.minutes * MINUTE_MS, at: now });
        return (true, Some(win));
    }
    (false, None)
}

// ── Starting a lockdown ───────────────────────────────────────────────────────

/// What a start or an end did: `chat`, `invites` and the problems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effects {
    pub chat: &'static str,
    pub invites: &'static str,
    pub errors: Vec<String>,
}

impl Default for Effects {
    fn default() -> Self {
        Self { chat: "skipped", invites: "skipped", errors: Vec::new() }
    }
}

/// Error text of a failed Discord call.
fn why(err: &serenity::Error) -> String {
    match bot_core::discord::error_code(err) {
        Some(50013 | 50001) => "Discord refused (missing permission or my role is too low)".into(),
        _ => clamp_text(&bot_core::discord::error_message(err), 150),
    }
}

struct ChatPlan {
    prior: Option<String>,
    strip: Option<u64>,
    taken: u64,
    blocked: u64,
}

fn take_chat(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, existing: Option<&AntiRaidState>, effects: &mut Effects) -> ChatPlan {
    let existing_prior = existing.and_then(|e| e.prior_everyone_send.clone());
    let pending = parse_prior(existing_prior.as_deref()); // a restore left over from an earlier lockdown is kept
    let keep = ChatPlan { prior: existing_prior.clone(), strip: None, taken: 0, blocked: 0 };
    if !settings.is_some_and(|s| s.anti_raid_lock_chat) {
        return keep;
    }
    if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageRoles"]).is_empty() {
        effects.chat = "failed";
        effects.errors.push("Chat not locked: I need **Manage Roles**.".into());
        return keep;
    }
    let Some(bits) = everyone_bits(app, guild_id) else {
        effects.chat = "failed";
        effects.errors.push("Chat not locked: the @everyone role was not found.".into());
        return keep;
    };
    let taken = split_send(bits).0 & bot_send_bits(app, guild_id); // only what can be given back afterwards
    let blocked = split_send(bits).0 & !taken;
    let total = pending.unwrap_or(0) | taken;
    ChatPlan { prior: if total == 0 && pending.is_none() { None } else { Some(total.to_string()) }, strip: Some(bits & !taken), taken, blocked }
}

fn cached_incidents(app: &App, guild_id: GuildId) -> IncidentPair {
    bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| {
        g.incidents_data
            .map(|d| (d.invites_disabled_until.map(|t| t.timestamp_millis()), d.dms_disabled_until.map(|t| t.timestamp_millis())))
            .unwrap_or((None, None))
    })
    .unwrap_or((None, None))
}

/// The incident actions as the bot knows them (`guild.incidentsData`): `(invitesDisabledUntil, dmsDisabledUntil)` in
/// ms. The cache, unless the bot just set them and Discord's GUILD_UPDATE has not reached the cache yet.
pub fn incidents_of(app: &App, guild_id: GuildId) -> IncidentPair {
    let cached = cached_incidents(app, guild_id);
    let now = now_ms(app);
    let m = mem(app);
    let mut i = lock(&m);
    let key = guild_id.to_string();
    if let Some(o) = i.incidents.get(&key).copied() {
        if o.before == cached && now - o.at < 60_000 {
            return o.answer;
        }
        i.incidents.remove(&key);
    }
    cached
}

fn iso(ms: i64) -> String {
    bot_util::js::to_iso_string(ms as f64).unwrap_or_default()
}

/// `guild.setIncidentActions(payload)`; the answer is remembered until Discord's GUILD_UPDATE reaches the cache.
#[allow(clippy::result_large_err)] // serenity::Error, as returned by serenity
async fn set_incident_actions(app: &App, guild_id: GuildId, invites_until: Option<i64>, keep_dms: Option<i64>) -> Result<(), serenity::Error> {
    let Some(http) = app.http() else { return Err(serenity::Error::Other("offline")) };
    let mut body = serde_json::Map::new();
    body.insert("invites_disabled_until".into(), invites_until.map(|ms| Value::String(iso(ms))).unwrap_or(Value::Null));
    if let Some(d) = keep_dms {
        body.insert("dms_disabled_until".into(), Value::String(iso(d)));
    }
    let before = cached_incidents(app, guild_id);
    let data = http.edit_guild_incident_actions(guild_id, &Value::Object(body)).await?;
    let answer = (data.invites_disabled_until.map(|t| t.timestamp_millis()), data.dms_disabled_until.map(|t| t.timestamp_millis()));
    let at = now_ms(app);
    let m = mem(app);
    lock(&m).incidents.insert(guild_id.to_string(), Incidents { before, answer, at });
    Ok(())
}

/// Pauses invites until `until`. Returns that end when the bot set the pause, else `None`.
async fn pause_invites(app: &App, guild_id: GuildId, until: i64, effects: &mut Effects) -> Option<i64> {
    if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageGuild"]).is_empty() {
        effects.invites = "failed";
        effects.errors.push("Invites not paused: I need **Manage Server**.".into());
        return None;
    }
    let (current, dms) = incidents_of(app, guild_id);
    if current.is_some_and(|c| c >= until) {
        effects.invites = "already";
        return None;
    }
    let keep_dms = dms.filter(|d| *d > now_ms(app)); // keep a DM pause somebody else set
    match set_incident_actions(app, guild_id, Some(until), keep_dms).await {
        Ok(()) => {
            effects.invites = "paused";
            Some(until)
        }
        Err(e) => {
            effects.invites = "failed";
            effects.errors.push(format!("Invites not paused: {}.", why(&e)));
            None
        }
    }
}

/// `{ joins, seconds }` of a detected raid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub joins: usize,
    pub seconds: i64,
}

/// The arguments of [`start_lockdown`].
#[derive(Debug, Clone, Default)]
pub struct StartInput {
    pub guild_id: Option<GuildId>,
    pub settings: Option<Settings>,
    pub started_by_id: Option<String>,
    pub reason: Option<String>,
    pub minutes: Option<i64>,
    pub trigger: Option<Trigger>,
}

/// What [`start_lockdown`] returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Started {
    /// `{ ok: true, started, state, minutes?, effects? }`.
    Ok { started: bool, state: AntiRaidState, minutes: Option<i64>, effects: Option<Effects> },
    /// `{ ok: false, error }`.
    Failed { error: String },
}

/// Starts a lockdown unless one is on. The row is saved and the unlock job scheduled first; then invites are paused
/// and chat locked as the settings ask, each failing alone. Serialised per guild, so a burst of joins starts exactly
/// one. Never fails.
pub async fn start_lockdown(app: &App, guild_id: GuildId, input: StartInput) -> Started {
    let m = mem(app);
    let a = app.clone();
    m.mutex
        .run(&guild_id.to_string(), move || async move {
            match do_start(&a, guild_id, input).await {
                Ok(s) => s,
                Err(e) => {
                    bot_util::log_error!(format!("[antiraid] {guild_id}: could not start a lockdown"), bot_core::error::log_arg(&e));
                    memo_delete(&a, &guild_id.to_string());
                    Started::Failed { error: "The lockdown could not be saved. Please try again in a moment.".into() }
                }
            }
        })
        .await
}

fn unlock_key(guild_id: &str) -> String {
    format!("{}:{guild_id}", raid::UNLOCK_JOB)
}

/// The unlock job of a lockdown (`antiraid.unlock:{guildId}`, payload `{}`, replace).
pub fn unlock_job(guild_id: &str, run_at: DateTime<Utc>) -> ScheduleJob {
    ScheduleJob::new(guild_id, JobType::AntiraidUnlock, run_at).payload(json!({})).dedupe_key(unlock_key(guild_id)).on_conflict(OnConflict::Replace)
}

async fn update_row(app: &App, guild_id: &str, sets: Vec<(&str, SqlValue)>) -> anyhow::Result<Option<AntiRaidState>> {
    let mut b = UpdateBuilder::new(&ANTI_RAID_STATE_INFO);
    for (col, v) in sets {
        b = b.set_typed(col, v)?;
    }
    Ok(b.filter("guildId", json!(guild_id))?.build()?.fetch_optional(app.db()).await?)
}

fn ts(dt: Option<DateTime<Utc>>) -> SqlValue {
    match dt {
        Some(d) => SqlValue::timestamp(d),
        None => SqlValue::Timestamp(None),
    }
}

fn text(v: Option<String>) -> SqlValue {
    SqlValue::Text(v)
}

fn cleared(prior: Option<String>) -> Vec<(&'static str, SqlValue)> {
    vec![
        ("lockedAt", ts(None)),
        ("lockedUntil", ts(None)),
        ("lockedById", text(None)),
        ("reason", text(None)),
        ("priorEveryoneSend", text(prior)),
        ("invitesPausedUntil", ts(None)),
    ]
}

/// Edits @everyone's permissions and puts the answer into the cache.
#[allow(clippy::result_large_err)] // serenity::Error, as returned by serenity
async fn set_everyone(app: &App, guild_id: GuildId, bits: u64, reason: &str) -> Result<(), serenity::Error> {
    let Some(http) = app.http() else { return Err(serenity::Error::Other("offline")) };
    let role = http.edit_role(guild_id, RoleId::new(guild_id.get()), &json!({ "permissions": bits.to_string() }), Some(reason)).await?;
    bot_core::discord::cache_created_role(app, guild_id, &role);
    Ok(())
}

async fn do_start(app: &App, guild_id: GuildId, input: StartInput) -> anyhow::Result<Started> {
    let g = guild_id.to_string();
    let now = now_ms(app);
    let existing = load_state(app.db(), &g).await?;
    if is_active(existing.as_ref(), now)
        && let Some(e) = existing.clone()
    {
        mark_active(app, &g, e.locked_until.map(|u| u.timestamp_millis()).unwrap_or(now));
        return Ok(Started::Ok { started: false, state: e, minutes: None, effects: None });
    }
    let s = match input.settings.clone() {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(&g).await,
    };
    let mins = clamp_int(input.minutes.map(|m| m as f64).or(s.as_ref().map(|s| f64::from(s.anti_raid_lockdown_minutes))), raid::MINUTES, raid::DEFAULT_MINUTES);
    let locked_at = ms_to_dt(now);
    let locked_until = ms_to_dt(now + mins * MINUTE_MS);
    let mut effects = Effects::default();
    let reason = input.reason.as_deref().map(|r| clamp_text(bot_util::js::trim(r), raid::REASON_MAX)).filter(|r| !r.is_empty());

    let chat = take_chat(app, guild_id, s.as_deref(), existing.as_ref(), &mut effects);
    app.settings().ensure_guild_settings(&g).await?;
    // Saved before anything is touched: a crash after this leaves a lockdown the job will end, never lost bits.
    let data = |b: InsertBuilder| -> Result<InsertBuilder, bot_db::DbError> {
        b.typed("lockedAt", ts(Some(locked_at)))?
            .typed("lockedUntil", ts(Some(locked_until)))?
            .typed("lockedById", text(input.started_by_id.clone()))?
            .typed("reason", text(reason.clone()))?
            .typed("priorEveryoneSend", text(chat.prior.clone()))?
            .typed("invitesPausedUntil", ts(None))
    };
    let set = [
        ("lockedAt", locked_at_json(locked_at)),
        ("lockedUntil", locked_at_json(locked_until)),
        ("lockedById", json!(input.started_by_id)),
        ("reason", json!(reason)),
        ("priorEveryoneSend", json!(chat.prior)),
        ("invitesPausedUntil", Value::Null),
    ];
    let mut state: AntiRaidState = data(InsertBuilder::new(&ANTI_RAID_STATE_INFO).value("guildId", json!(g))?)?
        .on_conflict_update(&["guildId"], set)?
        .build()?
        .fetch_one(app.db())
        .await?;
    mark_active(app, &g, locked_until.timestamp_millis());

    if let Err(e) = app.scheduler().schedule_job(unlock_job(&g, locked_until)).await {
        // No job would ever end it: undo the row.
        if let Err(e2) = update_row(app, &g, cleared(existing.as_ref().and_then(|r| r.prior_everyone_send.clone()))).await {
            bot_util::log_error!(format!("[antiraid] {g}: could not undo the lockdown row"), bot_core::error::log_arg(&e2));
        }
        memo_delete(app, &g);
        return Err(e);
    }

    {
        let m = mem(app);
        lock(&m).windows.remove(&g);
    }

    if let Some(strip) = chat.strip {
        if chat.taken == 0 {
            effects.chat = if chat.blocked == 0 { "nothing" } else { "failed" };
            if chat.blocked != 0 {
                effects.errors.push(blocked_chat_text(chat.blocked));
            }
        } else {
            match set_everyone(app, guild_id, strip, &audit_reason("raid lockdown")).await {
                Ok(()) => {
                    effects.chat = if chat.blocked == 0 { "locked" } else { "partial" };
                    if chat.blocked != 0 {
                        effects.errors.push(blocked_chat_text(chat.blocked));
                    }
                }
                Err(err) => {
                    effects.chat = "failed";
                    effects.errors.push(format!("Chat not locked: {}.", why(&err)));
                    // Nothing was taken, so there is nothing to give back (keep an earlier pending restore).
                    let prior = existing.as_ref().and_then(|r| r.prior_everyone_send.clone());
                    if let Err(e) = update_row(app, &g, vec![("priorEveryoneSend", text(prior.clone()))]).await {
                        bot_util::log_error!(format!("[antiraid] {g}: could not reset the saved permissions"), bot_core::error::log_arg(&e));
                    }
                    state.prior_everyone_send = prior;
                }
            }
        }
    }
    if s.as_ref().is_some_and(|s| s.anti_raid_pause_invites) {
        // The end the bot really set (capped under lockedUntil for a 24 hour lockdown) is saved.
        let until = locked_until.timestamp_millis().min(now_ms(app) + raid::INVITE_CAP_MS);
        if let Some(paused) = pause_invites(app, guild_id, until, &mut effects).await {
            state.invites_paused_until = Some(ms_to_dt(paused));
            if let Err(e) = update_row(app, &g, vec![("invitesPausedUntil", ts(Some(ms_to_dt(paused))))]).await {
                bot_util::log_error!(format!("[antiraid] {g}: could not save the invite pause"), bot_core::error::log_arg(&e));
            }
        }
    }
    bot_util::log_info!(format!(
        "[antiraid] {g}: lockdown until {} ({}); chat {}, invites {}",
        iso(locked_until.timestamp_millis()),
        input.started_by_id.as_deref().map(|b| format!("by {b}")).unwrap_or_else(|| "automatic".into()),
        effects.chat,
        effects.invites
    ));
    Ok(Started::Ok { started: true, state, minutes: Some(mins), effects: Some(effects) })
}

fn locked_at_json(dt: DateTime<Utc>) -> Value {
    json!(iso(dt.timestamp_millis()))
}

// ── Ending a lockdown ─────────────────────────────────────────────────────────

struct Restore {
    ok: bool,
    changed: bool,
    permanent: bool,
    error: Option<String>,
    keep: Option<String>,
}

async fn restore_everyone(app: &App, guild_id: GuildId, prior: Option<&str>) -> Restore {
    let ok = |changed| Restore { ok: true, changed, permanent: false, error: None, keep: None };
    let Some(current) = everyone_bits(app, guild_id) else { return ok(false) };
    let owed = restored_bits(current, prior) & !current; // saved bits @everyone still lacks
    if owed == 0 {
        return ok(false);
    }
    let names = bit_names(owed);
    if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageRoles"]).is_empty() {
        return Restore {
            ok: false,
            changed: false,
            permanent: true,
            error: Some(format!("I need **Manage Roles** to give @everyone back: {names}. Run `/antiraid lockdown end` once I have it.")),
            keep: None,
        };
    }
    // A bit the bot cannot grant must not stop the others from coming back. It stays saved, and is named.
    let stuck = owed & !bot_send_bits(app, guild_id);
    let give = owed & !stuck;
    let mut changed = false;
    if give != 0 {
        if let Err(err) = set_everyone(app, guild_id, current | give, &audit_reason("lockdown ended")).await {
            return Restore {
                ok: false,
                changed: false,
                permanent: matches!(bot_core::discord::error_code(&err), Some(50013 | 50001)),
                error: Some(format!("Could not give @everyone back {names}: {}. Run `/antiraid lockdown end` to try again.", why(&err))),
                keep: None,
            };
        }
        changed = true;
    }
    if stuck != 0 {
        let m = many(stuck);
        let (them, perms) = if m { ("them", "those permissions") } else { ("it", "that permission") };
        return Restore {
            ok: false,
            changed,
            permanent: true,
            error: Some(format!(
                "@everyone still lacks {}: none of my roles has {them}, so I cannot give {them} back. Give one of my roles {perms} (or turn {them} on for @everyone yourself), then run `/antiraid lockdown end`.",
                bit_names(stuck)
            )),
            keep: Some(stuck.to_string()),
        };
    }
    ok(changed)
}

/// Resumes the invites the bot paused: the guild's pause must end where the one saved at start ends.
async fn resume_invites(app: &App, guild_id: GuildId, row: &AntiRaidState) -> (&'static str, Option<String>) {
    let (current, dms) = incidents_of(app, guild_id);
    let ours = row.invites_paused_until.map(|d| d.timestamp_millis());
    let now = now_ms(app);
    if current.is_some_and(|c| c <= now) {
        return ("none", None);
    }
    if let Some(c) = current
        && ours.is_none_or(|o| (c - o).abs() > raid::INVITE_MATCH_MS)
    {
        return ("foreign", None); // not ours
    }
    if current.is_none() && ours.is_none() {
        return ("none", None);
    }
    if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageGuild"]).is_empty() {
        return if current.is_none() { ("none", None) } else { ("failed", Some("Invites stay paused: I need **Manage Server** to resume them.".into())) };
    }
    let keep_dms = dms.filter(|d| *d > now);
    match set_incident_actions(app, guild_id, None, keep_dms).await {
        Ok(()) => (if current.is_none() { "none" } else { "resumed" }, None),
        Err(e) => {
            if current.is_none() {
                ("none", None)
            } else {
                ("failed", Some(format!("Invites stay paused: {}.", why(&e))))
            }
        }
    }
}

/// What [`end_lockdown`] did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ended {
    pub was_active: bool,
    pub restored: bool,
    pub pending_restore: bool,
    pub permanent: bool,
    pub row: Option<AntiRaidState>,
    pub effects: Effects,
    pub tally: Option<Tally>,
}

/// Ends the lockdown: gives @everyone its send permissions back, resumes invites, clears the row and the unlock job.
/// Idempotent. A chat restore that fails keeps the saved bits (and the job) so another call retries it. `only_if_due`
/// (the unlock job) leaves a lockdown that is not due yet alone. Fails only on a database failure.
pub async fn end_lockdown(app: &App, guild_id: GuildId, only_if_due: bool) -> anyhow::Result<Ended> {
    let m = mem(app);
    let a = app.clone();
    m.mutex.run(&guild_id.to_string(), move || async move { do_end(&a, guild_id, only_if_due).await }).await
}

async fn do_end(app: &App, guild_id: GuildId, only_if_due: bool) -> anyhow::Result<Ended> {
    let g = guild_id.to_string();
    let now = now_ms(app);
    let row = load_state(app.db(), &g).await?;
    let active = row.as_ref().is_some_and(|r| r.locked_at.is_some());
    let pending = row.as_ref().is_some_and(|r| r.prior_everyone_send.is_some());
    let Some(row) = row.filter(|_| active || pending) else {
        mark_none(app, &g);
        return Ok(Ended::default());
    };
    if only_if_due && active && row.locked_until.is_some_and(|u| u.timestamp_millis() > now + raid::DUE_SLACK_MS) {
        return Ok(Ended { row: Some(row), ..Default::default() });
    }

    {
        let mm = mem(app);
        lock(&mm).windows.remove(&g); // the burst that caused this must not start the next one
    }
    let mut effects = Effects::default();
    let mut permanent = false;
    let mut keep_prior = false;
    let mut keep_value: Option<String> = None;

    if pending {
        let r = restore_everyone(app, guild_id, row.prior_everyone_send.as_deref()).await;
        if r.ok {
            effects.chat = if r.changed { "restored" } else { "unchanged" };
        } else {
            effects.chat = if r.changed { "partialRestore" } else { "failed" };
            effects.errors.extend(r.error);
            keep_prior = true;
            keep_value = r.keep.or_else(|| row.prior_everyone_send.clone());
            permanent = r.permanent;
        }
    }
    if active {
        let (state, error) = resume_invites(app, guild_id, &row).await;
        effects.invites = state;
        effects.errors.extend(error);
    }

    update_row(app, &g, cleared(if keep_prior { keep_value } else { None })).await?;
    mark_none(app, &g);
    // A failed restore keeps the job: the scheduler retries it.
    if !keep_prior {
        app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(unlock_key(&g)), ..Default::default() }).await;
    }
    let tally = {
        let mm = mem(app);
        lock(&mm).tallies.remove(&g)
    };
    bot_util::log_info!(format!("[antiraid] {g}: lockdown ended; chat {}, invites {}", effects.chat, effects.invites));
    Ok(Ended { was_active: active, restored: effects.chat == "restored", pending_restore: keep_prior, permanent, row: Some(row), effects, tally })
}

// ── Alerts ────────────────────────────────────────────────────────────────────

/// `INVITE_TEXT[state]`.
pub fn invite_text(state: &str) -> &'static str {
    match state {
        "paused" => "⏸️ Paused",
        "already" => "⏸️ Already paused for longer",
        "failed" => "⚠️ Not paused",
        "skipped" => "Left open",
        "none" => "—",
        "resumed" => "▶️ Resumed",
        "foreign" => "Left as it was (not paused by me)",
        _ => "—",
    }
}

/// `CHAT_TEXT[state]`.
pub fn chat_text(state: &str) -> &'static str {
    match state {
        "locked" => "🔇 @everyone cannot send messages",
        "nothing" => "🔇 @everyone could not send already",
        "failed" => "⚠️ Not locked",
        "partial" => "🔇 Partly locked (see below)",
        "partialRestore" => "⚠️ Only partly allowed again (see below)",
        "skipped" => "Left open",
        "restored" => "🔊 Sending allowed again",
        "unchanged" => "🔊 Already allowed",
        "none" => "—",
        _ => "—",
    }
}

/// The custom id of the End button (`antiraid:end`).
pub fn end_button_id() -> String {
    bot_config::custom_id!("antiraid", "end").unwrap_or_else(|_| "antiraid:end".into())
}

/// The "End lockdown" button row of an alert.
pub fn end_button_row() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(end_button_id()).label("End lockdown").emoji(ReactionType::Unicode("🔓".into())).style(ButtonStyle::Danger),
    ])
}

fn ts_of(dt: Option<DateTime<Utc>>, style: &str) -> String {
    dt.map(|d| bot_util::time::discord_timestamp(&d, style)).unwrap_or_default()
}

/// The alert for a started lockdown.
pub fn build_start_embed(state: &AntiRaidState, settings: Option<&GuildSettings>, effects: Option<&Effects>, trigger: Option<Trigger>, problems: &[String]) -> CreateEmbed {
    let line = match trigger {
        Some(t) => format!("**{}** members joined within **{}** seconds.", t.joins, t.seconds),
        None => format!(
            "Started by {}{}",
            state.locked_by_id.as_deref().map(|id| format!("<@{id}>")).unwrap_or_else(|| "a moderator".into()),
            match state.reason.as_deref().filter(|r| !r.is_empty()) {
                Some(r) => format!(": {}", clamp_text(r, raid::REASON_MAX)),
                None => ".".into(),
            }
        ),
    };
    let title = if trigger.is_some() { "🚨 Raid detected: lockdown started" } else { "🔒 Lockdown started" };
    let action = settings.map(|s| s.anti_raid_action.as_str()).and_then(action_label).unwrap_or("Only alert the moderators");
    let mut embed = neutral_embed(Some(title), Some(&line), None)
        .colour(colors::ERROR)
        .field("Ends", format!("{} ({})", ts_of(state.locked_until, "R"), ts_of(state.locked_until, "t")), true)
        .field("New members", action, true)
        .field("Invites", invite_text(effects.map(|e| e.invites).unwrap_or("skipped")), true)
        .field("Chat", chat_text(effects.map(|e| e.chat).unwrap_or("skipped")), true)
        .footer(CreateEmbedFooter::new("Moderators can end the lockdown with the button."));
    let mut all: Vec<String> = Vec::new();
    for p in effects.map(|e| e.errors.clone()).unwrap_or_default().into_iter().chain(problems.iter().cloned()) {
        if !all.contains(&p) {
            all.push(p);
        }
    }
    if !all.is_empty() {
        embed = embed.field("Needs attention", clamp_text(&all.iter().map(|p| format!("• {p}")).collect::<Vec<_>>().join("\n"), 1024), false);
    }
    embed
}

/// The note posted when a lockdown ends. `how` is `expired` or `manual`.
pub fn build_end_embed(how: &str, ended_by_id: Option<&str>, result: Option<&Ended>) -> CreateEmbed {
    let mut lines = vec![if how == "expired" {
        "The lockdown ran out.".to_string()
    } else {
        format!("Ended by {}.", ended_by_id.map(|id| format!("<@{id}>")).unwrap_or_else(|| "a moderator".into()))
    }];
    if let Some(t) = result.and_then(|r| r.tally)
        && (t.kicked > 0 || t.banned > 0 || t.failed > 0 || t.flagged > 0)
    {
        let mut parts = Vec::new();
        if t.kicked > 0 {
            parts.push(format!("{} kicked", t.kicked));
        }
        if t.banned > 0 {
            parts.push(format!("{} banned", t.banned));
        }
        if t.flagged > 0 {
            parts.push(format!("{} reported", t.flagged));
        }
        if t.failed > 0 {
            parts.push(format!("{} could not be removed", t.failed));
        }
        lines.push(format!("Members who joined meanwhile: {}.", parts.join(", ")));
    }
    let pending = result.is_some_and(|r| r.pending_restore);
    let mut embed = neutral_embed(Some("🔓 Raid lockdown ended"), Some(&lines.join("\n")), None).colour(if pending { colors::WARNING } else { colors::SUCCESS });
    if let Some(e) = result.map(|r| &r.effects) {
        embed = embed.field("Invites", invite_text(e.invites), true).field("Chat", chat_text(e.chat), true);
        if !e.errors.is_empty() {
            embed = embed.field("Needs attention", clamp_text(&e.errors.iter().map(|p| format!("• {p}")).collect::<Vec<_>>().join("\n"), 1024), false);
        }
    }
    embed
}

/// The alert for new accounts.
pub fn build_young_embed(report: &YoungReport, more: u32) -> CreateEmbed {
    let done = match report.action.as_str() {
        "KICK" => "Kicked",
        "BAN" => "Banned",
        _ => "Only reported",
    };
    let s = |n: i64| if n == 1 { "" } else { "s" };
    let mut embed = neutral_embed(Some("🆕 New account joined"), Some(&format!("<@{}> ({})", report.user_id, clamp_text(&report.user_tag, 80))), None)
        .colour(colors::WARNING)
        .field("Account age", format!("{} day{} (minimum {})", report.age_days, s(report.age_days), report.min_days), true)
        .field("Action", done, true);
    if more > 0 {
        embed = embed.field("Since the last alert", format!("{more} more new account{}", if more == 1 { "" } else { "s" }), false);
    }
    embed
}

/// The alert channel, else the moderation log channel, when usable.
pub async fn alert_channel(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>) -> Option<ChannelId> {
    let s = settings?;
    for id in [s.anti_raid_alert_channel_id.as_deref(), s.mod_log_channel_id.as_deref()].into_iter().flatten() {
        let Some(cid) = id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new) else { continue };
        let Some(channel) = bot_core::interaction_helpers::resolve_channel(app, guild_id, cid).await else { continue };
        if check_channel_usable_for(app, guild_id, &channel, &["ViewChannel", "SendMessages", "EmbedLinks"]).ok {
            return Some(cid);
        }
    }
    None
}

/// Posts an alert in the alert channel (else the moderation log channel). Never pings; never fails. `None` when
/// nothing could be posted.
pub async fn post_alert(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, payload: CreateMessage) -> Option<(ChannelId, Message)> {
    let loaded;
    let s = match settings {
        Some(s) => Some(s),
        None => {
            loaded = app.settings().get_settings_safe(&guild_id.to_string()).await;
            loaded.as_deref()
        }
    };
    let Some(channel) = alert_channel(app, guild_id, s).await else {
        if no_alert(app, &guild_id.to_string()) {
            bot_util::log_warn!(format!("[antiraid] {guild_id}: no usable alert channel; set one with /antiraid config"));
        }
        return None;
    };
    let http = app.http()?;
    match channel.send_message(http.as_ref(), payload.allowed_mentions(CreateAllowedMentions::new())).await {
        Ok(m) => Some((channel, m)),
        Err(e) => {
            if !bot_core::discord::error_code(&e).is_some_and(|c| ALERT_GONE_CODES.contains(&c)) {
                bot_util::log_warn!(format!("[antiraid] {guild_id}: could not post an alert: {}", bot_core::discord::error_message(&e)));
            }
            None
        }
    }
}

/// Posts the alert of a started lockdown.
pub async fn announce_start(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, state: &AntiRaidState, effects: Option<&Effects>, trigger: Option<Trigger>) -> Option<(ChannelId, Message)> {
    let problems = permission_problems(app, guild_id, settings);
    let embed = build_start_embed(state, settings, effects, trigger, &problems);
    post_alert(app, guild_id, settings, CreateMessage::new().embed(embed).components(vec![end_button_row()])).await
}

/// How a lockdown end is announced.
#[derive(Debug, Clone, Copy)]
pub struct EndAnnounce<'a> {
    pub how: &'a str,
    pub ended_by_id: Option<&'a str>,
    /// Skipped in this channel, where a reply already says it.
    pub except_channel_id: Option<ChannelId>,
}

/// Posts the closing note of a lockdown.
pub async fn announce_end(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, result: &Ended, opts: EndAnnounce<'_>) -> Option<(ChannelId, Message)> {
    let loaded;
    let s = match settings {
        Some(s) => Some(s),
        None => {
            loaded = app.settings().get_settings_safe(&guild_id.to_string()).await;
            loaded.as_deref()
        }
    };
    if let Some(except) = opts.except_channel_id
        && alert_channel(app, guild_id, s).await == Some(except)
    {
        return None;
    }
    post_alert(app, guild_id, s, CreateMessage::new().embed(build_end_embed(opts.how, opts.ended_by_id, Some(result)))).await
}

/// Counts one new account for the throttled alert. Posts it when the throttle allows; the accounts seen in between
/// are counted into the next one. Never fails.
pub async fn report_young(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, report: YoungReport) -> Option<(ChannelId, Message)> {
    let g = guild_id.to_string();
    let m = mem(app);
    let pending = {
        let mut i = lock(&m);
        let entry = i.young_pending.entry(g.clone()).or_insert((0, report.clone()));
        entry.0 += 1;
        entry.1 = report;
        if !m.young_throttle.allow_at(&g, now_ms(app)) {
            return None;
        }
        i.young_pending.remove(&g)
    }?;
    post_alert(app, guild_id, settings, CreateMessage::new().embed(build_young_embed(&pending.1, pending.0 - 1))).await
}

// ── Acting on a member ────────────────────────────────────────────────────────

/// What [`remove_member`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removed {
    pub ok: bool,
    pub skipped: bool,
    pub code: Option<String>,
    pub error: Option<String>,
}

/// Kicks or bans a joiner through moderation (a case in the mod log, no DM, no escalation). Runs a few at a time.
/// Never fails.
pub async fn remove_member(app: &App, guild_id: GuildId, member: &Member, action: &str, reason: &str, detail: Option<&str>) -> Removed {
    let moderator = feat_moderation::moderation_service::bot_user(app).unwrap_or_else(|| feat_moderation::guild_ops::UserRef::id(""));
    let mut input = ModActionInput::new(guild_id.to_string(), member.user.id.to_string(), moderator, action);
    input.target_member = Some(member.clone());
    input.target_user = Some(member.user.clone());
    input.reason = Some(clamp_text(reason, raid::REASON_MAX));
    input.source = moderation_source().into();
    input.silent = true;
    input.skip_escalation = true;
    if action == "BAN" {
        input.delete_message_seconds = Some(raid::BAN_DELETE_SECONDS);
    }
    if let Some(d) = detail.filter(|d| !d.is_empty()) {
        input.log_fields = vec![("Anti-Raid".into(), clamp_text(d, 300))];
    }
    let m = mem(app);
    let a = app.clone();
    let res = m.action_slots.run(move || async move { crate::automod_service::execute_mod_action(&a, input).await }).await;
    match res {
        Ok(_) => Removed { ok: true, ..Default::default() },
        // `applied`: the member was removed but the case could not be saved. Removing them again would fail.
        Err(e) if e.applied => Removed { ok: true, ..Default::default() },
        Err(e) => {
            let code = e.code.as_str().to_string();
            // Staff, the owner, someone already gone or already banned: nothing to do and nothing wrong.
            if ["IMMUNE", "NOT_IN_GUILD", "ALREADY"].contains(&code.as_str()) {
                return Removed { ok: false, skipped: true, code: Some(code), error: None };
            }
            if no_alert(app, &format!("{guild_id}:remove")) {
                bot_util::log_warn!(format!("[antiraid] {guild_id}: could not {} {}: {}", bot_util::js::lower(action), member.user.id, e.error));
            }
            Removed { ok: false, skipped: false, code: Some(code), error: Some(e.error) }
        }
    }
}

/// `tallyOf(guildId)` mutation for the plugin.
pub(crate) fn add_to_tally(app: &App, guild_id: &str, f: impl FnOnce(&mut Tally)) {
    tally_add(app, guild_id, f);
}
