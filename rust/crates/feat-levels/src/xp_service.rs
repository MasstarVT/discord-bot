//! Port of `src/services/xpService.js`: XP maths, atomic XP writes, the level-change pipeline (announcements, reward
//! roles, the `levelUp` hook), voice XP sessions, bulk role tasks, the `xp` rewards provider and the hook handlers that
//! [`crate::plugin`] wires up. Settings are read through the settings service only; the cache never fails (a Redis
//! failure is decided fail-open or fail-closed at each call site).
//!
//! Other crates (the dashboard tasks, giveaways, achievements) call these functions directly. Functions that talk to
//! Discord take the [`App`] and plain ids, and degrade (log and skip) when Discord is not connected.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::adapters::{member_display_name, render_context};
use bot_core::custom_embed::{RenderOptions, render_template};
use bot_core::interaction_helpers::{Payload, cached_channel, cached_guild, check_assignable_role, check_channel_usable};
use bot_core::prelude::*;
use bot_core::rewards::RewardResult;
use bot_core::sync::KeyedMutex;
use bot_db::sqlx::postgres::{PgArguments, PgRow};
use bot_db::sqlx::query::Query;
use bot_db::sqlx::{self, Postgres, Row};
use bot_db::{LEVEL_ROLE_INFO, LevelRole, PgPool, SQL_NOW, SelectBuilder, USER_LEVEL_INFO, UserLevel};
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, ChannelId, GuildChannel, GuildId, Member, RoleId, UserId, VoiceState};
use serenity::builder::{CreateActionRow, CreateAllowedMentions, CreateButton};

// ── Constants ─────────────────────────────────────────────────────────────────

/// `DEFAULT_LEVELUP_MESSAGE`.
pub const DEFAULT_LEVELUP_MESSAGE: &str = "GG {player}, you just advanced to level {level}!";
/// `ANNOUNCE_MODES`.
pub const ANNOUNCE_MODES: [&str; 4] = ["OFF", "CURRENT", "DM", "CHANNEL"];
/// `ANNOUNCE_MODE_LABELS`, in the same order as [`ANNOUNCE_MODES`].
pub const ANNOUNCE_MODE_LABELS: [(&str, &str); 4] =
    [("OFF", "Disabled"), ("CURRENT", "Current channel"), ("DM", "Private message"), ("CHANNEL", "Custom channel")];
/// `XP_RANGE_MAX`.
pub const XP_RANGE_MAX: i64 = 1000;
/// `VOICE_KEY_TTL_SECONDS`: 26 h, outlives the 24 h segment cap.
pub const VOICE_KEY_TTL_SECONDS: i64 = 93_600;
/// `VOICE_SEGMENT_CAP_MINUTES`: a single unpaid stretch credits at most 24 h (R194).
pub const VOICE_SEGMENT_CAP_MINUTES: i64 = 1_440;
/// `VOICE_CHECKPOINT_MS`: pay out and rotate a session at most every 10 minutes.
pub const VOICE_CHECKPOINT_MS: i64 = 10 * 60_000;
/// `VOICE_CHANNEL_SCAN_CAP`: other members re-evaluated per channel per event.
pub const VOICE_CHANNEL_SCAN_CAP: usize = 50;
/// `ROLE_GIVE_MAX_MEMBERS`: `/levelconfig xp give target:@Role`.
pub const ROLE_GIVE_MAX_MEMBERS: usize = 5_000;
/// `BULK_SYNC_MAX_MEMBERS`: role add backfill / role sync.
pub const BULK_SYNC_MAX_MEMBERS: i64 = 5_000;
/// `BULK_SQL_CHUNK`.
pub const BULK_SQL_CHUNK: usize = 500;
/// `LEADERBOARD_MAX_ROWS`: 20 pages x 10.
pub const LEADERBOARD_MAX_ROWS: i64 = 200;

const DEFAULT_XP_MIN: i64 = 15;
const DEFAULT_XP_MAX: i64 = 25;
const VOICE_SEED_CAP: usize = 2_000;
const INT_MAX: i64 = 2_147_483_647;
const XP_MAX: i64 = bot_config::XP_MAX as i64;

/// `ANNOUNCE_MODE_LABELS[mode]`.
pub fn announce_mode_label(mode: &str) -> Option<&'static str> {
    ANNOUNCE_MODE_LABELS.iter().find(|(m, _)| *m == mode).map(|(_, l)| *l)
}

// ── In-process state (one per app) ────────────────────────────────────────────

/// The module-level state of xpService.js: the voice mutex, the warning throttle, the bulk locks, the sweep-seed
/// throttle, the deprecation flags, and the time this process became ready.
pub struct LevelsState {
    voice_mutex: KeyedMutex,
    warn_throttle: bot_util::async_util::Throttle,
    bulk_running: Mutex<HashSet<String>>,
    sweep_seed_throttle: bot_util::async_util::Throttle,
    deprecated_warned: Mutex<HashSet<&'static str>>,
    ready_at: Mutex<Option<i64>>,
}

impl Default for LevelsState {
    fn default() -> Self {
        Self {
            voice_mutex: KeyedMutex::default(),
            warn_throttle: bot_util::async_util::create_throttle(60 * 60_000),
            bulk_running: Mutex::new(HashSet::new()),
            sweep_seed_throttle: bot_util::async_util::create_throttle(5 * 60_000),
            deprecated_warned: Mutex::new(HashSet::new()),
            ready_at: Mutex::new(None),
        }
    }
}

impl LevelsState {
    /// Pending and running voice reconciliations (for tests).
    pub fn voice_queue_len(&self) -> usize {
        self.voice_mutex.len()
    }
}

fn state(app: &App) -> Arc<LevelsState> {
    app.ext::<LevelsState>()
}

/// Overrides the ready time (`client.readyTimestamp`) that [`ready_at`] reads. The bot does not need it: bot-core
/// records the ready time on the app ([`App::ready_at_ms`]); tests use it to pin the time.
pub fn set_ready_at(app: &App, ms: i64) {
    *state(app).ready_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(ms);
}

/// `guild.client.readyTimestamp ?? now`: an override from [`set_ready_at`], else the app's ready time
/// ([`App::ready_at_ms`]), else `now` (evaluated afresh on every call, as the JS does). Voice time before it is never
/// credited (R194).
pub fn ready_at(app: &App, now: i64) -> i64 {
    if let Some(ms) = *state(app).ready_at.lock().unwrap_or_else(|e| e.into_inner()) {
        return ms;
    }
    app.ready_at_ms().unwrap_or(now)
}

/// Logs a warning at most once per hour per key (a broken channel or role would otherwise flood the log).
fn warn_once(app: &App, key: &str, message: String) {
    if state(app).warn_throttle.allow(key) {
        bot_util::log_warn!(message);
    }
}

// ── JS value helpers ──────────────────────────────────────────────────────────

/// `Number(v)` of a JSON value.
pub fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = bot_util::js::trim(s);
            if t.is_empty() { 0.0 } else { js_string_to_number(t) }
        }
        Some(Value::Array(a)) => match a.len() {
            0 => 0.0,
            1 => js_to_number(Some(&match &a[0] {
                Value::Array(_) | Value::Object(_) => Value::String(String::new()),
                other => other.clone(),
            })),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

fn js_string_to_number(t: &str) -> f64 {
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let lower = t.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return u64::from_str_radix(rest, radix).map(|n| n as f64).unwrap_or(f64::NAN);
        }
    }
    if t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) && !lower.contains("inf") {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

fn js_trunc(v: f64) -> f64 {
    if v.is_finite() { v.trunc() } else { v }
}

/// `toInt(v, min, max)`: `Math.min(max, Math.max(min, Math.trunc(Number(v)) || 0))`.
pub fn to_int(v: f64, min: i64, max: i64) -> i64 {
    let t = js_trunc(v);
    let t = if t.is_nan() || t == 0.0 { 0.0 } else { t };
    t.max(min as f64).min(max as f64) as i64
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

fn xp_for(level: i64) -> f64 {
    bot_config::xp_for_level(level) as f64
}

/// `levelFromXp(totalXp)`: the level reached with `total_xp` accumulated XP. A non-finite or negative input gives 0.
pub fn level_from_xp(total_xp: f64) -> i64 {
    if !total_xp.is_finite() || total_xp < 0.0 {
        return 0;
    }
    let mut level = 0i64;
    let mut remaining = total_xp;
    let mut i = 0;
    while i < 2000 && remaining >= xp_for(level) {
        remaining -= xp_for(level);
        level += 1;
        i += 1;
    }
    level
}

/// `xpIntoLevel(totalXp)`: XP accumulated within the current level (0 to `xpForLevel(level) - 1`).
pub fn xp_into_level(total_xp: f64) -> f64 {
    if !total_xp.is_finite() || total_xp < 0.0 {
        return 0.0;
    }
    let mut remaining = total_xp;
    let mut level = 0;
    while level < 2000 && remaining >= xp_for(level) {
        remaining -= xp_for(level);
        level += 1;
    }
    remaining
}

/// `xpToNextLevel(level)`: XP needed to advance from `level` to the next one.
pub fn xp_to_next_level(level: i64) -> i64 {
    bot_config::xp_for_level(level)
}

/// `totalXpForLevel(level)`: cumulative XP needed to REACH `level` (`levelFromXp(totalXpForLevel(L)) == L`).
pub fn total_xp_for_level(level: f64) -> f64 {
    let l = js_trunc(level);
    if l.is_nan() || l <= 0.0 {
        return 0.0;
    }
    5.0 * (((l - 1.0) * l * (2.0 * l - 1.0)) / 6.0) + 25.0 * l * (l - 1.0) + 100.0 * l
}

fn to_range_value(v: Option<f64>, fallback: i64) -> i64 {
    match v {
        Some(n) if n.is_finite() => (n.trunc() as i64).clamp(1, XP_RANGE_MAX),
        _ => fallback,
    }
}

/// `normalizeXpRange(min, max)`: each value truncated and clamped to 1..[`XP_RANGE_MAX`] (a non-number, `None`,
/// becomes the default 15 / 25); the pair is swapped when min is above max. Returns `(min, max)`.
pub fn normalize_xp_range(min: Option<f64>, max: Option<f64>) -> (i64, i64) {
    let lo = to_range_value(min, DEFAULT_XP_MIN);
    let hi = to_range_value(max, DEFAULT_XP_MAX);
    if lo > hi { (hi, lo) } else { (lo, hi) }
}

fn sanitize_multiplier(m: f64) -> f64 {
    if m.is_finite() && m > 0.0 { m } else { 1.0 }
}

fn sanitize_boost(b: f64) -> f64 {
    if b.is_finite() && b >= 1.0 { b } else { 1.0 }
}

/// The inputs of [`roll_xp`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollXp {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub multiplier: f64,
    pub boost: f64,
}

impl RollXp {
    pub fn new(min: f64, max: f64) -> Self {
        Self { min: Some(min), max: Some(max), multiplier: 1.0, boost: 1.0 }
    }
}

/// `rollXp({ min, max, multiplier, boost }, rng)`: one random XP award, uniform in [min, max], times the server
/// multiplier and the member's boost, at least 1. `rng` returns a number in [0, 1) like `Math.random`.
pub fn roll_xp(opts: RollXp, mut rng: impl FnMut() -> f64) -> i64 {
    let (lo, hi) = normalize_xp_range(opts.min, opts.max);
    let base = lo as f64 + (rng() * (hi - lo + 1) as f64).floor();
    (base * sanitize_multiplier(opts.multiplier) * sanitize_boost(opts.boost)).floor().max(1.0) as i64
}

/// `Math.random()`.
pub fn math_random() -> f64 {
    rand::random::<f64>()
}

/// One entry of `GuildSettings.xpBoostRoles`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoostRole {
    #[serde(rename = "roleId")]
    pub role_id: String,
    pub multiplier: f64,
}

impl BoostRole {
    /// The stored JSON (`{ roleId, multiplier }`, numbers as `JSON.stringify` writes them).
    pub fn to_json(&self) -> Value {
        json!({ "roleId": self.role_id, "multiplier": bot_core::command::js_number(self.multiplier) })
    }
}

/// The stored JSON of a boost list.
pub fn boost_roles_json(list: &[BoostRole]) -> Value {
    Value::Array(list.iter().map(BoostRole::to_json).collect())
}

/// `SNOWFLAKE_RE` (`^\d{17,20}$`).
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `normalizeBoostRoles(json)`: the only reader of `GuildSettings.xpBoostRoles`. Valid entries only, deduplicated by
/// role (the first position, the last value: a JS `Map`), multipliers rounded to two decimals, truncated to
/// `LIMITS.XP_BOOST_ROLES`. Never mutates its input.
pub fn normalize_boost_roles(json: Option<&Value>) -> Vec<BoostRole> {
    let Some(Value::Array(entries)) = json else { return Vec::new() };
    let mut order: Vec<String> = Vec::new();
    let mut values: HashMap<String, f64> = HashMap::new();
    for entry in entries {
        let Value::Object(o) = entry else { continue };
        let Some(Value::String(role_id)) = o.get("roleId") else { continue };
        if !is_snowflake(role_id) {
            continue;
        }
        let Some(m) = o.get("multiplier").and_then(Value::as_f64) else { continue };
        if !m.is_finite() || !(1.0..=5.0).contains(&m) {
            continue;
        }
        if !values.contains_key(role_id) {
            order.push(role_id.clone());
        }
        values.insert(role_id.clone(), bot_util::text::js_round(m * 100.0) / 100.0);
    }
    order
        .into_iter()
        .take(bot_config::limits::XP_BOOST_ROLES)
        .map(|role_id| {
            let multiplier = values[&role_id];
            BoostRole { role_id, multiplier }
        })
        .collect()
}

/// `bestBoostMultiplier(boostRoles, hasRole)`: the highest multiplier among the boost roles a member holds (boosts
/// never stack), else 1.
pub fn best_boost_multiplier(boost_roles: &[BoostRole], has_role: impl Fn(&str) -> bool) -> f64 {
    let mut best = 1.0;
    for b in boost_roles {
        if b.multiplier > best && has_role(&b.role_id) {
            best = b.multiplier;
        }
    }
    best
}

/// `hasNoXpRole(noXpRoleIds, hasRole)`: true when the member holds any role on the no-XP list.
pub fn has_no_xp_role(no_xp_role_ids: &[String], has_role: impl Fn(&str) -> bool) -> bool {
    no_xp_role_ids.iter().any(|id| has_role(id))
}

/// The channel facts `isNoXpChannel` reads: its id, its parent (category or forum), and for a thread the parent's
/// category.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelRef {
    pub id: String,
    pub parent_id: Option<String>,
    pub is_thread: bool,
    /// `channel.parent?.parentId` (threads only).
    pub parent_parent_id: Option<String>,
}

/// `isNoXpChannel(noXpChannelIds, channel)`: true when the channel, its parent (category / forum) or, for threads,
/// the parent's category is on the no-XP list (R189).
pub fn is_no_xp_channel(no_xp_channel_ids: &[String], channel: Option<&ChannelRef>) -> bool {
    let Some(ch) = channel else { return false };
    if no_xp_channel_ids.is_empty() {
        return false;
    }
    let mut ids: Vec<Option<&str>> = vec![Some(ch.id.as_str()), ch.parent_id.as_deref()];
    if ch.is_thread {
        ids.push(ch.parent_parent_id.as_deref());
    }
    ids.into_iter().flatten().any(|id| !id.is_empty() && no_xp_channel_ids.iter().any(|x| x == id))
}

/// The channel facts of a cached guild channel (`message.channel`).
pub fn channel_ref(app: &App, guild_id: GuildId, channel: &GuildChannel) -> ChannelRef {
    let is_thread = bot_core::permissions::is_thread(channel);
    let parent_parent_id = if is_thread {
        channel.parent_id.and_then(|p| cached_channel(app, guild_id, p)).and_then(|p| p.parent_id).map(|p| p.to_string())
    } else {
        None
    };
    ChannelRef { id: channel.id.to_string(), parent_id: channel.parent_id.map(|p| p.to_string()), is_thread, parent_parent_id }
}

/// The inputs of [`voice_eligibility`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceInput {
    pub channel_id: Option<String>,
    pub parent_id: Option<String>,
    pub afk_channel_id: Option<String>,
    pub self_deaf: bool,
    pub server_deaf: bool,
    pub no_xp_channel_ids: Vec<String>,
    pub no_xp_role: bool,
    pub other_listeners: usize,
}

/// `{ eligible, reason }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eligibility {
    pub eligible: bool,
    pub reason: Option<&'static str>,
}

/// `voiceEligibility(...)`: whether a member earns voice XP right now. `reason` is the first failing check, in this
/// order: no-channel, afk, no-xp-channel, no-xp-role, deafened, alone. Self-mute and stage suppression do not matter.
pub fn voice_eligibility(i: &VoiceInput) -> Eligibility {
    let no = |reason| Eligibility { eligible: false, reason: Some(reason) };
    let Some(channel_id) = i.channel_id.as_deref().filter(|c| !c.is_empty()) else { return no("no-channel") };
    if i.afk_channel_id.as_deref().is_some_and(|a| !a.is_empty() && a == channel_id) {
        return no("afk");
    }
    let listed = |id: &str| i.no_xp_channel_ids.iter().any(|x| x == id);
    if listed(channel_id) || i.parent_id.as_deref().is_some_and(|p| !p.is_empty() && listed(p)) {
        return no("no-xp-channel");
    }
    if i.no_xp_role {
        return no("no-xp-role");
    }
    if i.self_deaf || i.server_deaf {
        return no("deafened");
    }
    if i.other_listeners < 1 {
        return no("alone");
    }
    Eligibility { eligible: true, reason: None }
}

/// A member in a voice channel, as `countOtherListeners` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub id: String,
    pub bot: bool,
    pub self_deaf: bool,
    pub server_deaf: bool,
}

/// `countOtherListeners(channel, selfId)`: non-bot members other than `self_id` who are not deafened.
pub fn count_other_listeners(members: &[Listener], self_id: Option<&str>) -> usize {
    members.iter().filter(|m| Some(m.id.as_str()) != self_id && !m.bot && !m.self_deaf && !m.server_deaf).count()
}

/// A stored voice session.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceSession {
    pub start: f64,
    pub channel_id: Option<String>,
}

/// `parseVoiceSession(raw, now)`: `{ start, channelId }`, or a legacy number / numeric string; `None` for anything
/// else, a start at or below 0, or one more than 60 s in the future.
pub fn parse_voice_session(raw: Option<&Value>, now: f64) -> Option<VoiceSession> {
    let mut channel_id = None;
    let start = match raw? {
        Value::Object(o) => {
            if let Some(Value::String(c)) = o.get("channelId")
                && !c.is_empty()
            {
                channel_id = Some(c.clone());
            }
            js_to_number(o.get("start"))
        }
        Value::Array(_) => f64::NAN, // `Number(array.start)`
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = bot_util::js::trim(s);
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
                t.parse::<f64>().unwrap_or(f64::NAN)
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if !start.is_finite() || start <= 0.0 || start > now + 60_000.0 {
        return None;
    }
    Some(VoiceSession { start, channel_id })
}

/// The stored session value (`{ start, channelId }`).
pub fn voice_session_json(start: f64, channel_id: &str) -> Value {
    json!({ "start": bot_core::command::js_number(start), "channelId": channel_id })
}

/// `{ minutes, nextStart }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub minutes: i64,
    pub next_start: f64,
}

/// `computeVoiceSegment({ start, now, readyAt, capMinutes })`: the paid part of a voice stretch. Time before
/// `ready_at` is never credited, one stretch pays at most `cap_minutes` (the excess is dropped), and the sub-minute
/// remainder is carried in `next_start`.
pub fn compute_voice_segment(start: f64, now: f64, ready_at: f64, cap_minutes: i64) -> Segment {
    let effective = now.min(start.max(ready_at));
    let raw = ((now - effective) / 60_000.0).floor();
    if raw >= cap_minutes as f64 {
        return Segment { minutes: cap_minutes, next_start: now };
    }
    Segment { minutes: raw as i64, next_start: effective + raw * 60_000.0 }
}

/// `voiceXpForMinutes(minutes, { rate, multiplier, boost })`: rate (1-50, default 5) per minute times the multiplier
/// and the boost, at least 1; 0 below one minute.
pub fn voice_xp_for_minutes(minutes: f64, rate: Option<f64>, multiplier: f64, boost: f64) -> i64 {
    if minutes.is_nan() || minutes < 1.0 {
        return 0;
    }
    let r = match rate {
        Some(r) if r.is_finite() => r.clamp(1.0, 50.0),
        _ => 5.0,
    };
    (minutes * r * sanitize_multiplier(multiplier) * sanitize_boost(boost)).floor().max(1.0) as i64
}

/// `{ add, remove }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolePlan {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

/// `planLevelRoles({ rewards, level, stack, syncDown, memberRoleIds })`: which reward roles to add and remove for a
/// member at `level`. `rewards` is `(level, roleId)`. The caller applies every add before any remove (R060).
pub fn plan_level_roles(rewards: &[(i64, String)], level: i64, stack: bool, sync_down: bool, holds: impl Fn(&str) -> bool) -> RolePlan {
    let mut sorted: Vec<&(i64, String)> = rewards.iter().collect();
    sorted.sort_by_key(|r| r.0);
    let mut earned: Vec<String> = Vec::new();
    let mut unearned: Vec<String> = Vec::new();
    let mut top: Option<String> = None;
    for (l, id) in &sorted {
        if *l <= level {
            if !earned.contains(id) {
                earned.push(id.clone());
            }
            top = Some(id.clone());
        }
    }
    for (_, id) in &sorted {
        if !earned.contains(id) && !unearned.contains(id) {
            unearned.push(id.clone());
        }
    }
    let mut add: Vec<String> = Vec::new();
    let mut remove: Vec<String> = Vec::new();
    if stack {
        add.extend(earned.iter().filter(|id| !holds(id)).cloned());
    } else if let Some(top) = &top {
        if !holds(top) {
            add.push(top.clone());
        }
        remove.extend(earned.iter().filter(|id| *id != top && holds(id)).cloned());
    }
    if sync_down {
        remove.extend(unearned.iter().filter(|id| holds(id)).cloned());
    }
    let dedupe = |v: Vec<String>| {
        let mut out: Vec<String> = Vec::new();
        for x in v {
            if !out.contains(&x) {
                out.push(x);
            }
        }
        out
    };
    let add = dedupe(add);
    let remove = dedupe(remove).into_iter().filter(|id| !add.contains(id)).collect();
    RolePlan { add, remove }
}

/// Where a level-up goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceTarget {
    None,
    Dm,
    Channel { channel_id: String, fallback: bool },
}

/// `resolveAnnounceTarget({ mode, source, levelUpChannelId, sourceChannelId })` (the §7.3 matrix). Admin XP changes
/// never announce; an unknown mode counts as CURRENT.
pub fn resolve_announce_target(mode: Option<&str>, source: &str, level_up_channel_id: Option<&str>, source_channel_id: Option<&str>) -> AnnounceTarget {
    if source == "admin" {
        return AnnounceTarget::None;
    }
    let m = mode.filter(|m| ANNOUNCE_MODES.contains(m)).unwrap_or("CURRENT");
    let src = source_channel_id.filter(|s| !s.is_empty());
    match m {
        "OFF" => AnnounceTarget::None,
        "DM" => AnnounceTarget::Dm,
        "CURRENT" => src.map_or(AnnounceTarget::None, |s| AnnounceTarget::Channel { channel_id: s.into(), fallback: false }),
        _ => match level_up_channel_id.filter(|c| !c.is_empty()) {
            Some(c) => AnnounceTarget::Channel { channel_id: c.into(), fallback: false },
            None => src.map_or(AnnounceTarget::None, |s| AnnounceTarget::Channel { channel_id: s.into(), fallback: true }),
        },
    }
}

/// `templateMentionsPlayer(template)`: true when a level-up template pings the member (`{player}`, `{mention}`,
/// `{user}` or `{user.mention}`, any case), so embed mode must add the mention as message content.
pub fn template_mentions_player(template: &str) -> bool {
    let t = template.to_ascii_lowercase();
    ["{player}", "{mention}", "{user}", "{user.mention}"].iter().any(|tok| t.contains(tok))
}

/// `/\{rank\}/i.test(template)`.
pub fn template_uses_rank(template: &str) -> bool {
    template.to_ascii_lowercase().contains("{rank}")
}

/// `levelUpVars({ user, member, level, xp, rank })`: the level-up placeholder variables. `name` is
/// `member.displayName ?? user.globalName ?? user.username`; keys are lower-case.
pub fn level_up_vars(user_id: &str, name: &str, level: i64, xp: f64, rank: Option<i64>) -> Vec<(String, VarValue)> {
    vec![
        ("player".into(), VarValue::from(format!("<@{user_id}>"))),
        ("player.name".into(), VarValue::from(bot_core::markdown::escape_markdown(name))),
        ("level".into(), VarValue::from(level.to_string())),
        ("rank".into(), VarValue::from(rank.map_or_else(|| "?".to_string(), |r| r.to_string()))),
        ("xp".into(), VarValue::from(bot_util::text::format_number(xp))),
        ("mention".into(), VarValue::from(format!("<@{user_id}>"))),
    ]
}

/// The inputs of [`build_level_up_payload`].
#[derive(Debug)]
pub struct LevelUpInput<'a> {
    pub template: &'a str,
    pub use_embed: bool,
    /// The member, user, guild and channel of the render context (its `vars` are set here).
    pub ctx: RenderContext,
    pub user_id: &'a str,
    /// `member?.displayName ?? user.globalName ?? user.username ?? ''`.
    pub name: &'a str,
    pub guild_name: &'a str,
    pub level: i64,
    pub xp: f64,
    pub rank: Option<i64>,
    pub embed_color: Option<i64>,
    pub dm: bool,
}

/// A level-up message payload.
#[derive(Debug, Clone)]
pub struct LevelUpPayload {
    pub content: Option<String>,
    /// Embed JSON (zero or one).
    pub embeds: Vec<Value>,
    pub components: Vec<CreateActionRow>,
    /// `allowedMentions: { users: [id] }`.
    pub mention_user: String,
    /// Text mode (no `embeds` / `components` keys unless set).
    pub text_mode: bool,
}

impl LevelUpPayload {
    fn mentions(&self) -> CreateAllowedMentions {
        let ids: Vec<UserId> = self.mention_user.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new).into_iter().collect();
        CreateAllowedMentions::new().users(ids)
    }

    /// The message payload.
    pub fn to_payload(&self) -> Payload {
        let mut p = Payload::new().allowed_mentions(self.mentions());
        if let Some(c) = &self.content {
            p = p.content(c.clone());
        }
        if !self.text_mode {
            p = p.embeds(self.embeds.iter().map(bot_core::custom_embed::embed_from_json).collect());
            p = p.components(self.components.clone());
        } else if !self.components.is_empty() {
            p = p.components(self.components.clone());
        }
        p
    }

    /// The JSON the JS payload serialises to (`{ content?, embeds?, components?, allowedMentions }`).
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        if let Some(c) = &self.content {
            m.insert("content".into(), json!(c));
        }
        if !self.text_mode {
            m.insert("embeds".into(), Value::Array(self.embeds.clone()));
        }
        if !self.text_mode || !self.components.is_empty() {
            m.insert("components".into(), Value::Array(self.components.iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect()));
        }
        m.insert("allowedMentions".into(), json!({ "users": [self.mention_user] }));
        Value::Object(m)
    }
}

/// The disabled "Sent from" button a DM level-up carries.
pub fn sent_from_row(guild_name: &str) -> CreateActionRow {
    let id = bot_config::custom_id!("noop", "levelup").unwrap_or_else(|_| "noop:levelup".into());
    CreateActionRow::Buttons(vec![
        CreateButton::new(id)
            .label(bot_util::text::clamp_text(&format!("Sent from {guild_name}"), 80))
            .style(ButtonStyle::Secondary)
            .disabled(true),
    ])
}

/// `buildLevelUpPayload(...)`: the level-up message. Every token is replaced in a single pass by the shared renderer
/// (R190); the ping is scoped to the levelled member only; in embed mode the mention goes in `content`, because a
/// mention inside an embed never notifies (R298). `None` when nothing visible remains.
pub fn build_level_up_payload(input: LevelUpInput<'_>) -> Option<LevelUpPayload> {
    let mut ctx = input.ctx;
    ctx.vars = Some(level_up_vars(input.user_id, input.name, input.level, input.xp, input.rank));
    let row = input.dm.then(|| sent_from_row(input.guild_name));
    if !input.use_embed {
        let content = render_placeholders(input.template, &ctx, Some(2000));
        if bot_util::js::trim(&content).is_empty() {
            return None;
        }
        return Some(LevelUpPayload {
            content: Some(content),
            embeds: Vec::new(),
            components: row.into_iter().collect(),
            mention_user: input.user_id.to_string(),
            text_mode: true,
        });
    }
    if bot_util::js::trim(&render_placeholders(input.template, &ctx, Some(4096))).is_empty() {
        return None;
    }
    let tpl = json!({
        "content": if template_mentions_player(input.template) { json!(format!("<@{}>", input.user_id)) } else { Value::Null },
        "embed": { "description": input.template, "thumbnail": "{user.avatar}" },
        "buttons": [],
    });
    let ids: Vec<UserId> = input.user_id.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new).into_iter().collect();
    let opts = RenderOptions {
        default_color: Some(input.embed_color.and_then(|c| u32::try_from(c).ok()).unwrap_or(bot_config::colors::SUCCESS)),
        allowed_mentions: Some(CreateAllowedMentions::new().users(ids)),
        extra_components: row.into_iter().collect(),
    };
    let r = render_template(&tpl, &ctx, opts)?;
    if r.content.is_none() && r.embeds.is_empty() {
        return None;
    }
    let components = r.components();
    Some(LevelUpPayload { content: r.content, embeds: r.embeds, components, mention_user: input.user_id.to_string(), text_mode: false })
}

/// `rankWhere(guildId, userId, xp)`: the single tie rule shared by `{rank}`, `/rank`, `/levelconfig xp view` and the
/// leaderboard (which orders by xp desc, userId asc): the rows ranked ahead of this member, as the Prisma `where`.
pub fn rank_where(guild_id: &str, user_id: &str, xp: i64) -> Value {
    json!({ "guildId": guild_id, "OR": [{ "xp": { "gt": xp } }, { "xp": xp, "userId": { "lt": user_id } }] })
}

// ── Raw SQL (every timestamp is SQL_NOW) ───────────────────────────────────────

type PgQuery = Query<'static, Postgres, PgArguments>;

fn q(sql: String) -> PgQuery {
    sqlx::query(sqlx::AssertSqlSafe(sql))
}

/// The `levels.applyXpDelta` statement.
pub fn apply_delta_sql() -> String {
    format!(
        r#"INSERT INTO "UserLevel" ("id","guildId","userId","xp","level","messageCount","voiceMinutes","lastXpAt","createdAt","updatedAt")
  VALUES ($1, $2, $3, LEAST($4::bigint, $5::bigint)::int, 0, $6, $7,
          CASE WHEN $8::boolean THEN {now} ELSE NULL END, {now}, {now})
  ON CONFLICT ("guildId","userId") DO UPDATE SET
    "xp"           = LEAST("UserLevel"."xp"::bigint + $9::bigint, $10::bigint)::int,
    "messageCount" = LEAST("UserLevel"."messageCount"::bigint + $11::bigint, $12::bigint)::int,
    "voiceMinutes" = LEAST("UserLevel"."voiceMinutes"::bigint + $13::bigint, $14::bigint)::int,
    "lastXpAt"     = CASE WHEN $15::boolean THEN {now} ELSE "UserLevel"."lastXpAt" END,
    "updatedAt"    = {now}
  RETURNING "xp", "level", (xmax = 0) AS "created""#,
        now = SQL_NOW
    )
}

/// The parameters of `levels.applyXpDelta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyDelta {
    pub id: String,
    pub guild_id: String,
    pub user_id: String,
    pub delta: i64,
    pub messages: i64,
    pub voice_minutes: i64,
    pub touch_last_xp: bool,
}

fn apply_delta_query(p: &ApplyDelta) -> PgQuery {
    let int = |v: i64| v.clamp(0, INT_MAX) as i32;
    q(apply_delta_sql())
        .bind(p.id.clone())
        .bind(p.guild_id.clone())
        .bind(p.user_id.clone())
        .bind(p.delta)
        .bind(XP_MAX)
        .bind(int(p.messages))
        .bind(int(p.voice_minutes))
        .bind(p.touch_last_xp)
        .bind(p.delta)
        .bind(XP_MAX)
        .bind(p.messages)
        .bind(INT_MAX)
        .bind(p.voice_minutes)
        .bind(INT_MAX)
        .bind(p.touch_last_xp)
}

/// The `levels.subtractXp` statement.
pub fn subtract_sql() -> String {
    format!(
        r#"UPDATE "UserLevel" AS t SET "xp" = GREATEST(t."xp"::bigint - $1::bigint, 0)::int, "updatedAt" = {SQL_NOW}
  FROM (SELECT "id", "xp" AS "oldXp" FROM "UserLevel" WHERE "guildId" = $2 AND "userId" = $3 FOR UPDATE) AS o
  WHERE t."id" = o."id"
  RETURNING t."xp", t."level", o."oldXp""#
    )
}

fn subtract_query(guild_id: &str, user_id: &str, amount: i64) -> PgQuery {
    q(subtract_sql()).bind(amount).bind(guild_id.to_string()).bind(user_id.to_string())
}

/// The `levels.setXpZero` statement.
pub fn set_zero_sql() -> String {
    format!(
        r#"UPDATE "UserLevel" SET "xp" = 0, "updatedAt" = {SQL_NOW}
  WHERE "guildId" = $1 AND "userId" = $2
  RETURNING "xp", "level""#
    )
}

fn set_zero_query(guild_id: &str, user_id: &str) -> PgQuery {
    q(set_zero_sql()).bind(guild_id.to_string()).bind(user_id.to_string())
}

/// The `levels.setXp` statement.
pub fn set_xp_sql() -> String {
    format!(
        r#"INSERT INTO "UserLevel" ("id","guildId","userId","xp","level","messageCount","voiceMinutes","createdAt","updatedAt")
  VALUES ($1, $2, $3, LEAST($4::bigint, $5::bigint)::int, 0, 0, 0, {now}, {now})
  ON CONFLICT ("guildId","userId") DO UPDATE SET "xp" = EXCLUDED."xp", "updatedAt" = {now}
  RETURNING "xp", "level", (xmax = 0) AS "created""#,
        now = SQL_NOW
    )
}

fn set_xp_query(id: &str, guild_id: &str, user_id: &str, xp: i64) -> PgQuery {
    q(set_xp_sql()).bind(id.to_string()).bind(guild_id.to_string()).bind(user_id.to_string()).bind(xp).bind(XP_MAX)
}

/// The `levels.resetXp` statement.
pub fn reset_sql() -> String {
    r#"DELETE FROM "UserLevel" WHERE "guildId" = $1 AND "userId" = $2 RETURNING "level""#.to_string()
}

fn reset_query(guild_id: &str, user_id: &str) -> PgQuery {
    q(reset_sql()).bind(guild_id.to_string()).bind(user_id.to_string())
}

/// The `levels.setStoredLevel` statement.
pub fn stored_level_sql() -> String {
    format!(
        r#"UPDATE "UserLevel" SET "level" = $1, "updatedAt" = {SQL_NOW}
  WHERE "guildId" = $2 AND "userId" = $3 AND "xp" = $4 AND "level" <> $5"#
    )
}

fn stored_level_query(guild_id: &str, user_id: &str, xp_after: i64, new_level: i64) -> PgQuery {
    let int = |v: i64| v.clamp(i64::from(i32::MIN), INT_MAX) as i32;
    q(stored_level_sql()).bind(int(new_level)).bind(guild_id.to_string()).bind(user_id.to_string()).bind(int(xp_after)).bind(int(new_level))
}

/// The `levels.bulkGive` statement.
pub fn bulk_give_sql() -> String {
    format!(
        r#"INSERT INTO "UserLevel" ("id","guildId","userId","xp","level","messageCount","voiceMinutes","createdAt","updatedAt")
  SELECT u."id", $1, u."userId", LEAST($2::bigint, $3::bigint)::int, 0, 0, 0, {now}, {now}
  FROM unnest($4::text[], $5::text[]) AS u("id","userId")
  ON CONFLICT ("guildId","userId") DO UPDATE SET
    "xp" = LEAST("UserLevel"."xp"::bigint + $6::bigint, $7::bigint)::int, "updatedAt" = {now}
  RETURNING "userId", "xp", "level""#,
        now = SQL_NOW
    )
}

fn bulk_give_query(guild_id: &str, amount: i64, ids: Vec<String>, user_ids: Vec<String>) -> PgQuery {
    q(bulk_give_sql()).bind(guild_id.to_string()).bind(amount).bind(XP_MAX).bind(ids).bind(user_ids).bind(amount).bind(XP_MAX)
}

/// The `levels.bulkLevel` statement.
pub fn bulk_level_sql() -> String {
    format!(
        r#"UPDATE "UserLevel" AS t SET "level" = v."level", "updatedAt" = {SQL_NOW}
  FROM unnest($1::text[], $2::int[], $3::int[]) AS v("userId","xp","level")
  WHERE t."guildId" = $4 AND t."userId" = v."userId" AND t."xp" = v."xp" AND t."level" <> v."level"
  RETURNING t."userId""#
    )
}

fn bulk_level_query(guild_id: &str, user_ids: Vec<String>, xps: Vec<i32>, levels: Vec<i32>) -> PgQuery {
    q(bulk_level_sql()).bind(user_ids).bind(xps).bind(levels).bind(guild_id.to_string())
}

/// The rank count (`prisma.userLevel.count({ where: rankWhere(...) })`).
pub fn rank_sql() -> String {
    r#"SELECT COUNT(*)::bigint AS "n" FROM "UserLevel" WHERE "guildId" = $1 AND ("xp" > $2 OR ("xp" = $2 AND "userId" < $3))"#.to_string()
}

/// `RAW_SQL`: one entry per raw statement with sample parameters (run each inside a rolled-back transaction).
pub fn raw_sql() -> Vec<(&'static str, PgQuery)> {
    vec![
        (
            "levels.applyXpDelta",
            apply_delta_query(&ApplyDelta {
                id: "sample".into(),
                guild_id: "0".into(),
                user_id: "0".into(),
                delta: 1,
                messages: 1,
                voice_minutes: 0,
                touch_last_xp: true,
            }),
        ),
        ("levels.subtractXp", subtract_query("0", "0", 1)),
        ("levels.setXpZero", set_zero_query("0", "0")),
        ("levels.setXp", set_xp_query("sample", "0", "0", 1)),
        ("levels.resetXp", reset_query("0", "0")),
        ("levels.setStoredLevel", stored_level_query("0", "0", 1, 1)),
        ("levels.bulkGive", bulk_give_query("0", 1, vec!["sample".into()], vec!["0".into()])),
        ("levels.bulkLevel", bulk_level_query("0", vec!["0".into()], vec![1], vec![1])),
    ]
}

// ── Database operations ───────────────────────────────────────────────────────

/// The result of an XP write: `{ xp, oldLevel, newLevel, created }` (`old_xp` only from [`subtract_xp`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XpChange {
    pub xp: i64,
    pub old_level: i64,
    pub new_level: i64,
    pub created: bool,
    pub old_xp: Option<i64>,
}

fn int_col(row: &PgRow, col: &str) -> anyhow::Result<i64> {
    Ok(i64::from(row.try_get::<i32, _>(col)?))
}

/// The options of [`apply_xp_delta`].
#[derive(Debug, Clone, PartialEq)]
pub struct XpDelta {
    pub guild_id: String,
    pub user_id: String,
    pub delta: f64,
    pub messages: f64,
    pub voice_minutes: f64,
    pub touch_last_xp: bool,
}

impl XpDelta {
    pub fn new(guild_id: impl Into<String>, user_id: impl Into<String>, delta: f64) -> Self {
        Self { guild_id: guild_id.into(), user_id: user_id.into(), delta, messages: 0.0, voice_minutes: 0.0, touch_last_xp: false }
    }
}

/// `applyXpDelta(...)`: atomically adds XP (clamped to `XP_MAX` in SQL). The stored `level` column is NOT touched
/// here; the writer that wins [`set_stored_level`] owns the transition.
pub async fn apply_xp_delta(app: &App, d: XpDelta) -> anyhow::Result<XpChange> {
    app.settings().ensure_guild_settings(&d.guild_id).await?;
    let p = ApplyDelta {
        id: bot_db::new_uuid(),
        guild_id: d.guild_id,
        user_id: d.user_id,
        delta: to_int(d.delta, 1, XP_MAX),
        messages: to_int(d.messages, 0, INT_MAX),
        voice_minutes: to_int(d.voice_minutes, 0, INT_MAX),
        touch_last_xp: d.touch_last_xp,
    };
    let row = apply_delta_query(&p).fetch_one(app.db()).await?;
    let xp = int_col(&row, "xp")?;
    Ok(XpChange { xp, old_level: int_col(&row, "level")?, new_level: level_from_xp(xp as f64), created: row.try_get::<bool, _>("created")?, old_xp: None })
}

/// `subtractXp({ guildId, userId, amount })`: atomically removes XP (floored at 0). Never inserts a row (R300).
/// `None` when the member has no row.
pub async fn subtract_xp(app: &App, guild_id: &str, user_id: &str, amount: f64) -> anyhow::Result<Option<XpChange>> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let Some(row) = subtract_query(guild_id, user_id, to_int(amount, 1, XP_MAX)).fetch_optional(app.db()).await? else { return Ok(None) };
    let xp = int_col(&row, "xp")?;
    Ok(Some(XpChange {
        xp,
        old_level: int_col(&row, "level")?,
        new_level: level_from_xp(xp as f64),
        created: false,
        old_xp: Some(int_col(&row, "oldXp")?),
    }))
}

/// `setXp({ guildId, userId, xp })`: sets total XP (clamped to 0..XP_MAX). Setting 0 never creates a row (`None`
/// when there was none).
pub async fn set_xp(app: &App, guild_id: &str, user_id: &str, xp: f64) -> anyhow::Result<Option<XpChange>> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let value = to_int(xp, 0, XP_MAX);
    if value == 0 {
        let row = set_zero_query(guild_id, user_id).fetch_optional(app.db()).await?;
        return match row {
            Some(r) => Ok(Some(XpChange { xp: 0, old_level: int_col(&r, "level")?, new_level: 0, created: false, old_xp: None })),
            None => Ok(None),
        };
    }
    let row = set_xp_query(&bot_db::new_uuid(), guild_id, user_id, value).fetch_one(app.db()).await?;
    let xp = int_col(&row, "xp")?;
    Ok(Some(XpChange { xp, old_level: int_col(&row, "level")?, new_level: level_from_xp(xp as f64), created: row.try_get::<bool, _>("created")?, old_xp: None }))
}

/// `resetXp(guildId, userId)`: deletes a member's XP row; the old stored level, `None` when there was no row.
pub async fn reset_xp(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<i64>> {
    let row = reset_query(guild_id, user_id).fetch_optional(pool).await?;
    row.map(|r| int_col(&r, "level")).transpose()
}

/// `setStoredLevel(guildId, userId, xpAfter, newLevel)`: writes the stored level only while the XP is still the value
/// the caller saw, so a superseded writer never writes a stale level. True for exactly one writer per transition.
pub async fn set_stored_level(pool: &PgPool, guild_id: &str, user_id: &str, xp_after: i64, new_level: i64) -> anyhow::Result<bool> {
    let r = stored_level_query(guild_id, user_id, xp_after, new_level).execute(pool).await?;
    Ok(r.rows_affected() == 1)
}

/// One member who levelled up in a bulk give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkLevelUp {
    pub user_id: String,
    pub old_level: i64,
    pub new_level: i64,
    pub xp: i64,
}

/// `{ updated, levelUps }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BulkGive {
    pub updated: usize,
    pub level_ups: Vec<BulkLevelUp>,
}

/// `giveXpToMembers({ guildId, userIds, amount })`: gives the same XP to many members in chunked statements.
pub async fn give_xp_to_members(app: &App, guild_id: &str, user_ids: &[String], amount: f64) -> anyhow::Result<BulkGive> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let mut unique: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for u in user_ids {
        if seen.insert(u.clone()) {
            unique.push(u.clone());
        }
    }
    let give = to_int(amount, 1, XP_MAX);
    let mut out = BulkGive::default();
    for chunk in unique.chunks(BULK_SQL_CHUNK) {
        let ids: Vec<String> = chunk.iter().map(|_| bot_db::new_uuid()).collect();
        let rows = bulk_give_query(guild_id, give, ids, chunk.to_vec()).fetch_all(app.db()).await?;
        out.updated += rows.len();
        let mut stale: Vec<(String, i64, i64)> = Vec::new();
        for r in &rows {
            let (u, xp, level): (String, i64, i64) = (r.try_get("userId")?, int_col(r, "xp")?, int_col(r, "level")?);
            if level_from_xp(xp as f64) != level {
                stale.push((u, xp, level));
            }
        }
        if stale.is_empty() {
            continue;
        }
        let owned = bulk_level_query(
            guild_id,
            stale.iter().map(|s| s.0.clone()).collect(),
            stale.iter().map(|s| s.1 as i32).collect(),
            stale.iter().map(|s| level_from_xp(s.1 as f64) as i32).collect(),
        )
        .fetch_all(app.db())
        .await?;
        let won: HashSet<String> = owned.iter().filter_map(|r| r.try_get::<String, _>("userId").ok()).collect();
        for (u, xp, level) in stale {
            let new_level = level_from_xp(xp as f64);
            if won.contains(&u) && new_level > level {
                out.level_ups.push(BulkLevelUp { user_id: u, old_level: level, new_level, xp });
            }
        }
    }
    Ok(out)
}

/// `getRank(guildId, userId, xp)`: the 1-based leaderboard position (xp desc, userId asc).
pub async fn get_rank(pool: &PgPool, guild_id: &str, user_id: &str, xp: i64) -> anyhow::Result<i64> {
    let xp = xp.clamp(i64::from(i32::MIN), INT_MAX) as i32;
    let row = q(rank_sql()).bind(guild_id.to_string()).bind(xp).bind(user_id.to_string()).fetch_one(pool).await?;
    Ok(row.try_get::<i64, _>("n")? + 1)
}

/// `prisma.userLevel.findUnique({ where: { guildId_userId } })`.
pub async fn find_user_level(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<UserLevel>> {
    let q = SelectBuilder::new(&USER_LEVEL_INFO).filter("guildId", json!(guild_id))?.filter("userId", json!(user_id))?.build()?;
    Ok(q.fetch_optional::<UserLevel, _>(pool).await?)
}

/// `prisma.levelRole.findMany({ where: { guildId }, orderBy: { level: 'asc' } })`.
pub async fn level_roles(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<LevelRole>> {
    let q = SelectBuilder::new(&LEVEL_ROLE_INFO).filter("guildId", json!(guild_id))?.order_by("level", true)?.build()?;
    Ok(q.fetch_all::<LevelRole, _>(pool).await?)
}

/// The `(level, roleId)` pairs of reward rows.
pub fn reward_pairs(rows: &[LevelRole]) -> Vec<(i64, String)> {
    rows.iter().map(|r| (i64::from(r.level), r.role_id.clone())).collect()
}

/// `{ userId, xp }` of the ranked rows (xp desc, userId asc), with an optional minimum XP: the leaderboard and the
/// bulk role tasks.
pub async fn ranked_rows(pool: &PgPool, guild_id: &str, min_xp_exclusive: Option<i64>, min_xp_inclusive: Option<i64>, take: i64) -> anyhow::Result<Vec<(String, i64)>> {
    let mut sql = r#"SELECT "userId", "xp" FROM "UserLevel" WHERE "guildId" = $1"#.to_string();
    if min_xp_exclusive.is_some() {
        sql.push_str(r#" AND "xp" > $2"#);
    } else if min_xp_inclusive.is_some() {
        sql.push_str(r#" AND "xp" >= $2"#);
    }
    sql.push_str(r#" ORDER BY "xp" DESC, "userId" ASC LIMIT "#);
    sql.push_str(&take.max(0).to_string());
    let mut query = q(sql).bind(guild_id.to_string());
    if let Some(min) = min_xp_exclusive.or(min_xp_inclusive) {
        query = query.bind(min.clamp(i64::from(i32::MIN), INT_MAX) as i32);
    }
    let rows = query.fetch_all(pool).await?;
    rows.iter().map(|r| Ok((r.try_get::<String, _>("userId")?, int_col(r, "xp")?))).collect()
}

/// `prisma.userLevel.count({ where: { guildId, xp: { gt: 0 } } })`.
pub async fn ranked_count(pool: &PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let row = q(r#"SELECT COUNT(*)::bigint AS "n" FROM "UserLevel" WHERE "guildId" = $1 AND "xp" > 0"#.to_string())
        .bind(guild_id.to_string())
        .fetch_one(pool)
        .await?;
    Ok(row.try_get::<i64, _>("n")?)
}

// ── Discord helpers ───────────────────────────────────────────────────────────

fn parse_snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `guild.members.cache.get(id)`.
pub fn cached_member(app: &App, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    app.discord()?.cache.guild(guild_id)?.members.get(&user_id).cloned()
}

/// `guild.members.fetch(userId).catch(() => null)`: the cached member, else a bounded REST fetch.
pub async fn fetch_member(app: &App, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    if let Some(m) = cached_member(app, guild_id, user_id) {
        return Some(m);
    }
    let http = app.http()?;
    bot_util::async_util::with_timeout(async move { http.get_member(guild_id, user_id).await }, 10_000, "levels member fetch").await.ok()
}

/// `withTimeout(guild.members.fetch({ user: ids }), timeout_ms).catch(() => null)` then `guild.members.cache.get(id)`
/// for each id: the cached members plus those the gateway member request (`REQUEST_GUILD_MEMBERS`, one per 100 ids)
/// returned within `timeout_ms`. Never fails; the missing ones are left out.
pub async fn fetch_members(app: &App, guild_id: GuildId, ids: &[String], timeout_ms: u64) -> HashMap<String, Member> {
    let mut out = HashMap::new();
    let mut missing: Vec<UserId> = Vec::new();
    for id in ids {
        let Some(u) = parse_snowflake(id).map(UserId::new) else { continue };
        match cached_member(app, guild_id, u) {
            Some(m) => {
                out.insert(id.clone(), m);
            }
            None => missing.push(u),
        }
    }
    if missing.is_empty() || app.discord().is_none() {
        return out;
    }
    let a = app.clone();
    let fetched = bot_util::async_util::with_timeout(
        async move { bot_core::guild_members::fetch_members_by_ids(&a, guild_id, &missing, timeout_ms).await.map_err(|e| e.to_string()) },
        timeout_ms,
        "levels members",
    )
    .await;
    if let Ok(found) = fetched {
        for (u, m) in found {
            out.insert(u.to_string(), m);
        }
    }
    // A request that timed out part-way still cached what arrived (the JS reads the cache afterwards).
    for id in ids {
        if !out.contains_key(id)
            && let Some(m) = parse_snowflake(id).and_then(|u| cached_member(app, guild_id, UserId::new(u)))
        {
            out.insert(id.clone(), m);
        }
    }
    out
}

fn member_has(member: &Member, role_id: &str) -> bool {
    member.roles.iter().any(|r| r.to_string() == role_id)
}

/// `member.displayName ?? user.globalName ?? user.username`.
pub fn display_name(member: Option<&Member>, user: &serenity::all::User) -> String {
    member.map(member_display_name).unwrap_or_else(|| user.global_name.clone().unwrap_or_else(|| user.name.clone()))
}

// ── Reward roles ──────────────────────────────────────────────────────────────

/// `{ added, removed, failed }` of a role sync; `failed` is `(roleId, reason)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleSync {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// The options of [`sync_level_roles`].
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub sync_down: bool,
    /// The guild's reward rows when the caller already loaded them.
    pub rewards: Option<Vec<(i64, String)>>,
    pub reason: String,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self { sync_down: false, rewards: None, reason: "Level rewards".into() }
    }
}

/// `syncLevelRoles(member, level, settings, opts)`: brings a member's reward roles in line with `level`: adds first,
/// then removes, one role per call (R060). Never fails; failures are returned and logged (throttled).
pub async fn sync_level_roles(app: &App, guild_id: GuildId, member: &Member, level: i64, settings: Option<&GuildSettingsRef>, opts: SyncOptions) -> RoleSync {
    let mut out = RoleSync::default();
    let g = guild_id.to_string();
    let list = match opts.rewards {
        Some(l) => l,
        None => match level_roles(app.db(), &g).await {
            Ok(rows) => reward_pairs(&rows),
            Err(e) => {
                bot_util::log_error!(format!("levels: could not load role rewards of guild {g}"), bot_core::error::log_arg(&e));
                return out;
            }
        },
    };
    if list.is_empty() {
        return out;
    }
    let stack = settings.is_none_or(|s| s.level_roles_stack);
    let plan = plan_level_roles(&list, level, stack, opts.sync_down, |id| member_has(member, id));
    let ops: Vec<(String, bool)> = plan.add.into_iter().map(|r| (r, true)).chain(plan.remove.into_iter().map(|r| (r, false))).collect();
    for (role_id, add) in ops {
        let op = if add { "add" } else { "remove" };
        let check = match (app.discord(), parse_snowflake(&role_id)) {
            (Some(d), Some(rid)) => match d.cache.guild(guild_id) {
                Some(guild) => {
                    let role = guild.roles.get(&RoleId::new(rid)).cloned();
                    check_assignable_role(&guild, role.as_ref(), d.cache.current_user().id, None, true)
                }
                None => bot_core::interaction_helpers::RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() },
            },
            _ => bot_core::interaction_helpers::RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() },
        };
        if !check.ok {
            let reason = check.reason.unwrap_or_default();
            warn_once(app, &format!("role:{g}:{role_id}"), format!("levels: cannot manage role {role_id} in {g}: {reason}"));
            out.failed.push((role_id, reason));
            continue;
        }
        let Some(http) = app.http() else {
            out.failed.push((role_id, "Discord is not connected".into()));
            continue;
        };
        let rid = RoleId::new(parse_snowflake(&role_id).unwrap_or(1));
        let res = if add {
            http.add_member_role(guild_id, member.user.id, rid, Some(&opts.reason)).await
        } else {
            http.remove_member_role(guild_id, member.user.id, rid, Some(&opts.reason)).await
        };
        match res {
            Ok(()) => {
                if add { out.added.push(role_id) } else { out.removed.push(role_id) }
            }
            Err(e) => {
                let code = bot_core::discord::error_code(&e);
                let why = bot_core::discord::error_message(&e);
                out.failed.push((role_id.clone(), why.clone()));
                if matches!(code, Some(10011) | Some(10007)) {
                    continue;
                }
                if matches!(code, Some(50013) | Some(50001)) {
                    warn_once(app, &format!("role:{g}:{role_id}"), format!("levels: Discord refused to {op} role {role_id} in {g}: {why}"));
                } else {
                    bot_util::log_warn!(format!("levels: could not {op} role {role_id} in {g}: {why}"));
                }
            }
        }
    }
    out
}

/// The settings row type.
pub type GuildSettingsRef = bot_db::GuildSettings;

// ── Announcements ─────────────────────────────────────────────────────────────

/// The arguments of [`announce_level_up`].
#[derive(Debug, Clone)]
pub struct Announce<'a> {
    pub guild_id: GuildId,
    pub member: &'a Member,
    pub level: i64,
    pub xp: i64,
    pub settings: &'a GuildSettingsRef,
    pub source: &'a str,
    pub source_channel_id: Option<ChannelId>,
}

/// The level-up payload for a member, as [`announce_level_up`] would send it (the target decides `dm`).
pub async fn level_up_payload_for(app: &App, a: &Announce<'_>, dm: bool) -> Option<LevelUpPayload> {
    let s = a.settings;
    let template = s.level_up_message.as_deref().filter(|m| !bot_util::js::trim(m).is_empty()).unwrap_or(DEFAULT_LEVELUP_MESSAGE).to_string();
    let rank = if template_uses_rank(&template) {
        get_rank(app.db(), &a.guild_id.to_string(), &a.member.user.id.to_string(), a.xp).await.ok()
    } else {
        None
    };
    let ctx = render_context(app, Some(a.guild_id), &a.member.user, Some(a.member), a.source_channel_id);
    let guild_name = cached_guild(app, a.guild_id).map(|g| g.name).unwrap_or_default();
    let name = member_display_name(a.member);
    build_level_up_payload(LevelUpInput {
        template: &template,
        use_embed: s.level_up_use_embed,
        ctx,
        user_id: &a.member.user.id.to_string(),
        name: &name,
        guild_name: &guild_name,
        level: a.level,
        xp: a.xp as f64,
        rank,
        embed_color: s.embed_color.map(i64::from),
        dm,
    })
}

/// `announceLevelUp({ guild, member, level, xp, settings, source, sourceChannel })`: sends the level-up message where
/// the settings say. Never fails; true when a message was sent.
pub async fn announce_level_up(app: &App, a: Announce<'_>) -> bool {
    let g = a.guild_id.to_string();
    let mode = a.settings.level_up_mode.as_str();
    let source = a.source_channel_id.map(|c| c.to_string());
    let target = resolve_announce_target(Some(mode), a.source, a.settings.level_up_channel_id.as_deref(), source.as_deref());
    let dm = match &target {
        AnnounceTarget::None => return false,
        AnnounceTarget::Dm => true,
        AnnounceTarget::Channel { .. } => false,
    };
    let Some(payload) = level_up_payload_for(app, &a, dm).await else { return false };
    let Some(http) = app.http() else { return false };
    let member_id = a.member.user.id;
    if dm {
        let sent = match member_id.create_dm_channel(http.as_ref()).await {
            Ok(ch) => ch.id.send_message(http.as_ref(), payload.to_payload().to_message()).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            if bot_core::discord::error_code(&e) != Some(50007) {
                warn_once(app, &format!("announce-dm:{g}"), format!("levels: could not DM level-up to {member_id} in {g}: {}", bot_core::discord::error_message(&e)));
            }
            return false;
        }
        return true;
    }
    let AnnounceTarget::Channel { channel_id, .. } = target else { return false };
    let Some(cid) = parse_snowflake(&channel_id).map(ChannelId::new) else { return false };
    let needs: &[&str] = if a.settings.level_up_use_embed { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] };
    // The cached channel, else `guild.channels.fetch(id)` (a custom channel or thread missing from the cache).
    let usable = bot_core::interaction_helpers::check_channel_usable_fetch(app, a.guild_id, Some(cid), needs).await;
    if !usable.ok {
        // No fallback to another channel: a deleted or forbidden custom channel stays silent until an admin fixes it
        // (`/levelconfig status` flags it).
        let why = usable.reason.clone().unwrap_or_else(|| usable.missing.join(", "));
        warn_once(app, &format!("announce:{g}"), format!("levels: cannot announce in {channel_id} ({why}) for guild {g}"));
        return false;
    }
    if let Err(e) = cid.send_message(http.as_ref(), payload.to_payload().to_message()).await {
        let code = bot_core::discord::error_code(&e);
        let msg = format!("levels: could not announce in {channel_id} for guild {g}: {}", bot_core::discord::error_message(&e));
        if matches!(code, Some(10003) | Some(50001) | Some(50013)) {
            warn_once(app, &format!("announce:{g}"), msg);
        } else {
            bot_util::log_warn!(msg);
        }
        return false;
    }
    true
}

/// The arguments of [`handle_level_change`].
#[derive(Debug, Clone)]
pub struct LevelChange {
    pub guild_id: GuildId,
    pub member: Option<Member>,
    pub user_id: String,
    pub old_level: i64,
    pub new_level: i64,
    pub xp: i64,
    pub settings: Option<Settings>,
    /// `'message' | 'voice' | 'admin' | 'reward'`.
    pub source: String,
    pub channel_id: Option<ChannelId>,
    pub announce: bool,
}

impl LevelChange {
    /// From an XP write result.
    pub fn of(guild_id: GuildId, user_id: &str, r: &XpChange, settings: Option<Settings>, source: &str) -> Self {
        Self {
            guild_id,
            member: None,
            user_id: user_id.to_string(),
            old_level: r.old_level,
            new_level: r.new_level,
            xp: r.xp,
            settings,
            source: source.to_string(),
            channel_id: None,
            announce: true,
        }
    }

    pub fn member(mut self, member: Option<Member>) -> Self {
        self.member = member;
        self
    }

    pub fn channel(mut self, channel_id: Option<ChannelId>) -> Self {
        self.channel_id = channel_id;
        self
    }

    pub fn announce(mut self, announce: bool) -> Self {
        self.announce = announce;
        self
    }
}

/// `{ leveledUp, roles }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LevelChangeOutcome {
    pub leveled_up: bool,
    pub roles: Option<RoleSync>,
}

/// `handleLevelChange(...)`: the level-change pipeline: record the new stored level (one winner per transition),
/// announce a level-up once for the final level, sync reward roles, emit `levelUp`. A decrease only re-syncs roles
/// (when the toggle is on). Never fails.
pub async fn handle_level_change(app: &App, c: LevelChange) -> LevelChangeOutcome {
    if c.old_level == c.new_level {
        return LevelChangeOutcome::default();
    }
    let g = c.guild_id.to_string();
    let owned = match set_stored_level(app.db(), &g, &c.user_id, c.xp, c.new_level).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("levels: level change failed for {} in {g}", c.user_id), bot_core::error::log_arg(&e));
            return LevelChangeOutcome::default();
        }
    };
    if !owned {
        return LevelChangeOutcome::default(); // a concurrent writer owns this transition
    }
    let uid = parse_snowflake(&c.user_id).map(UserId::new);
    let who = match (c.member, uid) {
        (Some(m), _) => Some(m),
        (None, Some(u)) => fetch_member(app, c.guild_id, u).await,
        (None, None) => None,
    };
    let settings = c.settings.as_deref();
    if c.new_level > c.old_level {
        if c.announce
            && let (Some(m), Some(s)) = (who.as_ref(), settings)
            && is_module_enabled(Some(s), Some(ModuleKey::Leveling))
        {
            announce_level_up(app, Announce { guild_id: c.guild_id, member: m, level: c.new_level, xp: c.xp, settings: s, source: &c.source, source_channel_id: c.channel_id })
                .await;
        }
        let roles = match who.as_ref() {
            Some(m) => Some(
                sync_level_roles(app, c.guild_id, m, c.new_level, settings, SyncOptions { reason: format!("Level {} reward", c.new_level), ..Default::default() })
                    .await,
            ),
            None => None,
        };
        app.run_hooks(LevelUp {
            guild_id: c.guild_id,
            member: who,
            user_id: uid.unwrap_or(UserId::new(1)),
            old_level: c.old_level,
            new_level: c.new_level,
            channel_id: c.channel_id,
            source: c.source.clone(),
        })
        .await;
        return LevelChangeOutcome { leveled_up: true, roles };
    }
    let roles = match (who.as_ref(), settings) {
        (Some(m), Some(s)) if s.level_roles_sync_on_demotion => Some(
            sync_level_roles(app, c.guild_id, m, c.new_level, settings, SyncOptions { sync_down: true, reason: "Level decreased".into(), ..Default::default() })
                .await,
        ),
        _ => None,
    };
    LevelChangeOutcome { leveled_up: false, roles }
}

// ── Message XP ────────────────────────────────────────────────────────────────

/// `xp:cooldown:{guildId}:{userId}`.
pub fn cooldown_key(guild_id: &str, user_id: &str) -> String {
    format!("xp:cooldown:{guild_id}:{user_id}")
}

/// The cooldown TTL in seconds: `Math.max(1, Math.trunc(settings.xpCooldown) || 1)`.
pub fn cooldown_ttl(xp_cooldown: i64) -> i64 {
    if xp_cooldown == 0 { 1 } else { xp_cooldown.max(1) }
}

/// What [`message_xp`] did.
#[derive(Debug)]
pub enum MessageXp {
    /// No settings, a no-XP channel or role, or the member is on cooldown (or Redis is down: fail closed).
    Skipped(&'static str),
    /// XP was added; `level_change` is the detached level-change task when the level moved.
    Awarded { amount: i64, change: XpChange, level_change: Option<tokio::task::JoinHandle<LevelChangeOutcome>> },
}

/// `handleMessageXp(ctx)`'s body. The core already dropped bots, webhooks, DMs and system messages and the module
/// gate guarantees xpEnabled.
pub async fn message_xp(
    app: &App,
    settings: Option<Settings>,
    guild_id: GuildId,
    member: &Member,
    channel: Option<ChannelRef>,
    channel_id: Option<ChannelId>,
    rng: impl FnMut() -> f64,
) -> anyhow::Result<MessageXp> {
    let Some(settings) = settings else { return Ok(MessageXp::Skipped("no-settings")) };
    if is_no_xp_channel(&settings.no_xp_channel_ids, channel.as_ref()) {
        return Ok(MessageXp::Skipped("no-xp-channel")); // R189
    }
    let has = |id: &str| member_has(member, id);
    if has_no_xp_role(&settings.no_xp_role_ids, has) {
        return Ok(MessageXp::Skipped("no-xp-role"));
    }
    // Atomic cooldown claim (R295-R297). `None` means Redis is unavailable: FAIL CLOSED (no XP), because without the
    // cooldown a spammer would earn XP on every message. The check runs after the exemptions so exempt messages do
    // not consume the cooldown.
    let g = guild_id.to_string();
    let u = member.user.id.to_string();
    let claimed = app.cache().set_nx(&cooldown_key(&g, &u), &1, cooldown_ttl(i64::from(settings.xp_cooldown))).await;
    if claimed != Some(true) {
        return Ok(MessageXp::Skipped("cooldown"));
    }
    let boosts = normalize_boost_roles(settings.xp_boost_roles.as_ref());
    let amount = roll_xp(
        RollXp {
            min: Some(f64::from(settings.xp_min)),
            max: Some(f64::from(settings.xp_max)),
            multiplier: settings.xp_multiplier,
            boost: best_boost_multiplier(&boosts, has),
        },
        rng,
    );
    let change = apply_xp_delta(app, XpDelta { messages: 1.0, touch_last_xp: true, ..XpDelta::new(&g, &u, amount as f64) }).await?;
    let mut level_change = None;
    if change.new_level != change.old_level {
        // Detached, so role edits and the announcement never delay the later messageCreate hooks.
        let c = LevelChange::of(guild_id, &u, &change, Some(settings.clone()), "message").member(Some(member.clone())).channel(channel_id);
        let a = app.clone();
        level_change = Some(tokio::spawn(async move { handle_level_change(&a, c).await }));
    }
    Ok(MessageXp::Awarded { amount, change, level_change })
}

/// The `messageCreate` hook (`handleMessageXp`).
pub async fn handle_message_xp(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let Some(member) = ctx.data.member.clone() else { return Ok(()) };
    let settings = ctx.get_settings().await;
    let channel = match &ctx.data.channel {
        Some(ch) => Some(channel_ref(&ctx.app, ctx.data.guild_id, ch)),
        // `message.channel`: the cached channel, else fetched (an uncached thread), so the no-XP check is never
        // skipped. Only looked up when there is a no-XP list to check (isNoXpChannel returns false without one).
        None if settings.as_ref().is_some_and(|s| !s.no_xp_channel_ids.is_empty()) => {
            bot_core::interaction_helpers::resolve_channel(&ctx.app, ctx.data.guild_id, ctx.data.channel_id)
                .await
                .map(|ch| channel_ref(&ctx.app, ctx.data.guild_id, &ch))
        }
        None => None,
    };
    message_xp(&ctx.app, settings, ctx.data.guild_id, &member, channel, Some(ctx.data.channel_id), math_random).await?;
    Ok(())
}

// ── Voice XP ──────────────────────────────────────────────────────────────────

/// `voice:start:{guildId}:{userId}`.
pub fn voice_key(guild_id: &str, user_id: &str) -> String {
    format!("voice:start:{guild_id}:{user_id}")
}

/// The voice facts of one member that [`reconcile_voice_member`] reads (from the gateway cache).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceView {
    pub channel_id: Option<String>,
    pub parent_id: Option<String>,
    pub afk_channel_id: Option<String>,
    pub self_deaf: bool,
    pub server_deaf: bool,
    pub other_listeners: usize,
}

/// The listeners of a voice channel (`channel.members`), from the cached voice states.
pub fn channel_listeners(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Vec<Listener> {
    let Some(d) = app.discord() else { return Vec::new() };
    let Some(guild) = d.cache.guild(guild_id) else { return Vec::new() };
    guild
        .voice_states
        .values()
        .filter(|vs| vs.channel_id == Some(channel_id))
        .map(|vs| {
            let bot = vs.member.as_ref().map(|m| m.user.bot).or_else(|| guild.members.get(&vs.user_id).map(|m| m.user.bot)).unwrap_or(false);
            Listener { id: vs.user_id.to_string(), bot, self_deaf: vs.self_deaf, server_deaf: vs.deaf }
        })
        .collect()
}

/// The [`VoiceView`] of a member in the state `state` (`None`: not in voice), read from the cache.
pub fn voice_view(app: &App, guild_id: GuildId, user_id: UserId, state: Option<&VoiceState>) -> VoiceView {
    let channel_id = state.and_then(|s| s.channel_id);
    let guild = cached_guild(app, guild_id);
    VoiceView {
        channel_id: channel_id.map(|c| c.to_string()),
        parent_id: channel_id.and_then(|c| cached_channel(app, guild_id, c)).and_then(|c| c.parent_id).map(|p| p.to_string()),
        afk_channel_id: guild.and_then(|g| g.afk_metadata.map(|a| a.afk_channel_id.to_string())),
        self_deaf: state.is_some_and(|s| s.self_deaf),
        server_deaf: state.is_some_and(|s| s.deaf),
        other_listeners: channel_id.map_or(0, |c| count_other_listeners(&channel_listeners(app, guild_id, c), Some(&user_id.to_string()))),
    }
}

async fn pay_voice_minutes(app: &App, guild_id: GuildId, member: &Member, settings: &Settings, minutes: i64, channel_id: Option<String>) {
    let g = guild_id.to_string();
    let u = member.user.id.to_string();
    let boosts = normalize_boost_roles(settings.xp_boost_roles.as_ref());
    let xp = voice_xp_for_minutes(
        minutes as f64,
        Some(f64::from(settings.voice_xp_rate)),
        settings.xp_multiplier,
        best_boost_multiplier(&boosts, |id| member_has(member, id)),
    );
    if xp < 1 {
        return;
    }
    match apply_xp_delta(app, XpDelta { voice_minutes: minutes as f64, touch_last_xp: true, ..XpDelta::new(&g, &u, xp as f64) }).await {
        Ok(r) => {
            if r.new_level != r.old_level {
                // A failure here loses at most this segment: the session key was already rotated or deleted.
                let channel = channel_id
                    .as_deref()
                    .and_then(parse_snowflake)
                    .map(ChannelId::new)
                    .filter(|c| cached_channel(app, guild_id, *c).is_some());
                let c = LevelChange::of(guild_id, &u, &r, Some(settings.clone()), "voice").member(Some(member.clone())).channel(channel);
                let a = app.clone();
                tokio::spawn(async move {
                    handle_level_change(&a, c).await;
                });
            }
        }
        Err(e) => bot_util::log_error!(format!("levels: could not credit voice XP to {u} in {g}"), bot_core::error::log_arg(&e)),
    }
}

/// Where [`reconcile_voice_member`] reads the member's voice facts.
#[derive(Debug, Clone)]
pub enum VoiceSource {
    /// The member's voice state (`None`: not in voice); the channel, its listeners and the AFK channel are read from
    /// the gateway cache when the reconciliation runs (inside the member's lock), as the JS reads the live
    /// `state.channel.members`.
    State(Box<Option<VoiceState>>),
    /// Facts given by the caller (tests, or a caller without a gateway cache).
    View(VoiceView),
}

/// `reconcileVoiceMember({ guild, member, state, settings, now, freshJoin })`: starts, checkpoints or ends one member's
/// voice session according to their eligibility (R054, R191-R195). Serialised per member (the place in the queue is
/// taken when this is called). Resolves to whether the member is eligible now.
pub fn reconcile_voice_member(
    app: &App,
    guild_id: GuildId,
    member: Member,
    source: VoiceSource,
    settings: Settings,
    now: i64,
    fresh_join: bool,
) -> impl std::future::Future<Output = bool> + Send + 'static {
    let key_lock = format!("{guild_id}:{}", member.user.id);
    let a = app.clone();
    state(app).voice_mutex.run(&key_lock, move || async move {
        let view = match source {
            VoiceSource::View(v) => v,
            VoiceSource::State(s) => voice_view(&a, guild_id, member.user.id, s.as_ref().as_ref()),
        };
        let g = guild_id.to_string();
        let u = member.user.id.to_string();
        let key = voice_key(&g, &u);
        let ready = ready_at(&a, now);
        let elig = voice_eligibility(&VoiceInput {
            channel_id: view.channel_id.clone(),
            parent_id: view.parent_id.clone(),
            afk_channel_id: view.afk_channel_id.clone(),
            self_deaf: view.self_deaf,
            server_deaf: view.server_deaf,
            no_xp_channel_ids: settings.no_xp_channel_ids.clone(),
            no_xp_role: has_no_xp_role(&settings.no_xp_role_ids, |id| member_has(&member, id)),
            other_listeners: view.other_listeners,
        });
        let mut session = parse_voice_session(a.cache().get(&key).await.as_ref(), now as f64);
        if session.is_some() && fresh_join {
            // A stored session on a fresh join is stale (a missed leave): discard it without credit. The key is
            // deleted too, so an ineligible join (an empty channel) cannot leave it to be paid out by a later event.
            a.cache().del_one(&key).await;
            session = None;
        }
        if !elig.eligible {
            // Left, AFK, no-XP channel, deafened or alone.
            if let Some(s) = session {
                a.cache().del_one(&key).await;
                let seg = compute_voice_segment(s.start, now as f64, ready as f64, VOICE_SEGMENT_CAP_MINUTES);
                if seg.minutes >= 1 {
                    pay_voice_minutes(&a, guild_id, &member, &settings, seg.minutes, s.channel_id).await;
                }
            }
            return false;
        }
        let channel = view.channel_id.clone().unwrap_or_default();
        let Some(s) = session else {
            // (Re)start, whatever the transition: covers AFK -> normal, un-deafen, a partner joining (R054/R192/R193).
            // Redis down: set() reports false and no session starts (fail closed).
            a.cache().set(&key, &voice_session_json(now as f64, &channel), VOICE_KEY_TTL_SECONDS).await;
            return true;
        };
        let effective = s.start.max(ready as f64);
        if s.channel_id.as_deref() != Some(channel.as_str()) || now as f64 - effective >= VOICE_CHECKPOINT_MS as f64 {
            let seg = compute_voice_segment(s.start, now as f64, ready as f64, VOICE_SEGMENT_CAP_MINUTES);
            a.cache().set(&key, &voice_session_json(seg.next_start, &channel), VOICE_KEY_TTL_SECONDS).await;
            if seg.minutes >= 1 {
                pay_voice_minutes(&a, guild_id, &member, &settings, seg.minutes, Some(s.channel_id.unwrap_or(channel))).await;
            }
        }
        true
    })
}

/// `seedVoiceSweep(guild)`: seeds the recurring 10-minute checkpoint job for a guild (one row per guild; an existing
/// one is kept).
pub async fn seed_voice_sweep(app: &App, guild_id: &str) {
    let run_at = chrono::DateTime::from_timestamp_millis(app.clock().now_ms() + VOICE_CHECKPOINT_MS).unwrap_or_else(chrono::Utc::now);
    let job = ScheduleJob::new(guild_id, JobType::LevelsVoiceSweep, run_at).dedupe_key(sweep_dedupe_key(guild_id)).on_conflict(OnConflict::Keep);
    if let Err(e) = app.scheduler().schedule_job(job).await {
        bot_util::log_warn!(format!("levels: could not schedule the voice sweep of guild {guild_id}: {e}"));
    }
}

/// `levels.voiceSweep:{guildId}`.
pub fn sweep_dedupe_key(guild_id: &str) -> String {
    format!("levels.voiceSweep:{guild_id}")
}

/// `voiceChangeMatters(ctx)`: the sync filter of the voiceStateUpdate hook. Eligibility depends only on the channel
/// and the deafen state, so a `state` change that left both deafen flags alone (mute, camera, stream, suppress)
/// changes nobody's voice XP and is skipped.
pub fn voice_change_matters(change: &str, old_state: Option<&VoiceState>, new_state: Option<&VoiceState>) -> bool {
    if change != "state" {
        return true;
    }
    let deaf = |s: Option<&VoiceState>| s.is_some_and(|s| s.self_deaf || s.deaf);
    deaf(old_state) != deaf(new_state)
}

/// The members in voice in a guild (`guild.voiceStates.cache` with a member, not bots), up to `cap`.
fn voice_members(app: &App, guild_id: GuildId, channel: Option<ChannelId>, cap: usize) -> Vec<(Member, VoiceState)> {
    let Some(d) = app.discord() else { return Vec::new() };
    let Some(guild) = d.cache.guild(guild_id) else { return Vec::new() };
    let mut out = Vec::new();
    for vs in guild.voice_states.values() {
        if out.len() >= cap {
            break;
        }
        let Some(ch) = vs.channel_id else { continue };
        if channel.is_some_and(|c| c != ch) {
            continue;
        }
        let Some(member) = vs.member.clone().or_else(|| guild.members.get(&vs.user_id).cloned()) else { continue };
        if member.user.bot {
            continue;
        }
        out.push((member, vs.clone()));
    }
    out
}

/// `reconcileOthers(...)`: re-evaluates the other members of the given channels, one at a time. Never fails.
pub async fn reconcile_others(app: &App, guild_id: GuildId, member_id: UserId, channels: &[ChannelId], settings: &Settings, now: i64) {
    for ch in channels {
        let mut scanned = 0;
        for (m, vs) in voice_members(app, guild_id, Some(*ch), usize::MAX) {
            if m.user.id == member_id {
                continue;
            }
            if scanned >= VOICE_CHANNEL_SCAN_CAP {
                break;
            }
            scanned += 1;
            reconcile_voice_member(app, guild_id, m, VoiceSource::State(Box::new(Some(vs))), settings.clone(), now, false).await;
        }
    }
}

/// `handleVoiceStateXp(ctx)`'s body: reconciles the member, re-checks the others of the channels whose "alone"
/// status may have changed (detached; its task is returned), and seeds the sweep on a join.
pub async fn voice_state_xp(app: &App, settings: Option<Settings>, ev: &VoiceStateUpdate) -> Option<tokio::task::JoinHandle<()>> {
    let settings = settings?; // settings unavailable (database down): leave the session alone
    let g = ev.guild_id.to_string();
    let member = ev.member.clone();
    if !settings.voice_xp_enabled {
        app.cache().del_one(&voice_key(&g, &member.user.id.to_string())).await; // drop without credit
        return None;
    }
    let now = app.clock().now_ms();
    let fresh = ev.change.as_str() == "join";
    reconcile_voice_member(app, ev.guild_id, member.clone(), VoiceSource::State(Box::new(Some(ev.new_state.clone()))), settings.clone(), now, fresh).await;

    // Other members whose "alone" status may have changed (R191). Another member is eligible only with at least one
    // other listener, so their status can flip only when a channel now has one or two listeners; busier channels are
    // skipped. The re-check is detached so logging and automations do not wait for it.
    let old_ch = ev.old_state.as_ref().and_then(|s| s.channel_id);
    let new_ch = ev.new_state.channel_id;
    let mut ids: Vec<ChannelId> = Vec::new();
    if let Some(o) = old_ch
        && Some(o) != new_ch
    {
        ids.push(o);
    }
    if let Some(n) = new_ch
        && !ids.contains(&n)
    {
        ids.push(n);
    }
    let channels: Vec<ChannelId> = ids
        .into_iter()
        .filter(|c| cached_channel(app, ev.guild_id, *c).is_some() && count_other_listeners(&channel_listeners(app, ev.guild_id, *c), None) <= 2)
        .collect();
    let mut task = None;
    if !channels.is_empty() {
        let (a, gid, mid, s) = (app.clone(), ev.guild_id, member.user.id, settings.clone());
        task = Some(tokio::spawn(async move { reconcile_others(&a, gid, mid, &channels, &s, now).await }));
    }
    if fresh && state(app).sweep_seed_throttle.allow(&g) {
        seed_voice_sweep(app, &g).await;
    }
    task
}

/// The `voiceStateUpdate` hook (`handleVoiceStateXp`).
pub async fn handle_voice_state_xp(ctx: Arc<HookCtx<VoiceStateUpdate>>) -> BotResult {
    let settings = ctx.get_settings().await;
    voice_state_xp(&ctx.app, settings, &ctx.data).await;
    Ok(())
}

/// `seedVoiceSessions(ctx)`'s body: starts sessions for members already in voice (joined during downtime) and seeds
/// the sweep job when anyone is eligible.
pub async fn seed_voice_sessions_for(app: &App, guild_id: GuildId, settings: Option<Settings>) {
    let Some(settings) = settings.filter(|s| s.voice_xp_enabled) else { return };
    let mut any = false;
    for (m, vs) in voice_members(app, guild_id, None, VOICE_SEED_CAP) {
        // `now` defaults to `Date.now()` per member in the JS.
        let now = app.clock().now_ms();
        if reconcile_voice_member(app, guild_id, m, VoiceSource::State(Box::new(Some(vs))), settings.clone(), now, false).await {
            any = true;
        }
    }
    if any {
        seed_voice_sweep(app, &guild_id.to_string()).await;
    }
}

/// The `guildReady` hook (`seedVoiceSessions`); `available` behaves like `startup`.
pub async fn seed_voice_sessions(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    let settings = ctx.get_settings().await;
    seed_voice_sessions_for(&ctx.app, ctx.data.guild_id, settings).await;
    Ok(())
}

/// `runVoiceSweep(job, { guild })`: the `levels.voiceSweep` job (idempotent): checkpoints sessions older than 10
/// minutes and starts missing ones, then re-seeds itself while anyone is in voice.
pub async fn run_voice_sweep(app: &App, guild_id: &str) -> BotResult<JobOutcome> {
    // A missing row means the database was unreachable: fail so the scheduler retries instead of dropping the job.
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else {
        anyhow::bail!("settings of guild {guild_id} unavailable");
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Leveling)) || !settings.voice_xp_enabled {
        return Ok(JobOutcome::Done);
    }
    let Some(gid) = parse_snowflake(guild_id).map(GuildId::new) else { return Ok(JobOutcome::Done) };
    let members = voice_members(app, gid, None, VOICE_SEED_CAP);
    if members.is_empty() {
        return Ok(JobOutcome::Done); // the next join re-seeds
    }
    for (m, vs) in members {
        // `now` defaults to `Date.now()` per member in the JS.
        let now = app.clock().now_ms();
        reconcile_voice_member(app, gid, m, VoiceSource::State(Box::new(Some(vs))), settings.clone(), now, false).await;
    }
    let next = chrono::DateTime::from_timestamp_millis(app.clock().now_ms() + VOICE_CHECKPOINT_MS).unwrap_or_else(chrono::Utc::now);
    Ok(JobOutcome::Reschedule { next_run_at: next, payload: None })
}

// ── roleDelete cleanup ────────────────────────────────────────────────────────

/// `handleRoleDelete(ctx)`'s body (always runs, even while the module is off): drops the role's reward and its no-XP /
/// boost entries. Returns the number of rewards removed.
pub async fn role_deleted(app: &App, guild_id: &str, role_id: &str, settings: Option<Settings>) -> anyhow::Result<u64> {
    let n = bot_db::delete_where(&LEVEL_ROLE_INFO, [("guildId", json!(guild_id)), ("roleId", json!(role_id))])?.execute(app.db()).await?;
    if let Some(s) = settings
        && (s.no_xp_role_ids.iter().any(|x| x == role_id) || normalize_boost_roles(s.xp_boost_roles.as_ref()).iter().any(|b| b.role_id == role_id))
    {
        let rid = role_id.to_string();
        app.settings()
            .update_settings_with(guild_id, move |cur| {
                let mut patch = SettingsPatch::new();
                if cur.no_xp_role_ids.contains(&rid) {
                    let kept: Vec<String> = cur.no_xp_role_ids.iter().filter(|x| **x != rid).cloned().collect();
                    patch = patch.set("noXpRoleIds", json!(kept));
                }
                let boosts = normalize_boost_roles(cur.xp_boost_roles.as_ref());
                if boosts.iter().any(|b| b.role_id == rid) {
                    let kept: Vec<BoostRole> = boosts.into_iter().filter(|b| b.role_id != rid).collect();
                    patch = patch.set("xpBoostRoles", boost_roles_json(&kept));
                }
                Ok(patch)
            })
            .await?;
    }
    if n > 0 {
        bot_util::log_info!(format!("levels: removed {n} reward(s) for deleted role {role_id} in {guild_id}"));
    }
    Ok(n)
}

/// The `roleDelete` hook (`handleRoleDelete`).
pub async fn handle_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> BotResult {
    let settings = ctx.get_settings().await;
    role_deleted(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string(), settings).await?;
    Ok(())
}

// ── Bulk tasks ────────────────────────────────────────────────────────────────

/// `tryStartBulk(guildId)`: true when this call took the guild's bulk lock.
pub fn try_start_bulk(app: &App, guild_id: &str) -> bool {
    state(app).bulk_running.lock().unwrap_or_else(|e| e.into_inner()).insert(guild_id.to_string())
}

/// `endBulk(guildId)`.
pub fn end_bulk(app: &App, guild_id: &str) {
    state(app).bulk_running.lock().unwrap_or_else(|e| e.into_inner()).remove(guild_id);
}

/// `isBulkRunning(guildId)`.
pub fn is_bulk_running(app: &App, guild_id: &str) -> bool {
    state(app).bulk_running.lock().unwrap_or_else(|e| e.into_inner()).contains(guild_id)
}

/// `{ processed, changed, failed, missing }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BulkSync {
    pub processed: usize,
    pub changed: usize,
    pub failed: usize,
    pub missing: usize,
}

/// `runBulkRoleSync({ guild, entries, settings, syncDown })`: re-syncs reward roles for ranked members
/// (`(userId, level)`), sequentially. Never fails.
pub async fn run_bulk_role_sync(app: &App, guild_id: GuildId, entries: &[(String, i64)], settings: Option<&GuildSettingsRef>, sync_down: bool) -> BulkSync {
    let mut out = BulkSync::default();
    let g = guild_id.to_string();
    let rewards = match level_roles(app.db(), &g).await {
        Ok(r) => reward_pairs(&r),
        Err(e) => {
            bot_util::log_error!(format!("levels: bulk role sync failed in {g}"), bot_core::error::log_arg(&e));
            return out;
        }
    };
    if rewards.is_empty() {
        return out;
    }
    for chunk in entries.chunks(100) {
        let ids: Vec<String> = chunk.iter().map(|e| e.0.clone()).collect();
        let members = fetch_members(app, guild_id, &ids, 10_000).await;
        for (user_id, level) in chunk {
            let Some(m) = members.get(user_id) else {
                out.missing += 1;
                continue;
            };
            if m.user.bot {
                continue;
            }
            let r = sync_level_roles(app, guild_id, m, *level, settings, SyncOptions { sync_down, rewards: Some(rewards.clone()), ..Default::default() }).await;
            if !r.added.is_empty() || !r.removed.is_empty() {
                out.changed += 1;
            }
            if !r.failed.is_empty() {
                out.failed += 1;
            }
            out.processed += 1;
        }
    }
    out
}

/// What [`report_bulk_result`] needs of the invoking interaction.
#[derive(Clone)]
pub struct BulkReport {
    pub responder: Responder,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    /// `interaction.createdTimestamp`.
    pub created_ms: i64,
}

/// `reportBulkResult(interaction, text, { ephemeral })`: reports a finished background task to the invoker: a
/// follow-up while the interaction token lives, else the channel, else the log. Every branch is caught.
pub async fn report_bulk_result(app: &App, r: &BulkReport, text: &str, ephemeral: bool) {
    let embed = info_embed("Role Sync Finished", Some(text), None);
    if app.clock().now_ms() - r.created_ms < 14 * 60_000 {
        let follow = Payload::embed(embed).ephemeral(ephemeral).to_follow_up();
        if let Err(e) = r.responder.follow_up(follow).await {
            if bot_core::interaction_helpers::is_interaction_gone(&e) {
                bot_util::log_debug!(format!("levels: bulk report skipped, interaction gone ({})", e.code().map(|c| c.to_string()).unwrap_or_default()));
            } else {
                bot_util::log_warn!(format!("levels: could not report a bulk result: {e}"));
            }
        }
        return;
    }
    if check_channel_usable(app, r.guild_id, Some(r.channel_id), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS).ok
        && let Some(http) = app.http()
    {
        if let Err(e) = r.channel_id.send_message(http.as_ref(), Payload::embed(embed).no_mentions().to_message()).await {
            bot_util::log_warn!(format!("levels: could not report a bulk result: {}", bot_core::discord::error_message(&e)));
        }
        return;
    }
    bot_util::log_info!(text.to_string());
}

// ── Rewards provider (the `xp` kind on the rewards bus) ──────────────────────

/// `xpRewardProvider`.
pub struct XpRewardProvider;

#[async_trait]
impl RewardProvider for XpRewardProvider {
    async fn grant(&self, app: &App, args: RewardArgs) -> BotResult<RewardResult> {
        let g = args.guild_id.to_string();
        let settings = app.settings().get_settings_safe(&g).await;
        if !is_module_enabled(settings.as_deref(), Some(ModuleKey::Leveling)) {
            return Ok(RewardResult::fail("module-disabled"));
        }
        let who = match args.member.clone() {
            Some(m) => Some(m),
            None => match parse_snowflake(&args.user_id) {
                Some(u) => fetch_member(app, args.guild_id, UserId::new(u)).await,
                None => None,
            },
        };
        let Some(who) = who else { return Ok(RewardResult::fail("not-member")) };
        if who.user.bot {
            return Ok(RewardResult::fail("bot"));
        }
        match apply_xp_delta(app, XpDelta::new(&g, &args.user_id, args.amount as f64)).await {
            Ok(r) => {
                handle_level_change(app, LevelChange::of(args.guild_id, &args.user_id, &r, settings, "reward").member(Some(who)).channel(args.channel_id)).await;
                Ok(RewardResult::ok(Some(r.xp)))
            }
            Err(e) => {
                bot_util::log_error!("levels: xp reward grant failed", bot_core::error::log_arg(&e));
                Ok(RewardResult::fail("error"))
            }
        }
    }

    /// XP clamps at 0: taking more than the member has removes everything they have (unlike coins).
    async fn take(&self, app: &App, args: RewardArgs) -> BotResult<RewardResult> {
        let g = args.guild_id.to_string();
        let settings = app.settings().get_settings_safe(&g).await;
        if !is_module_enabled(settings.as_deref(), Some(ModuleKey::Leveling)) {
            return Ok(RewardResult::fail("module-disabled"));
        }
        let r = match subtract_xp(app, &g, &args.user_id, args.amount as f64).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("levels: xp reward take failed", bot_core::error::log_arg(&e));
                return Ok(RewardResult::fail("error"));
            }
        };
        let Some(r) = r.filter(|r| r.old_xp != Some(0)) else { return Ok(RewardResult::fail("insufficient")) };
        let who = match args.member.clone() {
            Some(m) => Some(m),
            None => match parse_snowflake(&args.user_id) {
                Some(u) => fetch_member(app, args.guild_id, UserId::new(u)).await,
                None => None,
            },
        };
        if r.new_level < r.old_level {
            handle_level_change(app, LevelChange::of(args.guild_id, &args.user_id, &r, settings, "reward").member(who).announce(false)).await;
        }
        Ok(RewardResult::ok(Some(r.xp)))
    }
}

// ── Deprecated entry points ───────────────────────────────────────────────────
// XP runs through the levels plugin. These remain only so a stale caller can never double-grant.

fn warn_deprecated(app: &App, name: &'static str) {
    let first = state(app).deprecated_warned.lock().unwrap_or_else(|e| e.into_inner()).insert(name);
    if first {
        bot_util::log_warn!("xpService.grantXp/handleVoiceXp is deprecated; XP runs via the levels plugin");
    }
}

/// `grantXp()`: deprecated no-op (warns once).
pub async fn grant_xp(app: &App) {
    warn_deprecated(app, "grantXp");
}

/// `handleVoiceXp()`: deprecated no-op (warns once).
pub async fn handle_voice_xp(app: &App) {
    warn_deprecated(app, "handleVoiceXp");
}
