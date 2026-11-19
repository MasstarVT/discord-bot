//! Port of `src/services/achievementService.js`: tiered milestones (messages, reactions, level, boost, purchases)
//! with role / XP / coin rewards and announcements. All logic lives here; the plugin only wires hooks to these
//! functions and the two commands only parse options and render replies.
//!
//! Every counted event is ONE atomic UPDATE per member ([`record_progress`], phase 1); messages are first summed in
//! memory per member and written as one `+n` UPDATE every few seconds (the message buffer). The first event of a member
//! per achievement INSERTs the rows (phase 2, `ON CONFLICT DO NOTHING`). A tier unlock is claimed with a conditional
//! update (compare-and-set on `tier`) BEFORE anything is paid, so each tier pays out at most once per member. Payout and
//! announcements run fire-and-forget (spawned tasks) so later hooks are never held up. There is deliberately NO keyed
//! mutex on the progress path; the config lock guards config writes only.
//!
//! The JS module-scope maps live in [`AchState`] (`app.ext`), one per app.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bot_config::limits;
use bot_core::App;
use bot_core::custom_embed::{RenderOptions, render_template};
use bot_core::interaction_helpers::{Payload, check_assignable_role_cached, check_channel_usable};
use bot_core::rewards::{RewardArgs, RewardKind};
use bot_core::settings::Settings;
use bot_db::sqlx::{self, Row};
use bot_db::{ACHIEVEMENT_INFO, Achievement, DbError, GuildSettings, InsertBuilder, SQL_NOW, SqlValue};
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders};
use bot_util::text::{clamp_text, format_number, progress_bar};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Member, RoleId, UserId};
use serenity::builder::CreateAllowedMentions;

// ── Constants ─────────────────────────────────────────────────────────────────

/// `TRIGGERS`.
pub const TRIGGERS: [&str; 5] = ["MESSAGES", "REACTIONS", "LEVEL", "BOOST", "PURCHASES"];
/// `TIER_NAMES`.
pub const TIER_NAMES: [&str; 4] = ["Bronze", "Silver", "Gold", "Diamond"];
/// `TIER_EMOJIS`.
pub const TIER_EMOJIS: [&str; 4] = ["🥉", "🥈", "🥇", "💎"];
/// `REWARD_TYPES`, in their fixed order.
pub const REWARD_TYPES: [&str; 4] = ["ROLE_ADD", "ROLE_REMOVE", "XP", "COINS"];
/// `GOAL_INPUT_MAX`: builder max for integer goal options.
pub const GOAL_INPUT_MAX: i64 = 10_000_000;
/// `PROGRESS_MAX`: int4-safe clamp.
pub const PROGRESS_MAX: i64 = 1_000_000_000;
/// `MAX_REWARD_AMOUNT`: per reward, XP or coins.
pub const MAX_REWARD_AMOUNT: i64 = 1_000_000;
pub const NAME_MAX: usize = 50;
pub const DESCRIPTION_MAX: usize = 200;
pub const MESSAGE_MAX: usize = 1500;
/// `DEFAULT_ACHIEVEMENT_MESSAGE`.
pub const DEFAULT_ACHIEVEMENT_MESSAGE: &str = "🏆 {player} unlocked **{achievement}**!";
/// `ALMOST_THERE_MESSAGE`.
pub const ALMOST_THERE_MESSAGE: &str = "🔥 Almost there! You're at **{progress}/{goal}** for **{achievement}** in **{server}**.";
pub const ACH_CACHE_TTL_S: i64 = 300;
pub const ACH_MEMO_TTL_MS: i64 = 30_000;
pub const MEMO_MAX: usize = 10_000;
pub const SEEDED_MAX: usize = 100_000;
pub const REACTION_DEDUPE_TTL_S: i64 = 86_400;
pub const BOOST_DEDUPE_TTL_S: i64 = 86_400;
pub const BOOST_FRESH_MS: i64 = 60_000;
pub const MAX_PURCHASE_DELTA: i64 = 1000;
pub const DEADLINE_MIN_MS: i64 = 60_000;
pub const DEADLINE_MAX_MS: i64 = 5 * 365 * 86_400_000;
pub const MEMBER_PAGE_SIZE: usize = 6;
pub const LIST_PAGE_SIZE: usize = 10;
/// `MESSAGE_HOOK_TRIGGERS`.
pub const MESSAGE_HOOK_TRIGGERS: [&str; 3] = ["MESSAGES", "LEVEL", "BOOST"];
/// `MODULE_OFF_NOTE`.
pub const MODULE_OFF_NOTE: &str =
    "\n\n⚠️ The Achievements module is off, so no progress is counted. Turn it on with `/module enable achievements`.";
/// The almost-there threshold (90 % of the next goal).
const ALMOST_RATIO: f64 = 0.9;

/// `achievementsKey(guildId)`: the Redis key of the guild's achievement index.
pub fn achievements_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:achievements")
}

/// `ach:react:{g}:{user}:{message}:{emojiKey}`: one count per (message, emoji) per member per 24 h.
pub fn reaction_key(guild_id: &str, user_id: &str, message_id: &str, emoji_key: &str) -> String {
    format!("ach:react:{guild_id}:{user_id}:{message_id}:{emoji_key}")
}

/// `ach:boost:{g}:{user}:{premiumSinceTimestamp}`: one count per boost start.
pub fn boost_key(guild_id: &str, user_id: &str, premium_since_ms: i64) -> String {
    format!("ach:boost:{guild_id}:{user_id}:{premium_since_ms}")
}

/// `TRIGGER_LABELS[trigger]`.
pub fn trigger_label(trigger: &str) -> Option<&'static str> {
    Some(match trigger {
        "MESSAGES" => "Messages sent",
        "REACTIONS" => "Reactions added",
        "LEVEL" => "Level reached",
        "BOOST" => "Server boosts",
        "PURCHASES" => "Shop items bought",
        _ => return None,
    })
}

/// `TRIGGER_EMOJI[trigger]`.
pub fn trigger_emoji(trigger: &str) -> &'static str {
    match trigger {
        "MESSAGES" => "💬",
        "REACTIONS" => "😄",
        "LEVEL" => "⭐",
        "BOOST" => "💎",
        "PURCHASES" => "🛒",
        _ => "",
    }
}

/// `TRIGGER_UNITS[trigger]`: (singular, plural).
pub fn trigger_units(trigger: &str) -> (&'static str, &'static str) {
    match trigger {
        "MESSAGES" => ("message", "messages"),
        "REACTIONS" => ("reaction", "reactions"),
        "LEVEL" => ("level", "levels"),
        "BOOST" => ("boost", "boosts"),
        "PURCHASES" => ("item bought", "items bought"),
        _ => ("", ""),
    }
}

/// `GOAL_MAX[trigger]`.
pub fn goal_max(trigger: &str) -> Option<i64> {
    Some(match trigger {
        "MESSAGES" | "REACTIONS" => 10_000_000,
        "LEVEL" => 1000,
        "BOOST" => 100,
        "PURCHASES" => 1_000_000,
        _ => return None,
    })
}

fn trigger_pos(trigger: &str) -> Option<usize> {
    TRIGGERS.iter().position(|t| *t == trigger)
}

/// One `PRESETS` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    pub emoji: &'static str,
    pub trigger: &'static str,
    pub goals: &'static [i64],
}

/// `PRESETS`, in declaration order.
pub const PRESETS: [(&str, Preset); 3] = [
    ("chatterbox", Preset { name: "Chatterbox", description: "Send messages in the server", emoji: "💬", trigger: "MESSAGES", goals: &[100, 500, 2500, 10000] }),
    ("booster", Preset { name: "Booster", description: "Boost the server", emoji: "💎", trigger: "BOOST", goals: &[1] }),
    ("level10", Preset { name: "Level 10", description: "Reach level 10", emoji: "⭐", trigger: "LEVEL", goals: &[10] }),
];

/// `PRESETS[key]`.
pub fn preset(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|(k, _)| *k == key).map(|(_, p)| p)
}

// ── In-process state ──────────────────────────────────────────────────────────

/// An insertion-ordered map with a size cap (the JS `Map` + "delete the first key" eviction).
#[derive(Debug)]
struct Lru<V> {
    map: HashMap<String, (u64, V)>,
    order: BTreeMap<u64, String>,
    seq: u64,
    max: usize,
}

impl<V> Lru<V> {
    fn new(max: usize) -> Self {
        Self { map: HashMap::new(), order: BTreeMap::new(), seq: 0, max }
    }

    fn remove(&mut self, key: &str) -> Option<V> {
        let (seq, v) = self.map.remove(key)?;
        self.order.remove(&seq);
        Some(v)
    }

    /// Deletes, then re-inserts at the end; evicts the oldest past the cap.
    fn insert(&mut self, key: String, value: V) {
        self.remove(&key);
        self.seq += 1;
        self.order.insert(self.seq, key.clone());
        self.map.insert(key, (self.seq, value));
        while self.map.len() > self.max {
            let Some((_, k)) = self.order.pop_first() else { break };
            self.map.remove(&k);
        }
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key).map(|(_, v)| v)
    }

    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// The message buffer's settings (`MESSAGE_FLUSH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageFlush {
    pub delay_ms: u64,
    pub at: i64,
    pub max_members: usize,
}

/// `MESSAGE_FLUSH`.
pub const MESSAGE_FLUSH: MessageFlush = MessageFlush { delay_ms: 5_000, at: 50, max_members: 10_000 };

struct PendingEntry {
    seq: u64,
    guild_id: String,
    user_id: String,
    sig: String,
    ids: Vec<String>,
    inc: i64,
    timer: Option<tokio::task::AbortHandle>,
    settings: Option<Settings>,
    member: Option<Member>,
    channel: Option<ChannelId>,
}

/// The JS module-level state of the achievements service, one per app (`app.ext::<AchState>()`).
pub struct AchState {
    index_memo: Mutex<Lru<Arc<Index>>>,
    index_loading: bot_core::sync::KeyedMutex,
    seeded: Mutex<Lru<()>>,
    warn_throttle: bot_util::async_util::Throttle,
    config_locks: bot_core::sync::KeyedMutex,
    flush_config: Mutex<MessageFlush>,
    pending: Mutex<HashMap<String, PendingEntry>>,
    pending_seq: AtomicU64,
    running: Mutex<HashMap<u64, (String, tokio::sync::watch::Receiver<bool>)>>,
    running_seq: AtomicU64,
    buffer_stopped: AtomicBool,
    shutdown_registered: AtomicBool,
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for AchState {
    fn default() -> Self {
        Self {
            index_memo: Mutex::new(Lru::new(MEMO_MAX)),
            index_loading: bot_core::sync::KeyedMutex::new(),
            seeded: Mutex::new(Lru::new(SEEDED_MAX)),
            warn_throttle: bot_util::async_util::create_throttle(10 * 60_000),
            config_locks: bot_core::sync::KeyedMutex::new(),
            flush_config: Mutex::new(MESSAGE_FLUSH),
            pending: Mutex::new(HashMap::new()),
            pending_seq: AtomicU64::new(0),
            running: Mutex::new(HashMap::new()),
            running_seq: AtomicU64::new(0),
            buffer_stopped: AtomicBool::new(false),
            shutdown_registered: AtomicBool::new(false),
            background: Mutex::new(Vec::new()),
        }
    }
}

fn state(app: &App) -> Arc<AchState> {
    app.ext::<AchState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `__resetAchievementState()`: forgets every in-process cache (waiting message counts are dropped, not written).
pub fn reset_achievement_state(app: &App) {
    let st = state(app);
    lock(&st.index_memo).clear();
    lock(&st.seeded).clear();
    for (_, e) in lock(&st.pending).drain() {
        if let Some(t) = e.timer {
            t.abort();
        }
    }
    st.buffer_stopped.store(false, Ordering::SeqCst);
}

/// `__setMessageFlush(over)`: changes the buffer (`delay_ms: 0` writes every message at once).
pub fn set_message_flush(app: &App, config: MessageFlush) {
    *lock(&state(app).flush_config) = config;
}

/// `withConfigLock(guildId, fn)`: runs `f` after every earlier config write of the same guild (config only; never the
/// progress or payout path).
pub fn with_config_lock<T, F, Fut>(app: &App, guild_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    state(app).config_locks.run(&format!("cfg:{guild_id}"), f)
}

fn warn_once(app: &App, guild_id: &str, kind: &str, message: &str, err: Option<&anyhow::Error>) {
    if !state(app).warn_throttle.allow(&format!("{guild_id}:{kind}")) {
        return;
    }
    match err {
        Some(e) => bot_util::log_warn!(message.to_string(), bot_core::error::log_arg(e)),
        None => bot_util::log_warn!(message.to_string()),
    }
}

fn mark_seeded(app: &App, achievement_id: &str, user_id: &str) {
    lock(&state(app).seeded).insert(format!("{achievement_id}:{user_id}"), ());
}

fn is_seeded(app: &App, achievement_id: &str, user_id: &str) -> bool {
    lock(&state(app).seeded).contains(&format!("{achievement_id}:{user_id}"))
}

/// Spawns a fire-and-forget task (payout, announcement, almost-there DM) and keeps its handle so tests can await it.
fn spawn_background<F>(app: &App, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(fut);
    let st = state(app);
    let mut bg = lock(&st.background);
    bg.retain(|h| !h.is_finished());
    bg.push(handle);
}

/// Waits for every payout, announcement and DM started so far (tests, and the shutdown flush).
pub async fn settle_background(app: &App) {
    loop {
        let handles: Vec<tokio::task::JoinHandle<()>> = std::mem::take(&mut *lock(&state(app).background));
        if handles.is_empty() {
            return;
        }
        for h in handles {
            let _ = h.await;
        }
    }
}

/// `Math.trunc(Number(n))` clamped to `min..=max`.
fn to_int(n: i64, min: i64, max: i64) -> i64 {
    n.clamp(min, max)
}

fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

// ── Rewards and tiers ─────────────────────────────────────────────────────────

/// One tier reward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reward {
    RoleAdd(String),
    RoleRemove(String),
    Xp(i64),
    Coins(i64),
}

impl Reward {
    /// The stored `type`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::RoleAdd(_) => "ROLE_ADD",
            Self::RoleRemove(_) => "ROLE_REMOVE",
            Self::Xp(_) => "XP",
            Self::Coins(_) => "COINS",
        }
    }

    /// The stored JSON: `{ type, roleId }` or `{ type, amount }`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::RoleAdd(r) | Self::RoleRemove(r) => json!({ "type": self.type_name(), "roleId": r }),
            Self::Xp(n) | Self::Coins(n) => json!({ "type": self.type_name(), "amount": n }),
        }
    }
}

impl Serialize for Reward {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Reward {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        normalize_rewards(&Value::Array(vec![v])).into_iter().next().ok_or_else(|| serde::de::Error::custom("invalid reward"))
    }
}

/// One tier: `{ name, goal, rewards }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier {
    pub name: String,
    pub goal: i64,
    pub rewards: Vec<Reward>,
}

/// `Number.isInteger(v)` for a JSON value, as an i64.
fn js_integer(v: Option<&Value>) -> Option<i64> {
    let f = v?.as_f64()?;
    (f.is_finite() && f.fract() == 0.0).then_some(f as i64)
}

fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `normalizeRewards(raw)`: keeps valid rewards only, one per type, ROLE_REMOVE dropped when it equals the ROLE_ADD
/// role, ordered ROLE_ADD, ROLE_REMOVE, XP, COINS. Never fails.
pub fn normalize_rewards(raw: &Value) -> Vec<Reward> {
    let Some(arr) = raw.as_array() else { return Vec::new() };
    let mut by: [Option<Reward>; 4] = [None, None, None, None];
    for r in arr {
        let Some(o) = r.as_object() else { continue };
        let Some(i) = o.get("type").and_then(Value::as_str).and_then(|t| REWARD_TYPES.iter().position(|x| *x == t)) else { continue };
        if by[i].is_some() {
            continue;
        }
        if i < 2 {
            let Some(role) = o.get("roleId").and_then(Value::as_str).filter(|s| is_snowflake(s)) else { continue };
            by[i] = Some(if i == 0 { Reward::RoleAdd(role.to_string()) } else { Reward::RoleRemove(role.to_string()) });
        } else {
            let Some(n) = js_integer(o.get("amount")).filter(|n| (1..=MAX_REWARD_AMOUNT).contains(n)) else { continue };
            by[i] = Some(if i == 2 { Reward::Xp(n) } else { Reward::Coins(n) });
        }
    }
    if let (Some(Reward::RoleAdd(a)), Some(Reward::RoleRemove(b))) = (&by[0], &by[1])
        && a == b
    {
        by[1] = None;
    }
    by.into_iter().flatten().collect()
}

/// The JSON array of a reward list.
pub fn rewards_json(rewards: &[Reward]) -> Value {
    Value::Array(rewards.iter().map(Reward::to_json).collect())
}

/// `normalizeTiers(raw)`: the stored tiers JSON (a bare array). Pure, never fails, used on every read.
pub fn normalize_tiers(raw: &Value) -> Vec<Tier> {
    let Some(arr) = raw.as_array() else { return Vec::new() };
    let valid = arr
        .iter()
        .filter(|t| t.is_object() && js_integer(t.get("goal")).is_some_and(|g| (1..=GOAL_INPUT_MAX).contains(&g)))
        .take(limits::ACHIEVEMENT_TIERS);
    let mut out: Vec<Tier> = Vec::new();
    for t in valid {
        let goal = js_integer(t.get("goal")).unwrap_or(0);
        if out.last().is_some_and(|l| goal <= l.goal) {
            break;
        }
        out.push(Tier { name: TIER_NAMES[out.len()].to_string(), goal, rewards: normalize_rewards(t.get("rewards").unwrap_or(&Value::Null)) });
    }
    out
}

/// The JSON array of tiers (what `serializeTiers` stores).
pub fn tiers_json(tiers: &[Tier]) -> Value {
    serde_json::to_value(tiers).unwrap_or_else(|_| Value::Array(Vec::new()))
}

/// `validateTiers(tiers, trigger)`: checks tiers (JSON as given) before every write.
pub fn validate_tiers(tiers: &Value, trigger: &str) -> Result<(), String> {
    let arr = match tiers.as_array() {
        Some(a) if !a.is_empty() && a.len() <= limits::ACHIEVEMENT_TIERS => a,
        _ => return Err(format!("An achievement needs 1 to {} tiers.", limits::ACHIEVEMENT_TIERS)),
    };
    let Some(max) = goal_max(trigger) else { return Err("Unknown trigger.".into()) };
    let mut prev = 0;
    for t in arr {
        let Some(goal) = js_integer(t.get("goal")).filter(|g| *g >= 1) else {
            return Err("Goals must be whole numbers of at least 1.".into());
        };
        if goal > max {
            return Err(format!("{} goals can be at most {}.", trigger_label(trigger).unwrap_or_default(), format_number(max as f64)));
        }
        if goal <= prev {
            return Err("Goals must go up from tier to tier.".into());
        }
        prev = goal;
        let rewards = match t.get("rewards") {
            None | Some(Value::Null) => Value::Array(Vec::new()),
            Some(r) => r.clone(),
        };
        // JSON.stringify(normalizeRewards(rewards)) !== JSON.stringify(rewards)
        let (normal, given) = (serde_json::to_string(&rewards_json(&normalize_rewards(&rewards))), serde_json::to_string(&rewards));
        if normal.ok() != given.ok() {
            return Err("Invalid rewards.".into());
        }
    }
    Ok(())
}

/// `serializeTiers(tiers)`: writes always store the normalised form.
pub fn serialize_tiers(tiers: &Value) -> Vec<Tier> {
    normalize_tiers(tiers)
}

/// Tiers given as (goal, rewards) pairs, as JSON (`[{ goal, rewards }]`), for [`serialize_tiers`] and
/// [`validate_tiers`].
pub fn tier_inputs_json(tiers: &[(i64, Vec<Reward>)]) -> Value {
    Value::Array(tiers.iter().map(|(g, r)| json!({ "goal": g, "rewards": rewards_json(r) })).collect())
}

/// `mergeRewards(existing, incoming, givenTypes)`: replaces the existing reward of every given type with the incoming
/// one; the rest is kept.
pub fn merge_rewards(existing: &[Reward], incoming: &[Reward], given_types: &[&str]) -> Vec<Reward> {
    let mut all: Vec<Value> = existing.iter().filter(|r| !given_types.contains(&r.type_name())).map(Reward::to_json).collect();
    all.extend(incoming.iter().map(Reward::to_json));
    normalize_rewards(&Value::Array(all))
}

/// `remapTierAfterRemoval(tier, k)`: members past the removed tier lose one count.
pub fn remap_tier_after_removal(tier: i64, k: i64) -> i64 {
    if tier > k { tier - 1 } else { tier }
}

// ── Input parsing ─────────────────────────────────────────────────────────────

fn is_goal_separator(c: char) -> bool {
    bot_util::js::is_js_space(c) || matches!(c, ',' | ';' | '>' | '|' | '/')
}

/// `/^(\d+(?:\.\d+)?)([km])?$/i`: (number text, multiplier).
fn goal_token(tok: &str) -> Option<(&str, f64)> {
    let (num, mult) = match tok.chars().last() {
        Some('k' | 'K') => (&tok[..tok.len() - 1], 1e3),
        Some('m' | 'M') => (&tok[..tok.len() - 1], 1e6),
        _ => (tok, 1.0),
    };
    let (int, frac) = match num.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (num, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(int) && frac.is_none_or(digits)).then_some((num, mult))
}

/// `parseGoals(input, trigger)`: `100` or `100,500,1k` (separators: space , ; > | /); suffix k/m multiply by 1e3/1e6.
pub fn parse_goals(input: &str, trigger: &str) -> Result<Vec<i64>, String> {
    let tokens: Vec<&str> = bot_util::js::trim(input).split(is_goal_separator).filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() || tokens.len() > limits::ACHIEVEMENT_TIERS {
        return Err("Give 1 to 4 goals, lowest first, e.g. `100,500,1k`.".into());
    }
    let max = goal_max(trigger).unwrap_or(GOAL_INPUT_MAX);
    let mut goals: Vec<i64> = Vec::new();
    for tok in tokens {
        let bad = || format!("`\"{tok}\"` is not a whole number.");
        let Some((num, mult)) = goal_token(tok) else { return Err(bad()) };
        let value = num.parse::<f64>().unwrap_or(f64::NAN) * mult;
        let rounded = bot_util::text::js_round(value);
        // 1.1k is 1100.0000000000002 in floating point; only a real fraction (1.0005k = 1000.5) is refused.
        if !value.is_finite() || (value - rounded).abs() > 1e-6 {
            return Err(bad());
        }
        if rounded < 1.0 {
            return Err("Goals must be at least 1.".into());
        }
        if rounded > max as f64 {
            return Err(format!("{} goals can be at most {}.", trigger_label(trigger).unwrap_or("Goals"), format_number(max as f64)));
        }
        goals.push(rounded as i64);
    }
    if goals.windows(2).any(|w| w[1] <= w[0]) {
        return Err("Goals must go up from tier to tier, e.g. `100,500,1k`.".into());
    }
    Ok(goals)
}

/// `normalizeAchievementName(s)`: trim, drop control characters, collapse whitespace; 1 to 50 characters or `None`.
pub fn normalize_achievement_name(s: Option<&str>) -> Option<String> {
    let t: String = bot_util::js::trim(s.unwrap_or("")).chars().filter(|c| !matches!(*c as u32, 0..=0x1f | 0x7f)).collect();
    let mut out = String::with_capacity(t.len());
    let mut space = false;
    for c in t.chars() {
        if bot_util::js::is_js_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    let out = bot_util::js::trim(&out).to_string();
    let n = bot_util::js::utf16_len(&out);
    (1..=NAME_MAX).contains(&n).then_some(out)
}

static CUSTOM_EMOJI_RE: bot_util::jsre::LazyRe = bot_util::lazy_re!(r"^<a?:[A-Za-z0-9_]{1,32}:(\d{17,20})>$", "");
static UNICODE_EMOJI_RE: bot_util::jsre::LazyRe =
    bot_util::lazy_re!(r"^[\p{Extended_Pictographic}\p{Emoji_Component}‍️\p{Regional_Indicator}]+$", "u");
static PICTOGRAPHIC_RE: bot_util::jsre::LazyRe = bot_util::lazy_re!(r"\p{Extended_Pictographic}", "u");
static REGIONAL_RE: bot_util::jsre::LazyRe = bot_util::lazy_re!(r"\p{Regional_Indicator}", "gu");

/// `normalizeBadgeEmoji(s, { hasCustomEmoji, allowClear })`: one Unicode emoji or a custom emoji from a server the bot
/// is in. `Ok(None)` clears.
pub fn normalize_badge_emoji(s: &str, has_custom_emoji: impl Fn(&str) -> bool, allow_clear: bool) -> Result<Option<String>, String> {
    let v = bot_util::js::trim(s);
    if allow_clear && bot_util::js::lower(v) == "none" {
        return Ok(None);
    }
    if let Some((t, m)) = CUSTOM_EMOJI_RE.exec(v) {
        let id = m.group(&t, 1).unwrap_or_default();
        return if has_custom_emoji(&id) { Ok(Some(v.to_string())) } else { Err("I can only show custom emojis from servers I'm in.".into()) };
    }
    if !v.is_empty()
        && bot_util::js::utf16_len(v) <= 32
        && !v.chars().any(bot_util::js::is_js_space)
        && UNICODE_EMOJI_RE.test(v)
        && (PICTOGRAPHIC_RE.test(v) || REGIONAL_RE.find_all(&bot_util::jsre::u16s(v)).len() >= 2)
    {
        return Ok(Some(v.to_string()));
    }
    Err("Use one emoji, like 🏆, or a custom emoji from this server.".into())
}

/// `parseDeadline(input, tz, { now, allowClear })`: a date in the server time zone (`2026-12-31 23:59`) or a duration
/// (`30d`). `Ok(None)` clears.
pub fn parse_deadline(input: &str, tz: &str, now: &DateTime<Utc>, allow_clear: bool) -> Result<Option<DateTime<Utc>>, String> {
    let s = bot_util::js::trim(input);
    if allow_clear && ["none", "off", "clear"].contains(&bot_util::js::lower(s).as_str()) {
        return Ok(None);
    }
    let zone = if tz.is_empty() { "UTC" } else { tz };
    let Some(value) = bot_util::time::parse_date_input(s, zone, now) else {
        return Err(format!("Use a date like 2026-12-31 23:59 (server time zone: {zone}) or a duration like 30d."));
    };
    let delta = value.timestamp_millis() - now.timestamp_millis();
    if delta < DEADLINE_MIN_MS {
        return Err("The deadline must be in the future.".into());
    }
    if delta > DEADLINE_MAX_MS {
        return Err("The deadline can be at most 5 years away.".into());
    }
    Ok(Some(value))
}

/// `rewardUnavailableMessage(kind, settings)`: why a reward kind cannot be configured right now, or `None`.
pub fn reward_unavailable_message(app: &App, kind: RewardKind, settings: Option<&GuildSettings>) -> Option<String> {
    if app.rewards().is_reward_available(kind, settings) {
        return None;
    }
    let key = kind.module();
    let m = key.info();
    let label = if kind == RewardKind::Xp { "XP" } else { "Coin" };
    if !app.rewards().has_provider(kind) {
        return Some(format!("{label} rewards are not available on this bot (the {} {} module is not installed).", m.emoji, m.label));
    }
    Some(format!("{label} rewards need the **{} {}** module. Enable it with `/module enable {}`.", m.emoji, m.label, key.as_str()))
}

// ── Display helpers ───────────────────────────────────────────────────────────

/// `badgeOf(a)`: the achievement's emoji, or the trigger's default.
pub fn badge_of(emoji: Option<&str>, trigger: &str) -> String {
    emoji.map(str::to_string).unwrap_or_else(|| trigger_emoji(trigger).to_string())
}

/// `achievementLabel(a, tierIndex)`: `Name` for single-tier achievements, `Name (Gold)` otherwise.
pub fn achievement_label(name: &str, tier_count: usize, tier_index: usize) -> String {
    if tier_count == 1 { name.to_string() } else { format!("{name} ({})", TIER_NAMES.get(tier_index).copied().unwrap_or("undefined")) }
}

fn currency(settings: Option<&GuildSettings>) -> (String, String) {
    match settings {
        Some(s) => (s.currency_emoji.clone(), s.currency_name.clone()),
        None => ("🪙".into(), "coins".into()),
    }
}

/// `formatRewards(rewards, guild, settings)`: human text for a reward list. `guild_roles` is the guild's role cache
/// (`None` when there is no guild): a role missing from it is flagged, never removed.
pub fn format_rewards(rewards: &[Reward], guild_roles: Option<&HashSet<String>>, settings: Option<&GuildSettings>) -> String {
    let (emoji, name) = currency(settings);
    format_rewards_with(rewards, guild_roles, &emoji, &name)
}

/// [`format_rewards`] with the currency given (`settings.currencyEmoji`, `settings.currencyName`).
pub fn format_rewards_with(rewards: &[Reward], guild_roles: Option<&HashSet<String>>, emoji: &str, name: &str) -> String {
    let role_text = |id: &str| match guild_roles {
        Some(c) if !c.contains(id) => "@deleted-role ⚠️".to_string(),
        _ => format!("<@&{id}>"),
    };
    let parts: Vec<String> = rewards
        .iter()
        .map(|r| match r {
            Reward::RoleAdd(id) => role_text(id),
            Reward::RoleRemove(id) => format!("−{}", role_text(id)),
            Reward::Xp(n) => format!("+{} XP", format_number(*n as f64)),
            Reward::Coins(n) => format!("+{} {emoji} {name}", format_number(*n as f64)),
        })
        .collect();
    clamp_text(&parts.join(", "), 900)
}

/// `achievementStatus(a, nowMs)`: `on`, `off` or `ended`.
pub fn achievement_status(enabled: bool, deadline_ms: Option<i64>, now_ms: i64) -> &'static str {
    if !enabled {
        return "off";
    }
    if deadline_ms.is_some_and(|d| d < now_ms) { "ended" } else { "on" }
}

/// `buildAnnouncementVars(...)`: the variables of the announcement template and the almost-there DM, in the JS
/// order.
pub fn build_announcement_vars(a: &Lite, tier_index: i64, progress: i64, user_id: &str, display_name: &str, rewards_text: &str) -> Vec<(String, String)> {
    let tier = usize::try_from(tier_index).ok().and_then(|i| a.tiers.get(i)).or(a.tiers.last());
    let idx = tier_index.clamp(0, TIER_NAMES.len() as i64 - 1) as usize;
    vec![
        ("player".into(), format!("<@{user_id}>")),
        ("player.name".into(), bot_core::markdown::escape_markdown(display_name)),
        ("achievement".into(), achievement_label(&a.name, a.tiers.len(), idx)),
        ("achievement.name".into(), a.name.clone()),
        ("achievement.description".into(), a.description.clone().unwrap_or_default()),
        ("tier".into(), TIER_NAMES[idx].into()),
        ("tier.emoji".into(), TIER_EMOJIS[idx].into()),
        ("goal".into(), format_number(tier.map_or(0, |t| t.goal) as f64)),
        ("progress".into(), format_number(progress as f64)),
        ("rewards".into(), rewards_text.to_string()),
    ]
}

/// `sampleVars(member)`: sample values for the `announce message` preview.
pub fn sample_vars(member_id: Option<&str>, display_name: Option<&str>) -> Vec<(String, String)> {
    vec![
        ("player".into(), format!("<@{}>", member_id.unwrap_or("0"))),
        ("player.name".into(), bot_core::markdown::escape_markdown(display_name.unwrap_or("Player"))),
        ("achievement".into(), "Chatterbox (Gold)".into()),
        ("achievement.name".into(), "Chatterbox".into()),
        ("achievement.description".into(), "Send messages in the server".into()),
        ("tier".into(), "Gold".into()),
        ("tier.emoji".into(), "🥇".into()),
        ("goal".into(), "2,500".into()),
        ("progress".into(), "2,500".into()),
        ("rewards".into(), "+100 XP".into()),
    ]
}

/// Variables as the placeholder parser takes them.
pub fn to_vars(vars: Vec<(String, String)>) -> Vec<(String, VarValue)> {
    vars.into_iter().map(|(k, v)| (k, VarValue::Text(v))).collect()
}

/// `pickAnnounceChannel(mode, { source, configured }, isUsable)`: CHANNEL tries the configured channel then the
/// source; CURRENT the other way round; every other mode has no channel.
pub fn pick_announce_channel<T: Copy>(mode: &str, source: Option<T>, configured: Option<T>, is_usable: impl Fn(T) -> bool) -> Option<T> {
    let order = match mode {
        "CHANNEL" => [configured, source],
        "CURRENT" => [source, configured],
        _ => return None,
    };
    order.into_iter().flatten().find(|c| is_usable(*c))
}

// ── Progress maths ────────────────────────────────────────────────────────────

/// What a progress row means for its tiers (`evaluateProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Evaluation {
    /// A tier to claim (the number of tiers reached).
    pub advance_to: Option<i64>,
    /// An almost-there notice: (tier index, goal).
    pub almost: Option<(usize, i64)>,
}

/// `evaluateProgress(tiers, row)`. Pure.
pub fn evaluate_progress(tiers: &[Tier], progress: i64, tier: i64, almost_notified: bool) -> Evaluation {
    let none = Evaluation { advance_to: None, almost: None };
    let reached = tiers.iter().filter(|t| progress >= t.goal).count() as i64;
    let current = tier.min(tiers.len() as i64);
    if reached > current {
        return Evaluation { advance_to: Some(reached), almost: None };
    }
    if current >= tiers.len() as i64 || almost_notified {
        return none;
    }
    let cur = current.max(0) as usize;
    let goal = tiers[cur].goal;
    let threshold = (goal as f64 * ALMOST_RATIO).ceil() as i64;
    if threshold >= goal {
        return none; // goals below 10 never get an almost-there
    }
    if progress >= threshold && progress < goal { Evaluation { advance_to: None, almost: Some((cur, goal)) } } else { none }
}

/// `isBoostStart(ctx, now)`: `premiumSince` going from unset to set. Times are epoch ms (`0`/`None` = unset).
pub fn is_boost_start(old_known: bool, old_premium_ms: Option<i64>, new_premium_ms: Option<i64>, now: i64) -> bool {
    let Some(n) = new_premium_ms.filter(|n| *n != 0) else { return false };
    if old_known {
        return old_premium_ms.is_none_or(|o| o == 0);
    }
    now - n >= 0 && now - n <= BOOST_FRESH_MS // the old member was partial: only a fresh premiumSince counts
}

/// `activeAchievements(list, nowMs)`: achievements that still count (tiers, deadline not passed).
pub fn active_achievements(list: &[Lite], now_ms: i64) -> Vec<Lite> {
    list.iter().filter(|a| !a.tiers.is_empty() && a.deadline_ms.is_none_or(|d| now_ms <= d)).cloned().collect()
}

/// What `/achievements` shows for one achievement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayProgress {
    pub progress: i64,
    pub tier: i64,
    pub pending: bool,
}

/// `displayProgress(a, row, ul, member)`: with a row, its values; without one, the past progress the member would be
/// seeded with (only when `trackPastProgress`), and whether that already meets a goal. `ul` is (messageCount, level);
/// `boosting` is `member.premiumSinceTimestamp` being set.
pub fn display_progress(a: &Lite, row: Option<(i64, i64)>, ul: Option<(i64, i64)>, boosting: bool) -> DisplayProgress {
    let tiers = normalize_tiers(&tiers_json(&a.tiers));
    if let Some((progress, tier)) = row {
        return DisplayProgress { progress, tier: tier.min(tiers.len() as i64), pending: false };
    }
    let mut seed = 0;
    if a.track_past_progress {
        seed = match a.trigger.as_str() {
            "MESSAGES" => ul.map_or(0, |u| u.0),
            "LEVEL" => ul.map_or(0, |u| u.1),
            "BOOST" => i64::from(boosting),
            _ => 0,
        };
    }
    let reached = tiers.iter().filter(|t| seed >= t.goal).count();
    DisplayProgress { progress: seed, tier: 0, pending: reached > 0 }
}

fn unit_of(trigger: &str, n: i64) -> &'static str {
    let (one, many) = trigger_units(trigger);
    if n == 1 { one } else { many }
}

/// `goalText(trigger, goal)`: `10 messages`, `1 boost`, or `level 10`.
pub fn goal_text(trigger: &str, goal: i64) -> String {
    if trigger == "LEVEL" { format!("level {}", format_number(goal as f64)) } else { format!("{} {}", format_number(goal as f64), unit_of(trigger, goal)) }
}

/// The view `formatMemberEntry` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryView {
    pub progress: i64,
    pub tier: i64,
    pub pending: bool,
    /// `None` = not given (`undefined`).
    pub enabled: Option<bool>,
}

/// `formatMemberEntry(a, view, nowMs)`: up to 3 lines for one achievement in `/achievements`.
pub fn format_member_entry(a: &Lite, view: &EntryView, now_ms: i64) -> String {
    let t = &a.tiers;
    let u = view.tier.max(0) as usize;
    let p = view.progress;
    let mut line1 = format!("{} **{}**", badge_of(a.emoji.as_deref(), &a.trigger), a.name);
    if u > 0 {
        if t.len() == 1 {
            line1.push_str(" — ✅ Unlocked");
        } else {
            line1.push_str(&format!(" — {} {}", TIER_EMOJIS.get(u - 1).copied().unwrap_or("undefined"), TIER_NAMES.get(u - 1).copied().unwrap_or("undefined")));
        }
    }
    if view.enabled == Some(false) {
        line1.push_str(" (retired)");
    }
    let mut line2 = if u >= t.len() {
        if a.trigger == "LEVEL" {
            format!("✅ Completed · level {}", format_number(p as f64))
        } else {
            format!("✅ Completed · {} {}", format_number(p as f64), unit_of(&a.trigger, p))
        }
    } else {
        let g = t[u].goal;
        let mut l = format!("{} {}/{}", progress_bar((p as f64 / g as f64).min(1.0), 10.0, "▰", "▱"), format_number(p.min(g) as f64), format_number(g as f64));
        if t.len() > 1 {
            l.push_str(&format!(" → {} {}", TIER_EMOJIS[u], TIER_NAMES[u]));
        }
        if view.pending && p >= g {
            l.push_str(" · unlocks on your next activity");
        }
        l
    };
    if let Some(d) = a.deadline_ms {
        if d < now_ms {
            line2.push_str(" · ⌛ ended");
        } else {
            line2.push_str(&format!(" · ⏳ ends {}", bot_util::time::discord_timestamp_ms(d as f64, "R")));
        }
    }
    let mut lines = vec![line1, line2];
    if let Some(desc) = a.description.as_deref().filter(|d| !d.is_empty()) {
        lines.push(format!("*{}*", clamp_text(desc, 100)));
    }
    lines.join("\n")
}

// ── Index ─────────────────────────────────────────────────────────────────────

fn de_tiers<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Tier>, D::Error> {
    Ok(normalize_tiers(&Value::deserialize(d)?))
}

/// An achievement as primitives only (`AchLite`, what the cache stores; deadline as epoch ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lite {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
    pub trigger: String,
    #[serde(deserialize_with = "de_tiers")]
    pub tiers: Vec<Tier>,
    pub track_past_progress: bool,
    #[serde(default)]
    pub deadline_ms: Option<i64>,
}

/// `toLite(row)`.
pub fn to_lite(row: &Achievement) -> Lite {
    Lite {
        id: row.id.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        emoji: row.emoji.clone(),
        trigger: row.trigger.as_str().to_string(),
        tiers: normalize_tiers(&row.tiers),
        track_past_progress: row.track_past_progress,
        deadline_ms: row.deadline.map(|d| d.timestamp_millis()),
    }
}

/// The enabled achievements of a guild grouped by trigger (`{ at, byTrigger }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    pub at: i64,
    by_trigger: [Vec<Lite>; 5],
}

impl Index {
    /// `byTrigger[trigger]`.
    pub fn get(&self, trigger: &str) -> &[Lite] {
        trigger_pos(trigger).map(|i| self.by_trigger[i].as_slice()).unwrap_or(&[])
    }
}

/// `buildIndex(items)`.
pub fn build_index(items: &[Lite], at: i64) -> Index {
    let mut by: [Vec<Lite>; 5] = Default::default();
    for a in items {
        if let Some(i) = trigger_pos(&a.trigger) {
            by[i].push(a.clone());
        }
    }
    Index { at, by_trigger: by }
}

/// The Redis value of the index: `{ v: 1, guildId, items }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedIndex {
    pub v: i64,
    pub guild_id: String,
    pub items: Vec<Lite>,
}

fn memo_set(app: &App, guild_id: &str, items: &[Lite]) -> Arc<Index> {
    let idx = Arc::new(build_index(items, now_ms(app)));
    lock(&state(app).index_memo).insert(guild_id.to_string(), idx.clone());
    idx
}

fn memo_fresh(app: &App, guild_id: &str) -> Option<Arc<Index>> {
    let m = lock(&state(app).index_memo).get(guild_id).cloned()?;
    (now_ms(app) - m.at < ACH_MEMO_TTL_MS).then_some(m)
}

const SELECT_ACHIEVEMENTS: &str = r#"SELECT * FROM "Achievement" WHERE "guildId" = $1 ORDER BY "createdAt" ASC, "id" ASC"#;
const SELECT_ENABLED_ACHIEVEMENTS: &str = r#"SELECT * FROM "Achievement" WHERE "guildId" = $1 AND "enabled" = true ORDER BY "createdAt" ASC, "id" ASC"#;

async fn load_items_from_db(app: &App, guild_id: &str) -> anyhow::Result<Vec<Lite>> {
    let rows: Vec<Achievement> = sqlx::query_as(SELECT_ENABLED_ACHIEVEMENTS).bind(guild_id).fetch_all(app.db()).await?;
    Ok(rows.iter().map(to_lite).filter(|a| !a.tiers.is_empty()).collect())
}

/// Reads the Redis copy; a value that is not the JS shape (or another guild's) is a miss.
pub fn parse_cached_index(value: &Value, guild_id: &str) -> Option<Vec<Lite>> {
    if value.get("v").and_then(Value::as_f64) != Some(1.0) || value.get("guildId").and_then(Value::as_str) != Some(guild_id) {
        return None;
    }
    let items = value.get("items")?.as_array()?;
    items.iter().map(|i| serde_json::from_value::<Lite>(i.clone()).ok()).collect()
}

async fn load_index(app: &App, guild_id: &str, started_at: i64) -> Option<Arc<Index>> {
    let remember = |items: &[Lite]| {
        let cur = lock(&state(app).index_memo).get(guild_id).cloned();
        match cur {
            Some(c) if c.at >= started_at => c,
            _ => memo_set(app, guild_id, items),
        }
    };
    let key = achievements_key(guild_id);
    // Redis down or a miss: fail OPEN to Postgres (the cache is only a cache).
    if let Some(items) = app.cache().get(&key).await.as_ref().and_then(|c| parse_cached_index(c, guild_id)) {
        return Some(remember(&items));
    }
    // Postgres down: fail CLOSED. The caller gets None and skips the event.
    match load_items_from_db(app, guild_id).await {
        Ok(items) => {
            // NX: a stale reader can never overwrite what a writer put there (refresh uses a plain SET).
            let value = CachedIndex { v: 1, guild_id: guild_id.to_string(), items: items.clone() };
            app.cache().set_nx(&key, &value, ACH_CACHE_TTL_S).await;
            Some(remember(&items))
        }
        Err(e) => {
            warn_once(app, guild_id, "db", "achievements: index load failed", Some(&e));
            None
        }
    }
}

/// `getAchievementIndex(guildId)`: the enabled achievements grouped by trigger. `None` = Postgres is unreachable.
/// A stale memo causes one load per guild, not one per message.
pub async fn get_achievement_index(app: &App, guild_id: &str) -> Option<Arc<Index>> {
    if let Some(m) = memo_fresh(app, guild_id) {
        return Some(m);
    }
    let started_at = now_ms(app);
    let (a, g) = (app.clone(), guild_id.to_string());
    state(app)
        .index_loading
        .run(&format!("idx:{guild_id}"), move || async move {
            // A load that finished while this one waited is used as it is.
            if let Some(m) = lock(&state(&a).index_memo).get(&g).cloned().filter(|m| m.at >= started_at) {
                return Some(m);
            }
            load_index(&a, &g, started_at).await
        })
        .await
}

/// `refreshAchievementCache(guildId)`: rebuilds the memo and the Redis copy after a config write. Never fails.
pub async fn refresh_achievement_cache(app: &App, guild_id: &str) {
    lock(&state(app).index_memo).remove(guild_id);
    match load_items_from_db(app, guild_id).await {
        Ok(items) => {
            let value = CachedIndex { v: 1, guild_id: guild_id.to_string(), items: items.clone() };
            app.cache().set(&achievements_key(guild_id), &value, ACH_CACHE_TTL_S).await; // a plain SET overwrites
            memo_set(app, guild_id, &items);
        }
        Err(e) => {
            warn_once(app, guild_id, "db", "achievements: cache refresh failed", Some(&e));
            app.cache().del_one(&achievements_key(guild_id)).await;
        }
    }
}

/// `mayHaveTriggers(guildId, triggers)`: the sync hint of the hook filters; false only when the memo is fresh and
/// none of `triggers` has an achievement.
pub fn may_have_triggers(app: &App, guild_id: &str, triggers: &[&str]) -> bool {
    let Some(m) = lock(&state(app).index_memo).get(guild_id).cloned() else { return true };
    if now_ms(app) - m.at >= ACH_MEMO_TTL_MS {
        return true;
    }
    triggers.iter().any(|t| !m.get(t).is_empty())
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// The phase 1 / phase 3 statement: `$1` inc, `$2` floor, `$3` PROGRESS_MAX, `$4` userId, `$5` achievement ids.
pub fn progress_update_sql() -> String {
    format!(
        r#"UPDATE "AchievementProgress"
    SET "progress" = LEAST(GREATEST("progress"::bigint + $1::bigint, $2::bigint), $3::bigint)::int,
        "updatedAt" = {SQL_NOW}
    WHERE "userId" = $4::text AND "achievementId" = ANY($5::text[])
    RETURNING "id", "achievementId", "progress", "tier", "almostNotified""#
    )
}

/// The phase 2 statement: `$1` guildId, `$2` userId, `$3` PROGRESS_MAX, `$4` new ids, `$5` achievement ids, `$6`
/// initial progress.
pub fn progress_insert_sql() -> String {
    format!(
        r#"INSERT INTO "AchievementProgress" ("id", "guildId", "achievementId", "userId", "progress", "createdAt", "updatedAt")
    SELECT t.id, $1::text, t.aid, $2::text, LEAST(t.init::bigint, $3::bigint)::int, {SQL_NOW}, {SQL_NOW}
    FROM unnest($4::text[], $5::text[], $6::int[]) AS t(id, aid, init)
    ON CONFLICT ("achievementId", "userId") DO NOTHING
    RETURNING "id", "achievementId", "progress", "tier", "almostNotified""#
    )
}

/// `RAW_SQL`: every raw statement by name.
pub fn raw_sql() -> Vec<(&'static str, String)> {
    vec![("achievements.progressUpdate", progress_update_sql()), ("achievements.progressInsert", progress_insert_sql())]
}

/// A progress row as the raw statements return it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressRow {
    pub id: String,
    pub achievement_id: String,
    pub progress: i64,
    pub tier: i64,
    pub almost_notified: bool,
}

fn progress_rows(rows: Vec<bot_db::sqlx::postgres::PgRow>) -> Result<Vec<ProgressRow>, sqlx::Error> {
    rows.iter()
        .map(|r| {
            Ok(ProgressRow {
                id: r.try_get("id")?,
                achievement_id: r.try_get("achievementId")?,
                progress: i64::from(r.try_get::<i32, _>("progress")?),
                tier: i64::from(r.try_get::<i32, _>("tier")?),
                almost_notified: r.try_get("almostNotified")?,
            })
        })
        .collect()
}

async fn run_update(app: &App, user_id: &str, ids: &[String], inc: i64, floor: i64) -> Result<Vec<ProgressRow>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(progress_update_sql()))
        .bind(inc)
        .bind(floor)
        .bind(PROGRESS_MAX)
        .bind(user_id)
        .bind(ids)
        .fetch_all(app.db())
        .await?;
    progress_rows(rows)
}

async fn run_insert(app: &App, guild_id: &str, user_id: &str, aids: &[String], inits: &[i64]) -> Result<Vec<ProgressRow>, sqlx::Error> {
    let new_ids: Vec<String> = aids.iter().map(|_| bot_db::new_uuid()).collect();
    let inits: Vec<i32> = inits.iter().map(|n| to_int(*n, 0, PROGRESS_MAX) as i32).collect();
    let rows = sqlx::query(sqlx::AssertSqlSafe(progress_insert_sql()))
        .bind(guild_id)
        .bind(user_id)
        .bind(PROGRESS_MAX)
        .bind(&new_ids)
        .bind(aids)
        .bind(&inits)
        .fetch_all(app.db())
        .await?;
    progress_rows(rows)
}

/// `readUserLevel(guildId, userId)`: (messageCount, level).
pub async fn read_user_level(app: &App, guild_id: &str, user_id: &str) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let row = sqlx::query(r#"SELECT "messageCount", "level" FROM "UserLevel" WHERE "guildId" = $1 AND "userId" = $2"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_optional(app.db())
        .await?;
    match row {
        Some(r) => Ok(Some((i64::from(r.try_get::<i32, _>("messageCount")?), i64::from(r.try_get::<i32, _>("level")?)))),
        None => Ok(None),
    }
}

// ── Recording progress ────────────────────────────────────────────────────────

/// Who and what a progress write is for (the `ctx`, `settings`, `member` and `channel` of the JS calls).
#[derive(Debug, Clone)]
pub struct Origin {
    pub guild_id: String,
    pub settings: Option<Settings>,
    pub user_id: String,
    pub member: Option<Member>,
    pub channel: Option<ChannelId>,
}

fn is_fk_violation(e: &anyhow::Error) -> bool {
    if let Some(DbError::Sqlx(sqlx::Error::Database(d))) = e.downcast_ref::<DbError>() {
        return d.code().as_deref() == Some("23503");
    }
    if let Some(sqlx::Error::Database(d)) = e.downcast_ref::<sqlx::Error>() {
        return d.code().as_deref() == Some("23503");
    }
    e.to_string().to_lowercase().contains("foreign key")
}

/// `recordProgress(...)`: adds `inc` (count triggers) or raises progress to `floor` (LEVEL) for every listed
/// achievement of one member, creates the missing rows, then evaluates each row for unlocks. Never fails; `None` on a
/// database error.
pub async fn record_progress(app: &App, o: &Origin, trigger: &str, achievements: &[Lite], inc: i64, floor: i64) -> Option<Vec<ProgressRow>> {
    if achievements.is_empty() {
        return Some(Vec::new());
    }
    match record_inner(app, o, trigger, achievements, inc, floor).await {
        Ok(rows) => Some(rows),
        Err(e) => {
            warn_once(app, &o.guild_id, "progress", "achievements: progress update failed", Some(&e));
            // An achievement deleted mid-flight raises an FK violation in phase 2; drop the stale index.
            if is_fk_violation(&e) {
                refresh_achievement_cache(app, &o.guild_id).await;
            }
            None
        }
    }
}

async fn record_inner(app: &App, o: &Origin, trigger: &str, list: &[Lite], inc: i64, floor: i64) -> anyhow::Result<Vec<ProgressRow>> {
    let by_id: HashMap<&str, &Lite> = list.iter().map(|a| (a.id.as_str(), a)).collect();
    let inc_n = to_int(inc, 0, PROGRESS_MAX);
    let floor_n = to_int(floor, 0, PROGRESS_MAX);
    let ids: Vec<String> = list.iter().map(|a| a.id.clone()).collect();

    // Phase 1: the common case, one atomic statement.
    let mut rows = run_update(app, &o.user_id, &ids, inc_n, floor_n).await?;
    let have: HashSet<String> = rows.iter().map(|r| r.achievement_id.clone()).collect();
    let missing: Vec<&Lite> = list.iter().filter(|a| !have.contains(&a.id)).collect();

    // Phase 2: first event of this member for these achievements.
    if !missing.is_empty() {
        app.settings().ensure_guild_settings(&o.guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut ul: Option<(i64, i64)> = None;
        if trigger == "MESSAGES" && missing.iter().any(|a| a.track_past_progress) {
            ul = read_user_level(app, &o.guild_id, &o.user_id).await.unwrap_or(None); // a read error means a seed of 0
        }
        let init = |a: &Lite| -> i64 {
            match trigger {
                // The first of the messages seeds from the history (max, not plus); the rest add on top.
                "MESSAGES" => {
                    if a.track_past_progress {
                        ul.map_or(0, |u| u.0).max(1) + (inc_n - 1).max(0)
                    } else {
                        inc_n
                    }
                }
                "LEVEL" => floor_n,
                _ => inc_n,
            }
        };
        let aids: Vec<String> = missing.iter().map(|a| a.id.clone()).collect();
        let inits: Vec<i64> = missing.iter().map(|a| init(a)).collect();
        let inserted = run_insert(app, &o.guild_id, &o.user_id, &aids, &inits).await?;
        let created: HashSet<String> = inserted.iter().map(|r| r.achievement_id.clone()).collect();
        let n_inserted = inserted.len();
        rows.extend(inserted);
        // Phase 3: a concurrent dispatch created some of those rows first; add our share to them.
        if n_inserted < missing.len() {
            let conflicted: Vec<String> = missing.iter().filter(|a| !created.contains(&a.id)).map(|a| a.id.clone()).collect();
            rows.extend(run_update(app, &o.user_id, &conflicted, inc_n, floor_n).await?);
        }
    }

    if trigger == "LEVEL" || trigger == "BOOST" {
        for r in &rows {
            mark_seeded(app, &r.achievement_id, &o.user_id);
        }
    }
    for row in &rows {
        let Some(a) = by_id.get(row.achievement_id.as_str()) else { continue };
        if let Err(e) = handle_row(app, o, a, row).await {
            warn_once(app, &o.guild_id, "progress", "achievements: row evaluation failed", Some(&e));
        }
    }
    Ok(rows)
}

/// `catchUpSeeds(...)`: LEVEL and BOOST have no event for the past, so a member's first message seeds them: one
/// UserLevel read and at most one INSERT per member per process lifetime per achievement. Never fails.
pub async fn catch_up_seeds(app: &App, o: &Origin, idx: &Index) {
    if let Err(e) = catch_up_inner(app, o, idx).await {
        warn_once(app, &o.guild_id, "progress", "achievements: catch-up failed", Some(&e));
    }
}

fn member_premium_ms(member: &Member) -> Option<i64> {
    member.premium_since.map(|t| t.unix_timestamp() * 1000 + i64::from(t.timestamp_subsec_millis()))
}

async fn catch_up_inner(app: &App, o: &Origin, idx: &Index) -> anyhow::Result<()> {
    let now = now_ms(app);
    let mut cands: Vec<Lite> = active_achievements(idx.get("LEVEL"), now);
    cands.extend(active_achievements(idx.get("BOOST"), now));
    cands.retain(|a| a.track_past_progress && !is_seeded(app, &a.id, &o.user_id));
    if cands.is_empty() {
        return Ok(());
    }
    let mut level = 0;
    if cands.iter().any(|a| a.trigger == "LEVEL") {
        match read_user_level(app, &o.guild_id, &o.user_id).await {
            Ok(r) => level = r.map_or(0, |u| u.1),
            Err(e) => {
                warn_once(app, &o.guild_id, "progress", "achievements: catch-up read failed", Some(&e.into()));
                return Ok(()); // not marked, so the next message tries again
            }
        }
    }
    // Without the member a boost cannot be read: leave those unmarked for a later message.
    if o.member.is_none() {
        cands.retain(|a| a.trigger != "BOOST");
    }
    if cands.is_empty() {
        return Ok(());
    }
    let boosting = o.member.as_ref().and_then(member_premium_ms).is_some_and(|m| m != 0);
    let init_of = |a: &Lite| if a.trigger == "LEVEL" { level } else { i64::from(boosting) };
    let to_insert: Vec<&Lite> = cands.iter().filter(|a| init_of(a) >= 1).collect();
    if !to_insert.is_empty() {
        app.settings().ensure_guild_settings(&o.guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let aids: Vec<String> = to_insert.iter().map(|a| a.id.clone()).collect();
        let inits: Vec<i64> = to_insert.iter().map(|a| init_of(a)).collect();
        let inserted = run_insert(app, &o.guild_id, &o.user_id, &aids, &inits).await?;
        // Rows that already existed return nothing, so their progress (including a reset) is never touched.
        for row in &inserted {
            let Some(a) = to_insert.iter().find(|a| a.id == row.achievement_id) else { continue };
            if let Err(e) = handle_row(app, o, a, row).await {
                warn_once(app, &o.guild_id, "progress", "achievements: row evaluation failed", Some(&e));
            }
        }
    }
    for a in &cands {
        mark_seeded(app, &a.id, &o.user_id);
    }
    Ok(())
}

// ── Claiming unlocks ──────────────────────────────────────────────────────────

/// `handleRow(...)`: evaluates one progress row. An unlock is claimed with a compare-and-set on `tier` BEFORE
/// anything is paid; the payout and announcement are started without awaiting.
pub async fn handle_row(app: &App, o: &Origin, a: &Lite, row: &ProgressRow) -> anyhow::Result<()> {
    let e = evaluate_progress(&a.tiers, row.progress, row.tier, row.almost_notified);
    if let Some(advance_to) = e.advance_to {
        let completed = (advance_to >= a.tiers.len() as i64).then(|| app.clock().now().naive_utc());
        let sql = format!(
            r#"UPDATE "AchievementProgress" SET "tier" = $1, "almostNotified" = false, "completedAt" = $2, "updatedAt" = {SQL_NOW} WHERE "id" = $3 AND "tier" = $4"#
        );
        let count = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(advance_to as i32)
            .bind(completed)
            .bind(&row.id)
            .bind(row.tier as i32)
            .execute(app.db())
            .await?
            .rows_affected();
        if count == 1 {
            let (app2, o2, a2) = (app.clone(), o.clone(), a.clone());
            let from = row.tier.min(a.tiers.len() as i64);
            let progress = row.progress;
            spawn_background(app, async move {
                celebrate(&app2, &o2, &a2, from, advance_to, progress).await;
            });
        }
        return Ok(());
    }
    let dm_mode = o.settings.as_ref().is_some_and(|s| s.achievement_announce_mode == bot_db::AnnounceMode::Dm);
    if let (Some((tier_index, goal)), true) = (e.almost, dm_mode) {
        let sql = format!(
            r#"UPDATE "AchievementProgress" SET "almostNotified" = true, "updatedAt" = {SQL_NOW} WHERE "id" = $1 AND "tier" = $2 AND "almostNotified" = false"#
        );
        let count = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&row.id).bind(row.tier as i32).execute(app.db()).await?.rows_affected();
        if count == 1 {
            let (app2, o2, a2) = (app.clone(), o.clone(), a.clone());
            let progress = row.progress;
            spawn_background(app, async move {
                send_almost_there(&app2, &o2, &a2, tier_index, goal, progress).await;
            });
        }
    }
    Ok(())
}

// ── Payout and announcement ───────────────────────────────────────────────────

fn guild_id_of(s: &str) -> Option<GuildId> {
    s.parse::<u64>().ok().filter(|v| *v != 0).map(GuildId::new)
}

fn user_id_of(s: &str) -> Option<UserId> {
    s.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new)
}

/// `guild.members.fetch(userId)`: the cache, then REST; `None` when the member is gone or Discord is offline.
pub async fn fetch_member(app: &App, guild_id: &str, user_id: &str) -> Option<Member> {
    let (g, u) = (guild_id_of(guild_id)?, user_id_of(user_id)?);
    let d = app.discord()?;
    if let Some(m) = d.cache.guild(g).and_then(|guild| guild.members.get(&u).cloned()) {
        return Some(m);
    }
    d.http.get_member(g, u).await.ok()
}

/// `celebrate(...)`: pays the rewards of the unlocked tiers, then announces. Never awaited by a hook.
pub async fn celebrate(app: &App, o: &Origin, a: &Lite, from_tier: i64, to_tier: i64, progress: i64) {
    let who = match &o.member {
        Some(m) => Some(m.clone()),
        None => fetch_member(app, &o.guild_id, &o.user_id).await,
    };
    let lines = pay_tier_rewards(app, o, who.as_ref(), a, from_tier, to_tier).await;
    let Some(who) = who else { return }; // the member left: XP and coins were granted; roles and the announcement are skipped
    let rewards_text = bot_util::text::join_with_limit(&lines, ", ", 300, bot_util::text::default_more);
    announce_unlock(app, o, &who, a, to_tier - 1, progress, &rewards_text).await;
}

async fn edit_role(app: &App, guild_id: &str, member: &Member, role_id: &str, add: bool, a: &Lite, tier_index: usize) -> bool {
    let (Some(g), Some(rid)) = (guild_id_of(guild_id), role_id.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new)) else { return false };
    let chk = check_assignable_role_cached(app, g, rid, None, !add);
    if !chk.ok {
        let verb = if add { "give" } else { "take" };
        warn_once(app, guild_id, "role", &format!("achievements: cannot {verb} role {role_id} ({})", chk.reason.unwrap_or_default()), None);
        return false;
    }
    if member.roles.contains(&rid) == add {
        return false; // already held (add) or not held (remove)
    }
    let reason = clamp_text(&format!("Achievement: {} ({})", a.name, TIER_NAMES.get(tier_index).copied().unwrap_or("undefined")), 512);
    let Some(http) = app.http() else {
        bot_util::log_warn!("achievements: role edit failed (Discord is not connected)");
        return false;
    };
    // One role per call; a whole-list edit would PATCH the member's role list.
    let r = if add {
        http.add_member_role(g, member.user.id, rid, Some(&reason)).await
    } else {
        http.remove_member_role(g, member.user.id, rid, Some(&reason)).await
    };
    match r {
        Ok(()) => true,
        Err(e) => {
            let code = bot_core::discord::error_code(&e);
            if matches!(code, Some(10011 | 10007)) {
                return false;
            }
            if matches!(code, Some(50013 | 50001)) {
                warn_once(app, guild_id, "perm", &format!("achievements: missing permission to edit role {role_id}"), None);
            } else {
                bot_util::log_warn!(format!("achievements: role edit failed ({})", bot_core::discord::error_message(&e)));
            }
            false
        }
    }
}

/// `payTierRewards(...)`: applies the rewards of tiers `from_tier` to `to_tier - 1` in tier order and returns the
/// summary lines. At most once: a rejected role edit or a lost process is not retried. Never fails.
pub async fn pay_tier_rewards(app: &App, o: &Origin, member: Option<&Member>, a: &Lite, from_tier: i64, to_tier: i64) -> Vec<String> {
    let mut lines = Vec::new();
    let (mut xp, mut coins) = (0i64, 0i64);
    for i in from_tier.max(0)..to_tier.max(0) {
        let Some(tier) = a.tiers.get(i as usize) else { continue };
        for r in &tier.rewards {
            match r {
                Reward::Xp(n) => xp += n,
                Reward::Coins(n) => coins += n,
                Reward::RoleAdd(id) => {
                    if let Some(m) = member
                        && edit_role(app, &o.guild_id, m, id, true, a, i as usize).await
                    {
                        lines.push(format!("<@&{id}>"));
                    }
                }
                Reward::RoleRemove(id) => {
                    if let Some(m) = member
                        && edit_role(app, &o.guild_id, m, id, false, a, i as usize).await
                    {
                        lines.push(format!("−<@&{id}>"));
                    }
                }
            }
        }
    }
    let reason = clamp_text(&format!("Achievement: {} ({})", a.name, TIER_NAMES[(to_tier - 1).clamp(0, 3) as usize]), 100);
    let Some(g) = guild_id_of(&o.guild_id) else { return lines };
    let (emoji, name) = currency(o.settings.as_deref());
    for (kind, sum) in [(RewardKind::Xp, xp), (RewardKind::Coins, coins)] {
        if sum < 1 {
            continue;
        }
        let amount = sum.min(1_000_000_000);
        let args = RewardArgs::new(g, o.user_id.clone(), amount).reason(reason.clone()).member(member.cloned()).channel(o.channel);
        let r = app.rewards().grant(app, kind, args).await;
        if r.ok {
            lines.push(if kind == RewardKind::Xp {
                format!("+{} XP", format_number(amount as f64))
            } else {
                format!("+{} {emoji} {name}", format_number(amount as f64))
            });
        } else {
            let k = kind.as_str();
            warn_once(app, &o.guild_id, &format!("reward-{k}"), &format!("achievements: {k} reward not paid ({})", r.reason.unwrap_or_default()), None);
        }
    }
    lines
}

/// `templateMentionsPlayer(template)`: the template pings the member (`{player}`, `{mention}`, `{user}`,
/// `{user.mention}`).
pub fn template_mentions_player(template: &str) -> bool {
    static RE: bot_util::jsre::LazyRe = bot_util::lazy_re!(r"\{(player|mention|user|user\.mention)\}", "i");
    RE.test(template)
}

/// Where an announcement goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceTarget {
    Dm(UserId),
    Channel(ChannelId),
}

/// An announcement ready to send (what `announceUnlock` sends).
#[derive(Debug, Clone, PartialEq)]
pub struct Announcement {
    pub target: AnnounceTarget,
    pub content: Option<String>,
    /// Embed JSON (zero or one).
    pub embeds: Vec<Value>,
    /// `allowedMentions.users` (channel announcements only; a DM allows none).
    pub ping_users: Vec<String>,
}

/// The decision of `announceUnlock`: the rendered text and where it goes, or `None` (mode OFF, nothing visible, no
/// usable channel). Reads the cached guild; no Discord call.
#[allow(clippy::too_many_arguments)]
pub fn plan_announcement(app: &App, o: &Origin, member: &Member, a: &Lite, tier_index: i64, progress: i64, rewards_text: &str) -> Option<Announcement> {
    let settings = o.settings.as_deref();
    let mode = settings.map_or("CURRENT", |s| s.achievement_announce_mode.as_str());
    if mode == "OFF" {
        return None;
    }
    let display = bot_core::adapters::member_display_name(member);
    let vars = build_announcement_vars(a, tier_index, progress, &member.user.id.to_string(), &display, rewards_text);
    let text = settings.and_then(|s| s.achievement_message.as_deref()).map(bot_util::js::trim).filter(|t| !t.is_empty()).unwrap_or(DEFAULT_ACHIEVEMENT_MESSAGE).to_string();
    let use_embed = settings.is_some_and(|s| s.achievement_use_embed);
    let color = settings.and_then(|s| s.embed_color).filter(|c| (0..=0xffffff).contains(c)).map(|c| c as u32);
    // A mention inside an embed never notifies: in embed mode a template that pings the member also puts the mention
    // in the message content (channel announcements only).
    let ping_in_content = use_embed && mode != "DM" && template_mentions_player(&text);
    let uid = member.user.id;
    let tpl = if use_embed {
        json!({ "content": if ping_in_content { json!(format!("<@{uid}>")) } else { Value::Null }, "embed": { "description": text } })
    } else {
        json!({ "content": text })
    };
    let g = guild_id_of(&o.guild_id);
    let mut ctx: RenderContext = bot_core::adapters::render_context(app, g, &member.user, Some(member), o.channel);
    ctx.vars = Some(to_vars(vars));
    let out = render_template(&tpl, &ctx, RenderOptions { default_color: color, allowed_mentions: Some(CreateAllowedMentions::new().users([uid])), extra_components: Vec::new() })?;
    let has_embed = !out.embeds.is_empty();
    // Nothing visible rendered: never a bare ping.
    if if use_embed { !has_embed } else { out.content.as_deref().is_none_or(|c| bot_util::js::trim(c).is_empty()) } {
        return None;
    }
    if mode == "DM" {
        let guild_name = g.and_then(|g| bot_core::interaction_helpers::cached_guild(app, g)).map(|g| g.name).unwrap_or_default();
        let footer = format!("-# Sent from **{}**", bot_core::markdown::escape_markdown(&guild_name));
        let room = 2000usize.saturating_sub(bot_util::js::utf16_len(&footer) + 1);
        let content = bot_util::js::trim(&format!("{}\n{footer}", clamp_text(out.content.as_deref().unwrap_or(""), room))).to_string();
        return Some(Announcement { target: AnnounceTarget::Dm(uid), content: Some(content), embeds: out.embeds, ping_users: Vec::new() });
    }
    let needs: &[&str] = if use_embed { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] };
    let configured = settings
        .and_then(|s| crate::birthday_service::set_id(&s.achievement_channel_id))
        .and_then(|c| c.parse::<u64>().ok())
        .filter(|v| *v != 0)
        .map(ChannelId::new)
        .filter(|c| g.is_some_and(|g| bot_core::interaction_helpers::cached_channel(app, g, *c).is_some()));
    let target = pick_announce_channel(mode, o.channel, configured, |c| g.is_some_and(|g| check_channel_usable(app, g, Some(c), needs).ok))?;
    Some(Announcement { target: AnnounceTarget::Channel(target), content: out.content.filter(|c| !c.is_empty()), embeds: out.embeds, ping_users: vec![uid.to_string()] })
}

/// Sends a planned announcement. Never fails.
pub async fn send_announcement(app: &App, guild_id: &str, ann: &Announcement) {
    let Some(http) = app.http() else { return };
    let embeds: Vec<_> = ann.embeds.iter().map(bot_core::custom_embed::embed_from_json).collect();
    match ann.target {
        AnnounceTarget::Dm(user) => {
            let mut p = Payload::new().no_mentions().embeds(embeds);
            if let Some(c) = &ann.content {
                p = p.content(c.clone());
            }
            let r = match user.create_dm_channel(http.as_ref()).await {
                Ok(ch) => ch.id.send_message(http.as_ref(), p.to_message()).await.map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = r
                && bot_core::discord::error_code(&e) != Some(50007)
            {
                bot_util::log_debug!(format!("achievements: DM failed ({})", bot_core::discord::error_message(&e)));
            }
        }
        AnnounceTarget::Channel(ch) => {
            let users: Vec<UserId> = ann.ping_users.iter().filter_map(|u| user_id_of(u)).collect();
            let mut p = Payload::new().embeds(embeds).allowed_mentions(CreateAllowedMentions::new().users(users));
            if let Some(c) = &ann.content {
                p = p.content(c.clone());
            }
            if let Err(e) = ch.send_message(http.as_ref(), p.to_message()).await {
                let code = bot_core::discord::error_code(&e);
                if matches!(code, Some(50013 | 50001)) {
                    warn_once(app, guild_id, "announce", &format!("achievements: cannot announce in {ch} (missing permission)"), None);
                } else if code != Some(10003) {
                    bot_util::log_warn!(format!("achievements: announcement failed ({})", bot_core::discord::error_message(&e)));
                }
            }
        }
    }
}

/// `announceUnlock(...)`: announces an unlock according to `achievementAnnounceMode`. Never fails.
pub async fn announce_unlock(app: &App, o: &Origin, member: &Member, a: &Lite, tier_index: i64, progress: i64, rewards_text: &str) {
    if let Some(ann) = plan_announcement(app, o, member, a, tier_index, progress, rewards_text) {
        send_announcement(app, &o.guild_id, &ann).await;
    }
}

/// The almost-there DM text (`renderPlaceholders(ALMOST_THERE_MESSAGE, ...)`).
pub fn almost_there_text(ctx: RenderContext, a: &Lite, tier_index: usize, goal: i64, progress: i64, user_id: &str, display_name: &str) -> String {
    let mut vars = build_announcement_vars(a, tier_index as i64, progress, user_id, display_name, "");
    for v in vars.iter_mut() {
        if v.0 == "goal" {
            v.1 = format_number(goal as f64);
        }
    }
    let mut ctx = ctx;
    ctx.vars = Some(to_vars(vars));
    render_placeholders(ALMOST_THERE_MESSAGE, &ctx, Some(2000))
}

/// `sendAlmostThere(...)`: one DM per tier at 90 % of the next goal (only in DM mode). Never fails.
pub async fn send_almost_there(app: &App, o: &Origin, a: &Lite, tier_index: usize, goal: i64, progress: i64) {
    let Some(http) = app.http() else { return };
    let user = match &o.member {
        Some(m) => Some(m.user.clone()),
        None => match user_id_of(&o.user_id) {
            Some(u) => http.get_user(u).await.ok(),
            None => None,
        },
    };
    let Some(user) = user else { return };
    let display = match &o.member {
        Some(m) => bot_core::adapters::member_display_name(m),
        None => user.global_name.clone().unwrap_or_else(|| user.name.clone()),
    };
    let ctx = bot_core::adapters::render_context(app, guild_id_of(&o.guild_id), &user, o.member.as_ref(), None);
    let content = almost_there_text(ctx, a, tier_index, goal, progress, &o.user_id, &display);
    let r = match user.id.create_dm_channel(http.as_ref()).await {
        Ok(ch) => ch.id.send_message(http.as_ref(), Payload::text(content).no_mentions().to_message()).await.map(|_| ()),
        Err(e) => Err(e),
    };
    if let Err(e) = r
        && bot_core::discord::error_code(&e) != Some(50007)
    {
        bot_util::log_debug!(format!("achievements: almost-there DM failed ({})", bot_core::discord::error_message(&e)));
    }
}

// ── Message buffer (write-behind) ─────────────────────────────────────────────

/// Registers the final flush at shutdown (bot.js: order 10, 3 s), once per app.
pub fn ensure_shutdown_flush(app: &App) {
    if state(app).shutdown_registered.swap(true, Ordering::SeqCst) {
        return;
    }
    let a = app.clone();
    bot_core::client::on_shutdown(
        app,
        10,
        "achievement count flush",
        3000,
        "Achievement message counts were not written before shutdown",
        move || {
            let a = a.clone();
            async move {
                flush_message_counts(&a, true).await;
                Ok(())
            }
        },
    );
}

fn buffer_message(app: &App, o: Origin, achievements: &[Lite]) {
    let st = state(app);
    let key = format!("{}:{}", o.guild_id, o.user_id);
    let ids: Vec<String> = achievements.iter().map(|a| a.id.clone()).collect();
    let sig = ids.join(",");
    let cfg = *lock(&st.flush_config);
    let mut to_flush: Vec<PendingEntry> = Vec::new();
    {
        let mut pending = lock(&st.pending);
        // Another set of active achievements: the waiting messages belong to the old set.
        if pending.get(&key).is_some_and(|e| e.sig != sig)
            && let Some(e) = pending.remove(&key)
        {
            to_flush.push(e);
        }
        if !pending.contains_key(&key) {
            while pending.len() >= cfg.max_members.max(1) {
                let Some(oldest) = pending.iter().min_by_key(|(_, e)| e.seq).map(|(k, _)| k.clone()) else { break };
                if let Some(e) = pending.remove(&oldest) {
                    to_flush.push(e);
                }
            }
            let (a2, k2, delay) = (app.clone(), key.clone(), cfg.delay_ms);
            let timer = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                flush_member(&a2, &k2);
            });
            pending.insert(
                key.clone(),
                PendingEntry {
                    seq: st.pending_seq.fetch_add(1, Ordering::SeqCst),
                    guild_id: o.guild_id.clone(),
                    user_id: o.user_id.clone(),
                    sig,
                    ids,
                    inc: 0,
                    timer: Some(timer.abort_handle()),
                    settings: None,
                    member: None,
                    channel: None,
                },
            );
        }
        if let Some(e) = pending.get_mut(&key) {
            e.inc += 1;
            // The newest message wins: an unlock is announced where the member wrote last.
            e.settings = o.settings;
            if o.member.is_some() {
                e.member = o.member;
            }
            if o.channel.is_some() {
                e.channel = o.channel;
            }
            if e.inc >= cfg.at
                && let Some(e) = pending.remove(&key)
            {
                to_flush.push(e);
            }
        }
    }
    for e in to_flush {
        start_write(app, e);
    }
}

/// `flushMember(key)`: starts writing one member's waiting messages. Never fails.
fn flush_member(app: &App, key: &str) {
    let entry = lock(&state(app).pending).remove(key);
    if let Some(e) = entry {
        start_write(app, e);
    }
}

fn start_write(app: &App, mut entry: PendingEntry) {
    if let Some(t) = entry.timer.take() {
        t.abort();
    }
    let st = state(app);
    let id = st.running_seq.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = tokio::sync::watch::channel(false);
    let key = format!("{}:{}", entry.guild_id, entry.user_id);
    lock(&st.running).insert(id, (key, rx));
    let a = app.clone();
    tokio::spawn(async move {
        write_messages(&a, entry).await;
        lock(&state(&a).running).remove(&id);
        let _ = tx.send(true);
    });
}

async fn wait_all(rxs: Vec<tokio::sync::watch::Receiver<bool>>) {
    for mut rx in rxs {
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

/// `settleMember(guildId, userId)`: writes one member's waiting messages and waits for every write of that member.
async fn settle_member(app: &App, guild_id: &str, user_id: &str) {
    let key = format!("{guild_id}:{user_id}");
    flush_member(app, &key);
    let rxs: Vec<_> = lock(&state(app).running).values().filter(|(k, _)| *k == key).map(|(_, rx)| rx.clone()).collect();
    wait_all(rxs).await;
}

async fn write_messages(app: &App, entry: PendingEntry) {
    // The achievements as they are now (tier edits apply, a deleted one is dropped), limited to the ones these
    // messages counted for.
    let Some(idx) = get_achievement_index(app, &entry.guild_id).await else { return };
    let current: HashMap<&str, &Lite> = idx.get("MESSAGES").iter().map(|a| (a.id.as_str(), a)).collect();
    let list: Vec<Lite> = entry.ids.iter().filter_map(|id| current.get(id.as_str()).map(|a| (*a).clone())).collect();
    if list.is_empty() {
        return;
    }
    let o = Origin { guild_id: entry.guild_id, settings: entry.settings, user_id: entry.user_id, member: entry.member, channel: entry.channel };
    record_progress(app, &o, "MESSAGES", &list, entry.inc, 0).await;
}

/// `flushMessageCounts({ final })`: writes every waiting message count now and waits for all writes. `final`
/// (shutdown): later messages are written at once. Never fails.
pub async fn flush_message_counts(app: &App, final_flush: bool) {
    let st = state(app);
    if final_flush {
        st.buffer_stopped.store(true, Ordering::SeqCst);
    }
    let keys: Vec<String> = lock(&st.pending).keys().cloned().collect();
    for k in keys {
        flush_member(app, &k);
    }
    let rxs: Vec<_> = lock(&st.running).values().map(|(_, rx)| rx.clone()).collect();
    wait_all(rxs).await;
}

// ── Hook filters and runs ─────────────────────────────────────────────────────

/// `messageFilter`.
pub fn message_filter(app: &App, guild_id: &str, author_bot: bool) -> bool {
    !guild_id.is_empty() && !author_bot && may_have_triggers(app, guild_id, &MESSAGE_HOOK_TRIGGERS)
}

/// `reactionFilter`.
pub fn reaction_filter(app: &App, guild_id: &str, user_bot: bool) -> bool {
    !guild_id.is_empty() && !user_bot && may_have_triggers(app, guild_id, &["REACTIONS"])
}

/// `memberUpdateFilter`.
pub fn member_update_filter(app: &App, guild_id: &str, bot: bool, old_known: bool, old_premium_ms: Option<i64>, new_premium_ms: Option<i64>) -> bool {
    !bot && is_boost_start(old_known, old_premium_ms, new_premium_ms, now_ms(app)) && may_have_triggers(app, guild_id, &["BOOST"])
}

/// `levelUpFilter`.
pub fn level_up_filter(app: &App, guild_id: &str) -> bool {
    !guild_id.is_empty() && may_have_triggers(app, guild_id, &["LEVEL"])
}

/// `purchaseFilter`.
pub fn purchase_filter(app: &App, guild_id: &str) -> bool {
    !guild_id.is_empty() && may_have_triggers(app, guild_id, &["PURCHASES"])
}

/// `onMessageCreate(ctx)`.
pub async fn on_message_create(app: &App, o: Origin) {
    // `register` installs the flush at boot; this keeps it for an app that never installed the registry (idempotent).
    ensure_shutdown_flush(app);
    let Some(idx) = get_achievement_index(app, &o.guild_id).await else { return };
    let msgs = active_achievements(idx.get("MESSAGES"), now_ms(app));
    if !msgs.is_empty() {
        let cfg = *lock(&state(app).flush_config);
        if cfg.delay_ms > 0 && !state(app).buffer_stopped.load(Ordering::SeqCst) {
            buffer_message(app, o.clone(), &msgs);
        } else {
            record_progress(app, &o, "MESSAGES", &msgs, 1, 0).await;
        }
    }
    catch_up_seeds(app, &o, &idx).await;
}

/// `onReactionAdd(ctx)`: `message_channel` is the reacted message's channel.
pub async fn on_reaction_add(app: &App, guild_id: &str, settings: Option<Settings>, user_id: &str, message_id: &str, emoji_key: &str, message_channel: Option<ChannelId>) {
    let Some(idx) = get_achievement_index(app, guild_id).await else { return };
    let list = active_achievements(idx.get("REACTIONS"), now_ms(app));
    if list.is_empty() {
        return;
    }
    // Each (message, emoji) pair counts once per member per 24 h. Redis down does NOT count: fail closed.
    let ok = app.cache().set_nx(&reaction_key(guild_id, user_id, message_id, emoji_key), &1, REACTION_DEDUPE_TTL_S).await;
    if ok != Some(true) {
        return;
    }
    let channel = message_channel.filter(|c| guild_id_of(guild_id).is_some_and(|g| bot_core::interaction_helpers::cached_channel(app, g, *c).is_some()));
    // No member fetch here: it happens only on an unlock.
    let o = Origin { guild_id: guild_id.to_string(), settings, user_id: user_id.to_string(), member: None, channel };
    record_progress(app, &o, "REACTIONS", &list, 1, 0).await;
}

/// `onMemberUpdate(ctx)`: a boost start.
pub async fn on_member_update(app: &App, guild_id: &str, settings: Option<Settings>, member: Member, old_known: bool) {
    let Some(idx) = get_achievement_index(app, guild_id).await else { return };
    let list = active_achievements(idx.get("BOOST"), now_ms(app));
    if list.is_empty() {
        return;
    }
    let user_id = member.user.id.to_string();
    // false = duplicate event. None (Redis down): count only when the transition is certain.
    let key = match member_premium_ms(&member) {
        Some(ms) => boost_key(guild_id, &user_id, ms),
        None => format!("ach:boost:{guild_id}:{user_id}:null"), // the filter lets only a boost start through
    };
    let r = app.cache().set_nx(&key, &1, BOOST_DEDUPE_TTL_S).await;
    if r == Some(false) || (r.is_none() && !old_known) {
        return;
    }
    let o = Origin { guild_id: guild_id.to_string(), settings, user_id, member: Some(member), channel: None };
    record_progress(app, &o, "BOOST", &list, 1, 0).await;
}

/// `onLevelUp(ctx)`.
pub async fn on_level_up(app: &App, guild_id: &str, settings: Option<Settings>, user_id: &str, member: Option<Member>, new_level: i64, channel: Option<ChannelId>) {
    let Some(idx) = get_achievement_index(app, guild_id).await else { return };
    if user_id.is_empty() || new_level < 1 {
        return;
    }
    if member.as_ref().is_some_and(|m| m.user.bot) {
        return;
    }
    let list = active_achievements(idx.get("LEVEL"), now_ms(app));
    if list.is_empty() {
        return;
    }
    // The member can be absent (left, or a bulk role give); everything works from the id.
    let o = Origin { guild_id: guild_id.to_string(), settings, user_id: user_id.to_string(), member, channel };
    record_progress(app, &o, "LEVEL", &list, 0, new_level.min(PROGRESS_MAX)).await;
}

/// `onPurchase(ctx)`.
pub async fn on_purchase(app: &App, guild_id: &str, settings: Option<Settings>, user_id: &str, member: Option<Member>, quantity: i64, channel: Option<ChannelId>) {
    let Some(idx) = get_achievement_index(app, guild_id).await else { return };
    if user_id.is_empty() {
        return;
    }
    let list = active_achievements(idx.get("PURCHASES"), now_ms(app));
    if list.is_empty() {
        return;
    }
    let q = if quantity > 0 { quantity.min(MAX_PURCHASE_DELTA) } else { 1 };
    let o = Origin { guild_id: guild_id.to_string(), settings, user_id: user_id.to_string(), member, channel };
    record_progress(app, &o, "PURCHASES", &list, q, 0).await;
}

// ── Data functions for the commands ───────────────────────────────────────────

/// `listAchievements(guildId)`: every achievement of the guild (enabled or not), oldest first.
pub async fn list_achievements(pool: &bot_db::PgPool, guild_id: &str) -> Result<Vec<Achievement>, sqlx::Error> {
    sqlx::query_as(SELECT_ACHIEVEMENTS).bind(guild_id).fetch_all(pool).await
}

/// `/^[a-z0-9]{20,40}$/i`.
fn looks_like_id(s: &str) -> bool {
    (20..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `resolveAchievement(guildId, input)`: an autocomplete value (an id) or a typed name (Prisma `equals` with `mode:
/// 'insensitive'`, which is `ILIKE`).
pub async fn resolve_achievement(pool: &bot_db::PgPool, guild_id: &str, input: &str) -> Result<Option<Achievement>, sqlx::Error> {
    let s = bot_util::js::trim(input);
    if s.is_empty() {
        return Ok(None);
    }
    if looks_like_id(s) {
        let by_id: Option<Achievement> = sqlx::query_as(r#"SELECT * FROM "Achievement" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#).bind(s).bind(guild_id).fetch_optional(pool).await?;
        if by_id.is_some() {
            return Ok(by_id);
        }
    }
    sqlx::query_as(r#"SELECT * FROM "Achievement" WHERE "guildId" = $1 AND "name" ILIKE $2 LIMIT 1"#).bind(guild_id).bind(s).fetch_optional(pool).await
}

/// `nameTaken(guildId, name, exceptId)`: case-insensitive duplicate check.
pub async fn name_taken(pool: &bot_db::PgPool, guild_id: &str, name: &str, except_id: Option<&str>) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(r#"SELECT "id" FROM "Achievement" WHERE "guildId" = $1 AND "name" ILIKE $2 AND ($3::text IS NULL OR "id" <> $3) LIMIT 1"#)
        .bind(guild_id)
        .bind(name)
        .bind(except_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// An autocomplete row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRow {
    pub id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub trigger: String,
    pub enabled: bool,
}

/// `searchAchievements(guildId, focused)`: autocomplete rows, filtered in the database. `[]` on error.
pub async fn search_achievements(pool: &bot_db::PgPool, guild_id: &str, focused: &str) -> Vec<SearchRow> {
    let r = sqlx::query(r#"SELECT "id", "name", "emoji", "trigger"::text AS "trigger", "enabled" FROM "Achievement" WHERE "guildId" = $1 AND "name" ILIKE $2 ORDER BY "name" ASC LIMIT 25"#)
        .bind(guild_id)
        .bind(format!("%{focused}%"))
        .fetch_all(pool)
        .await;
    let Ok(rows) = r else { return Vec::new() };
    rows.iter()
        .filter_map(|r| {
            Some(SearchRow {
                id: r.try_get("id").ok()?,
                name: r.try_get("name").ok()?,
                emoji: r.try_get("emoji").ok()?,
                trigger: r.try_get("trigger").ok()?,
                enabled: r.try_get("enabled").ok()?,
            })
        })
        .collect()
}

/// What [`create_achievement`] stores.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAchievement {
    pub name: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub trigger: String,
    pub track_past_progress: bool,
    pub deadline: Option<DateTime<Utc>>,
    /// Tiers as given (`[{ goal, rewards }]`), serialised on write.
    pub tiers: Value,
}

/// Why a create or update was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteRefusal {
    Limit,
    NameTaken,
    NotFound,
}

/// `createAchievement(guildId, data)`: creates an achievement after the cap and the name check. Call inside
/// [`with_config_lock`], then refresh the cache.
pub async fn create_achievement(app: &App, guild_id: &str, data: &NewAchievement) -> anyhow::Result<Result<Achievement, WriteRefusal>> {
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Achievement" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(app.db()).await?;
    if count >= limits::ACHIEVEMENTS as i64 {
        return Ok(Err(WriteRefusal::Limit));
    }
    if name_taken(app.db(), guild_id, &data.name, None).await? {
        return Ok(Err(WriteRefusal::NameTaken));
    }
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let deadline = match data.deadline {
        Some(d) => SqlValue::timestamp(d),
        None => bot_db::query::null_of(ACHIEVEMENT_INFO.columns.iter().find(|c| c.name == "deadline").ok_or_else(|| anyhow::anyhow!("no deadline column"))?),
    };
    let q = InsertBuilder::new(&ACHIEVEMENT_INFO)
        .values([
            ("guildId", json!(guild_id)),
            ("name", json!(data.name)),
            ("description", json!(data.description)),
            ("emoji", json!(data.emoji)),
            ("trigger", json!(data.trigger)),
            ("trackPastProgress", json!(data.track_past_progress)),
            ("tiers", tiers_json(&serialize_tiers(&data.tiers))),
        ])?
        .typed("deadline", deadline)?
        .build()?;
    match q.fetch_one::<Achievement, _>(app.db()).await {
        Ok(a) => Ok(Ok(a)),
        Err(e) if e.is_unique_violation() => Ok(Err(WriteRefusal::NameTaken)),
        Err(e) => Err(e.into()),
    }
}

/// The scalar fields `/achievement-config edit`, `enable` and `disable` change (never the trigger). `Some(None)`
/// clears a nullable field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AchievementPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub emoji: Option<Option<String>>,
    pub track_past_progress: Option<bool>,
    pub deadline: Option<Option<DateTime<Utc>>>,
    pub enabled: Option<bool>,
}

impl AchievementPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `updateAchievement(guildId, id, data)`. Call inside [`with_config_lock`], then refresh.
pub async fn update_achievement(pool: &bot_db::PgPool, guild_id: &str, id: &str, patch: &AchievementPatch) -> anyhow::Result<Result<(), WriteRefusal>> {
    let mut sets: Vec<String> = Vec::new();
    let mut n = 0usize;
    let mut next = || {
        n += 1;
        n
    };
    let mut q_sets = Vec::new();
    if patch.name.is_some() {
        q_sets.push(format!("\"name\" = ${}", next()));
    }
    if patch.description.is_some() {
        q_sets.push(format!("\"description\" = ${}", next()));
    }
    if patch.emoji.is_some() {
        q_sets.push(format!("\"emoji\" = ${}", next()));
    }
    if patch.track_past_progress.is_some() {
        q_sets.push(format!("\"trackPastProgress\" = ${}", next()));
    }
    if patch.deadline.is_some() {
        q_sets.push(format!("\"deadline\" = ${}", next()));
    }
    if patch.enabled.is_some() {
        q_sets.push(format!("\"enabled\" = ${}", next()));
    }
    sets.extend(q_sets);
    sets.push(format!("\"updatedAt\" = {SQL_NOW}"));
    let (pid, pguild) = (next(), next());
    let sql = format!(r#"UPDATE "Achievement" SET {} WHERE "id" = ${pid} AND "guildId" = ${pguild}"#, sets.join(", "));
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    if let Some(v) = &patch.name {
        q = q.bind(v.clone());
    }
    if let Some(v) = &patch.description {
        q = q.bind(v.clone());
    }
    if let Some(v) = &patch.emoji {
        q = q.bind(v.clone());
    }
    if let Some(v) = patch.track_past_progress {
        q = q.bind(v);
    }
    if let Some(v) = patch.deadline {
        q = q.bind(v.map(|d| d.naive_utc()));
    }
    if let Some(v) = patch.enabled {
        q = q.bind(v);
    }
    match q.bind(id).bind(guild_id).execute(pool).await {
        Ok(r) if r.rows_affected() == 0 => Ok(Err(WriteRefusal::NotFound)),
        Ok(_) => Ok(Ok(())),
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505") => Ok(Err(WriteRefusal::NameTaken)),
        Err(e) => Err(e.into()),
    }
}

/// `deleteAchievement(guildId, id)`: the cascade removes its progress rows. Returns the rows deleted.
pub async fn delete_achievement(pool: &bot_db::PgPool, guild_id: &str, id: &str) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(r#"DELETE FROM "Achievement" WHERE "id" = $1 AND "guildId" = $2"#).bind(id).bind(guild_id).execute(pool).await?.rows_affected())
}

/// One `/achievements` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberEntry {
    pub a: Lite,
    pub enabled: bool,
    pub row: Option<bot_db::AchievementProgress>,
    pub display: DisplayProgress,
}

/// `loadMemberView(guildId, userId, member)`: everything `/achievements` needs for one member. `None` on a database
/// error. `boosting` is the member's `premiumSinceTimestamp` being set.
pub async fn load_member_view(app: &App, guild_id: &str, user_id: &str, boosting: bool) -> Option<Vec<MemberEntry>> {
    let pool = app.db();
    let rows = list_achievements(pool, guild_id);
    let progress = sqlx::query_as::<_, bot_db::AchievementProgress>(r#"SELECT * FROM "AchievementProgress" WHERE "guildId" = $1 AND "userId" = $2"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_all(pool);
    let ul = read_user_level(app, guild_id, user_id);
    let (rows, progress, ul) = tokio::join!(rows, progress, ul);
    let (rows, progress, ul) = match (rows, progress, ul) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (a, b, c) => {
            let e = a.err().or(b.err()).or(c.err());
            bot_util::log_warn!("achievements: member view failed", e.map(|e| bot_core::error::log_arg(&e.into())).unwrap_or_else(|| "".into()));
            return None;
        }
    };
    let by_ach: HashMap<&str, &bot_db::AchievementProgress> = progress.iter().map(|r| (r.achievement_id.as_str(), r)).collect();
    let mut entries = Vec::new();
    for row in &rows {
        let p = by_ach.get(row.id.as_str()).copied();
        if !row.enabled && !p.is_some_and(|p| p.tier >= 1) {
            continue; // disabled achievements show only when something was unlocked
        }
        let a = to_lite(row);
        if a.tiers.is_empty() {
            continue;
        }
        let display = display_progress(&a, p.map(|p| (i64::from(p.progress), i64::from(p.tier))), ul, boosting);
        entries.push(MemberEntry { a, enabled: row.enabled, row: p.cloned(), display });
    }
    Some(entries)
}

/// `getTierStats(achievementId, tierCount)`: `per_tier[i]` counts rows that unlocked tier i (tier >= i + 1).
pub async fn get_tier_stats(pool: &bot_db::PgPool, achievement_id: &str, tier_count: usize) -> Result<(Vec<i64>, i64), sqlx::Error> {
    let groups = sqlx::query(r#"SELECT COUNT(*) AS "n", "tier" FROM "AchievementProgress" WHERE "achievementId" = $1 GROUP BY "tier""#).bind(achievement_id).fetch_all(pool).await?;
    let groups: Vec<(i64, i64)> = groups.iter().map(|g| Ok((i64::from(g.try_get::<i32, _>("tier")?), g.try_get::<i64, _>("n")?))).collect::<Result<_, sqlx::Error>>()?;
    let per_tier = (0..tier_count).map(|i| groups.iter().filter(|(t, _)| *t > i as i64).map(|(_, n)| n).sum()).collect();
    let total = groups.iter().map(|(_, n)| n).sum();
    Ok((per_tier, total))
}

/// `resetMemberProgress(guildId, userId, achievementIds)`: zeroes a member's progress (rows are zeroed, never deleted,
/// so they are not re-seeded from history). Rewards already given are not taken back.
pub async fn reset_member_progress(app: &App, guild_id: &str, user_id: &str, achievement_ids: &[String]) -> anyhow::Result<()> {
    let mut ids: Vec<String> = Vec::new();
    for id in achievement_ids {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        return Ok(());
    }
    // Messages still waiting in the buffer were sent before the reset: write them first.
    settle_member(app, guild_id, user_id).await;
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let (g, u, list) = (guild_id.to_string(), user_id.to_string(), ids.clone());
    bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            let sql = format!(
                r#"UPDATE "AchievementProgress" SET "progress" = 0, "tier" = 0, "almostNotified" = false, "completedAt" = NULL, "updatedAt" = {SQL_NOW} WHERE "guildId" = $1 AND "userId" = $2 AND "achievementId" = ANY($3::text[])"#
            );
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&g).bind(&u).bind(&list).execute(&mut *tx).await.map_err(DbError::from)?;
            let new_ids: Vec<String> = list.iter().map(|_| bot_db::new_cuid()).collect();
            let sql = format!(
                r#"INSERT INTO "AchievementProgress" ("id", "guildId", "userId", "achievementId", "createdAt", "updatedAt")
                SELECT t.id, $1, $2, t.aid, {SQL_NOW}, {SQL_NOW} FROM unnest($3::text[], $4::text[]) AS t(id, aid)
                ON CONFLICT DO NOTHING"#
            );
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&g).bind(&u).bind(&new_ids).bind(&list).execute(&mut *tx).await.map_err(DbError::from)?;
            Ok::<_, DbError>(())
        })
    })
    .await?;
    for id in &ids {
        mark_seeded(app, id, user_id);
    }
    Ok(())
}

/// The kind of tier change [`apply_tier_change`] makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierChange {
    Add,
    Edit { index: usize, goal_changed: bool },
    Remove { index: usize },
}

/// `applyTierChange(achievementId, nextTiers, change)`: saves new tiers together with the matching progress fix-ups
/// in one transaction. Tiers are never revoked.
pub async fn apply_tier_change(pool: &bot_db::PgPool, achievement_id: &str, next_tiers: &[Tier], change: TierChange) -> anyhow::Result<()> {
    let tiers = tiers_json(&serialize_tiers(&tiers_json(next_tiers)));
    let n = next_tiers.len() as i32;
    let id = achievement_id.to_string();
    bot_db::transaction(pool, bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            let run = |sql: String| sqlx::query(sqlx::AssertSqlSafe(sql));
            run(format!(r#"UPDATE "Achievement" SET "tiers" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2"#)).bind(&tiers).bind(&id).execute(&mut *tx).await.map_err(DbError::from)?;
            match change {
                TierChange::Add => {
                    // Progress resumes toward the new top tier.
                    run(format!(r#"UPDATE "AchievementProgress" SET "completedAt" = NULL, "updatedAt" = {SQL_NOW} WHERE "achievementId" = $1 AND "completedAt" IS NOT NULL"#))
                        .bind(&id)
                        .execute(&mut *tx)
                        .await
                        .map_err(DbError::from)?;
                }
                TierChange::Remove { index } => {
                    let k = index as i32;
                    // Members who had passed the removed tier lose one count (remapTierAfterRemoval).
                    run(format!(r#"UPDATE "AchievementProgress" SET "tier" = ("tier" - 1), "updatedAt" = {SQL_NOW} WHERE "achievementId" = $1 AND "tier" > $2"#))
                        .bind(&id)
                        .bind(k)
                        .execute(&mut *tx)
                        .await
                        .map_err(DbError::from)?;
                    run(format!(r#"UPDATE "AchievementProgress" SET "completedAt" = $3, "updatedAt" = {SQL_NOW} WHERE "achievementId" = $1 AND "tier" >= $2 AND "completedAt" IS NULL"#))
                        .bind(&id)
                        .bind(n)
                        .bind(Utc::now().naive_utc())
                        .execute(&mut *tx)
                        .await
                        .map_err(DbError::from)?;
                    run(format!(r#"UPDATE "AchievementProgress" SET "almostNotified" = false, "updatedAt" = {SQL_NOW} WHERE "achievementId" = $1 AND "tier" = $2"#))
                        .bind(&id)
                        .bind(k)
                        .execute(&mut *tx)
                        .await
                        .map_err(DbError::from)?;
                }
                TierChange::Edit { index, goal_changed: true } => {
                    run(format!(r#"UPDATE "AchievementProgress" SET "almostNotified" = false, "updatedAt" = {SQL_NOW} WHERE "achievementId" = $1 AND "tier" = $2"#))
                        .bind(&id)
                        .bind(index as i32)
                        .execute(&mut *tx)
                        .await
                        .map_err(DbError::from)?;
                }
                TierChange::Edit { .. } => {}
            }
            Ok::<_, DbError>(())
        })
    })
    .await?;
    Ok(())
}

/// Epoch ms as a UTC date.
pub fn date_of_ms(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}
