//! Port of `src/services/statsService.js`: statistics channels (MEE6 Statistics Channels). Locked voice channels whose
//! NAME shows a live count (members, humans, bots, online, peak online, channel counts, roles, role members, boosts).
//! Constants, kind metadata, pure helpers, the refresh engine, the `stats.refresh` job body, the hook bodies and the CRUD
//! `/stats` calls. Redis is deliberately not used (Postgres is authoritative).
//!
//! Rename budget: Discord allows about 2 renames per 10 minutes per channel. Each counter is renamed at most once per
//! [`MIN_RENAME_GAP_MS`], tracked in `StatChannel.lastRenamedAt` and reserved atomically BEFORE the request. A change
//! that cannot be applied yet is never dropped: the blocked channel's earliest retry time becomes the job's next run.
//!
//! The JS module-level maps live in [`StatsState`] (`app.ext`); Discord goes through [`crate::stats_io`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use bot_core::App;
use bot_core::guild_members::FetchFailure;
use bot_core::scheduler::{CancelJobs, OnConflict, ScheduleJob};
use bot_db::{DbError, InsertBuilder, JobStatus, STAT_CHANNEL_INFO, SelectBuilder, StatChannel, StatKind};
use bot_util::placeholder_parser::{PlaceholderGuild, RenderContext, VarValue, render_placeholders};
use bot_util::text::{clamp_text, format_number};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;

use crate::discord_io::{CappedMap, DiscordError, js_random};
use crate::stats_io::{CreateVoiceChannel, Overwrite, StatsChannel, StatsGuild, stats_io};

macro_rules! sql_now {
    () => {
        "(now() AT TIME ZONE 'UTC')"
    };
}

// ── Constants ─────────────────────────────────────────────────────────────────

pub const REFRESH_INTERVAL_MS: i64 = 10 * 60_000;
pub const MIN_RENAME_GAP_MS: i64 = 5 * 60_000;
/// Coalesced pull after a join or leave.
pub const MEMBER_PULL_DELAY_MS: i64 = 60_000;
/// Never reschedule sooner than this.
pub const MIN_NEXT_RUN_MS: i64 = 60_000;
pub const EDIT_PULL_DELAY_MS: i64 = 5_000;
/// guildReady: first run at +30 s ... +120 s.
pub const STARTUP_DELAY_MS: i64 = 30_000;
pub const STARTUP_JITTER_MS: i64 = 90_000;
/// No new rename starts after this (the job timeout is 60 s).
pub const REFRESH_BUDGET_MS: i64 = 40_000;
pub const RENAME_TIMEOUT_MS: u64 = 10_000;
/// `fetchAllMembers` timeoutMs.
pub const MEMBER_FETCH_TIMEOUT_MS: u64 = 20_000;
pub const MEMBER_FETCH_RETRY_MS: i64 = 5 * 60_000;
pub const GUILD_FETCH_TIMEOUT_MS: u64 = 8_000;
/// The REST check of a channel missing from the cache.
pub const CHANNEL_FETCH_TIMEOUT_MS: u64 = 5_000;
/// Above this Discord sends no presences at startup.
pub const PRESENCE_CACHE_MAX_MEMBERS: u64 = 75_000;
/// HUMANS / BOTS / ROLE_MEMBERS refused above this.
pub const MEMBER_LIST_MAX_MEMBERS: u64 = 100_000;
/// Discord's channel-name limit.
pub const MAX_NAME_LENGTH: usize = 100;
pub const WORST_CASE_COUNT: i64 = 2_147_483_647;
pub const ROWS_MEMO_TTL_MS: i64 = 15 * 60_000;
pub const WARN_THROTTLE_MS: u64 = 6 * 60 * 60_000;
pub const MEMO_CAP: usize = 10_000;
pub const RENAME_REASON: &str = "Statistics channel update";
/// A channel that could not be checked is looked at again after this.
const UNKNOWN_RETRY_MS: i64 = 2 * 60_000;
/// A pull claimed during a run is honoured no sooner than now + this.
const PULL_FLOOR_MS: i64 = 5_000;
const INT_MAX: i64 = 2_147_483_647;

// ── Kinds ─────────────────────────────────────────────────────────────────────

/// `STAT_KINDS`: the Prisma enum `StatKind`, same order.
pub const STAT_KINDS: &[StatKind] = StatKind::ALL;

/// `KIND_INFO[kind]`: `(label, default template)`.
pub fn kind_info(kind: StatKind) -> (&'static str, &'static str) {
    match kind {
        StatKind::Members => ("Members", "Members: {count}"),
        StatKind::Humans => ("Humans", "Humans: {count}"),
        StatKind::Bots => ("Bots", "Bots: {count}"),
        StatKind::Online => ("Online", "Online: {count}"),
        StatKind::PeakOnline => ("Peak online", "Peak Online: {count}"),
        StatKind::TextChannels => ("Text channels", "Text Channels: {count}"),
        StatKind::VoiceChannels => ("Voice channels", "Voice Channels: {count}"),
        StatKind::TotalChannels => ("All channels", "Channels: {count}"),
        StatKind::Roles => ("Roles", "Roles: {count}"),
        StatKind::RoleMembers => ("Role members", "{role}: {count}"),
        StatKind::Boosts => ("Boosts", "Boosts: {count}"),
    }
}

/// `LIST_KINDS`: kinds that need the full member list.
pub fn is_list_kind(kind: StatKind) -> bool {
    matches!(kind, StatKind::Humans | StatKind::Bots | StatKind::RoleMembers)
}

/// `ONLINE_KINDS`.
pub fn is_online_kind(kind: StatKind) -> bool {
    matches!(kind, StatKind::Online | StatKind::PeakOnline)
}

/// `MEMBER_EVENT_KINDS`: kinds a join or leave can change.
pub fn is_member_event_kind(kind: StatKind) -> bool {
    matches!(kind, StatKind::Members | StatKind::Humans | StatKind::Bots | StatKind::Online | StatKind::PeakOnline | StatKind::RoleMembers)
}

/// `TEXT_TYPES`: text, announcement, forum, media.
pub const TEXT_TYPES: [u8; 4] = [0, 5, 15, 16];
/// `VOICE_TYPES`: voice, stage.
pub const VOICE_TYPES: [u8; 2] = [2, 13];
const CATEGORY: u8 = 4;
const VOICE: u8 = 2;

/// `PROBLEMS`.
pub const PROBLEMS: &[(&str, &str)] = &[
    ("members-incomplete", "Loading the member list; the count will update shortly"),
    ("server-too-large", "This server is too large for member-list counters"),
    ("role-deleted", "The counted role was deleted; remove this counter with /stats delete"),
    ("presence-unavailable", "Online data is unavailable right now"),
    ("missing-permissions", "I need View Channel and Manage Channels on this channel"),
    ("channel-deleted", "I can't find this channel; the counter is removed once Discord confirms it was deleted"),
    ("channel-unknown", "I could not check this channel just now; it will be tried again"),
    ("error", "The last update failed; it will be retried"),
];

/// `describeProblem(code)`: the text of a `PROBLEMS` key (`error` for an unknown key).
pub fn describe_problem(code: &str) -> &'static str {
    PROBLEMS.iter().find(|(k, _)| *k == code).or_else(|| PROBLEMS.iter().find(|(k, _)| *k == "error")).map(|(_, v)| *v).unwrap_or("")
}

/// `StatsError(code, extra)`: LIMIT, ROLE_LIMIT, DUPLICATE (with the channel id), MISSING_PERMISSIONS, CHANNEL_LIMIT,
/// DISCORD_REJECTED, NOT_FOUND.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsError {
    pub code: &'static str,
    pub channel_id: Option<String>,
}

impl StatsError {
    pub fn new(code: &'static str) -> Self {
        Self { code, channel_id: None }
    }
}

impl std::fmt::Display for StatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for StatsError {}

// ── In-process state (per shard; empty after a restart) ───────────────────────

/// `{ id, channelId, kind, roleId }`: the memoised view of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLite {
    pub id: String,
    pub channel_id: String,
    pub kind: StatKind,
    pub role_id: Option<String>,
}

impl From<&StatChannel> for RowLite {
    fn from(r: &StatChannel) -> Self {
        Self { id: r.id.clone(), channel_id: r.channel_id.clone(), kind: r.kind, role_id: r.role_id.clone() }
    }
}

/// The module-level state of the JS service, one per app.
pub struct StatsState {
    rows_memo: Mutex<CappedMap<(i64, Vec<RowLite>)>>,
    pending_pull: Mutex<CappedMap<i64>>,
    busy_guilds: Mutex<HashSet<String>>,
    fetch_blocked_until: Mutex<CappedMap<i64>>,
    name_aliases: Mutex<CappedMap<(String, String)>>,
    warn_once: RwLock<Arc<bot_util::async_util::Throttle>>,
    config_mutex: RwLock<bot_core::sync::KeyedMutex>,
}

impl Default for StatsState {
    fn default() -> Self {
        Self {
            rows_memo: Mutex::new(CappedMap::new(MEMO_CAP)),
            pending_pull: Mutex::new(CappedMap::new(MEMO_CAP)),
            busy_guilds: Mutex::new(HashSet::new()),
            fetch_blocked_until: Mutex::new(CappedMap::new(MEMO_CAP)),
            name_aliases: Mutex::new(CappedMap::new(MEMO_CAP)),
            warn_once: RwLock::new(Arc::new(bot_util::async_util::create_throttle(WARN_THROTTLE_MS))),
            config_mutex: RwLock::new(bot_core::sync::KeyedMutex::new()),
        }
    }
}

fn state(app: &App) -> Arc<StatsState> {
    app.ext::<StatsState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn warn_once(app: &App, key: &str) -> bool {
    let t = state(app).warn_once.read().unwrap_or_else(|e| e.into_inner()).clone();
    t.allow(key)
}

fn config_mutex(app: &App) -> bot_core::sync::KeyedMutex {
    state(app).config_mutex.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn now(app: &App) -> i64 {
    app.clock().now_ms()
}

fn prime_memo(app: &App, guild_id: &str, rows: &[StatChannel]) {
    let at = now(app);
    lock(&state(app).rows_memo).set(guild_id, (at, rows.iter().map(RowLite::from).collect()));
}

fn memo_add_row(app: &App, guild_id: &str, row: &StatChannel) {
    let st = state(app);
    let mut memo = lock(&st.rows_memo);
    if let Some((_, rows)) = memo.get_mut(guild_id) {
        rows.retain(|r| r.id != row.id);
        rows.push(RowLite::from(row));
    }
}

fn forget_row(app: &App, guild_id: &str, row_id: &str) {
    let st = state(app);
    if let Some((_, rows)) = lock(&st.rows_memo).get_mut(guild_id) {
        rows.retain(|r| r.id != row_id);
    }
}

fn fresh_memo(app: &App, guild_id: &str) -> Option<Vec<RowLite>> {
    let st = state(app);
    let memo = lock(&st.rows_memo);
    let (at, rows) = memo.get(guild_id)?;
    if now(app) - at <= ROWS_MEMO_TTL_MS { Some(rows.clone()) } else { None }
}

/// `__setRowsMemo(guildId, rows, at)`: test seam.
pub fn set_rows_memo(app: &App, guild_id: &str, rows: Vec<RowLite>, at: Option<i64>) {
    let at = at.unwrap_or_else(|| now(app));
    lock(&state(app).rows_memo).set(guild_id, (at, rows));
}

/// `__resetState()`: clears every in-process map and set and recreates the throttle and the mutex (tests).
pub fn reset_state(app: &App) {
    let st = state(app);
    lock(&st.rows_memo).clear();
    lock(&st.pending_pull).clear();
    lock(&st.busy_guilds).clear();
    lock(&st.fetch_blocked_until).clear();
    lock(&st.name_aliases).clear();
    *st.warn_once.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(bot_util::async_util::create_throttle(WARN_THROTTLE_MS));
    *st.config_mutex.write().unwrap_or_else(|e| e.into_inner()) = bot_core::sync::KeyedMutex::new();
}

/// The pending pull of a guild (test inspection of `pendingPull`).
pub fn pending_pull_of(app: &App, guild_id: &str) -> Option<i64> {
    lock(&state(app).pending_pull).get(guild_id).copied()
}

/// The fetch claim of a guild (test inspection of `fetchBlockedUntil`).
pub fn fetch_blocked_until_of(app: &App, guild_id: &str) -> Option<i64> {
    lock(&state(app).fetch_blocked_until).get(guild_id).copied()
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// `clampInt(n)`: an integer in 0 ... 2_147_483_647 (the Int column range).
pub fn clamp_int(n: f64) -> i64 {
    let t = if n.is_nan() { 0.0 } else { n.trunc() };
    t.clamp(0.0, INT_MAX as f64) as i64
}

/// `refreshJobKey(guildId)`.
pub fn refresh_job_key(guild_id: &str) -> String {
    format!("stats.refresh:{guild_id}")
}

/// `normalizeTemplate(str)`: runs of whitespace become one space, trimmed.
pub fn normalize_template(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    bot_util::js::trim(&out).to_string()
}

fn render(template: &str, guild: Option<&PlaceholderGuild>, count: &str, role: Option<&str>) -> String {
    let mut vars: Vec<(String, VarValue)> = vec![("count".into(), VarValue::Text(count.to_string()))];
    if let Some(r) = role {
        vars.push(("role".into(), VarValue::Text(r.to_string())));
    }
    render_placeholders(template, &RenderContext { guild: guild.cloned(), vars: Some(vars), ..Default::default() }, None)
}

/// `renderStatName(template, { count, roleName, guild })`: the channel name for a template and a count, 1-100
/// characters. A missing count renders as an ellipsis (creation only).
pub fn render_stat_name(template: &str, count: Option<i64>, role_name: Option<&str>, guild: Option<&PlaceholderGuild>) -> String {
    let count = match count {
        Some(c) => format_number(clamp_int(c as f64) as f64),
        None => "…".to_string(),
    };
    let text = render(template, guild, &count, role_name);
    let name = clamp_text(&normalize_template(&text), MAX_NAME_LENGTH);
    if name.is_empty() { count } else { name }
}

fn token_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\{[^{}\n]{1,64}\}").unwrap_or_else(|e| panic!("{e}")))
}

/// `validateStatTemplate(input, { kind, roleName, guild })`: the normalised template, or the error to show.
pub fn validate_stat_template(input: &str, kind: StatKind, role_name: Option<&str>, guild: Option<&PlaceholderGuild>) -> Result<String, String> {
    let t = normalize_template(input);
    let len = bot_util::js::utf16_len(&t);
    if !(1..=MAX_NAME_LENGTH).contains(&len) {
        return Err("Templates must be 1–100 characters.".into());
    }
    let lower = t.to_lowercase();
    if !lower.contains("{count}") {
        return Err("The template must contain `{count}`, e.g. `Members: {count}`.".into());
    }
    if lower.contains("{random") {
        return Err("Random variables are not allowed: the name would change on every refresh.".into());
    }
    if lower.contains("{role}") && kind != StatKind::RoleMembers {
        return Err("`{role}` only works with the **Role members** counter.".into());
    }
    let count = format_number(WORST_CASE_COUNT as f64);
    let role = if kind == StatKind::RoleMembers { Some(role_name.unwrap_or("role")) } else { None };
    // Tokens come from the TEMPLATE, not the output, so braces inside a role or server name never cause a false error.
    for m in token_re().find_iter(&t) {
        let whole = m.as_str();
        if render(whole, guild, &count, role) == whole {
            return Err(format!(
                "Unknown variable `{whole}`. Use `{{count}}`, `{{role}}` (role counters) or server variables such as `{{server}}`."
            ));
        }
    }
    let n = bot_util::js::utf16_len(&normalize_template(&render(&t, guild, &count, role)));
    if n > MAX_NAME_LENGTH {
        return Err(format!("With a large count the name would be {n} characters; keep it to 100 or fewer."));
    }
    Ok(t)
}

/// `isMemberCacheComplete(guild)`: the member cache holds every member.
pub fn is_member_cache_complete(guild: &StatsGuild) -> bool {
    guild.members.len() as u64 >= guild.member_count
}

/// `countChannels(channels)`: `(text, voice, total)`; threads are skipped, total excludes categories.
pub fn count_channels(channels: &[StatsChannel]) -> (i64, i64, i64) {
    let (mut text, mut voice, mut total) = (0, 0, 0);
    for c in channels {
        if c.is_thread {
            continue;
        }
        if TEXT_TYPES.contains(&c.kind) {
            text += 1;
        }
        if VOICE_TYPES.contains(&c.kind) {
            voice += 1;
        }
        if c.kind != CATEGORY {
            total += 1;
        }
    }
    (text, voice, total)
}

/// Everything a counter can show (`buildSnapshot`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub members: i64,
    pub members_complete: bool,
    pub humans: Option<i64>,
    pub bots: Option<i64>,
    pub online: Option<i64>,
    pub text: i64,
    pub voice: i64,
    pub total: i64,
    pub roles: i64,
    pub boosts: i64,
    pub role_counts: HashMap<String, i64>,
    pub roles_missing: HashSet<String>,
}

/// `buildSnapshot(guild, { roleIds, approxOnline, presences })`, from the caches only. `presences` is false when the
/// bot runs without the Presence intent, so the online count is always Discord's approximate one.
pub fn build_snapshot(guild: &StatsGuild, role_ids: &[String], approx_online: Option<i64>, presences: bool) -> Snapshot {
    let members = guild.member_count as i64;
    let members_complete = is_member_cache_complete(guild);
    let mut humans = None;
    let mut bots = None;
    let mut role_counts = HashMap::new();
    if members_complete && guild.member_count <= MEMBER_LIST_MAX_MEMBERS {
        let (mut h, mut b) = (0, 0);
        for id in role_ids {
            role_counts.insert(id.clone(), 0);
        }
        for m in &guild.members {
            if m.bot {
                b += 1;
            } else {
                h += 1;
            }
            for id in role_ids {
                if m.roles.contains(id) {
                    *role_counts.entry(id.clone()).or_insert(0) += 1;
                }
            }
        }
        humans = Some(h);
        bots = Some(b);
    }
    let online = if presences && guild.member_count <= PRESENCE_CACHE_MAX_MEMBERS {
        Some(guild.presences.iter().filter(|s| s.as_str() != "offline").count() as i64)
    } else {
        approx_online
    };
    let (text, voice, total) = count_channels(&guild.channels);
    Snapshot {
        members,
        members_complete,
        humans,
        bots,
        online,
        text,
        voice,
        total,
        roles: (guild.roles.len() as i64 - 1).max(0),
        boosts: guild.boosts as i64,
        role_counts,
        roles_missing: role_ids.iter().filter(|id| !guild.has_role(id)).cloned().collect(),
    }
}

/// The value a row should show, or why it cannot be known right now (`computeValue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Computed {
    pub value: Option<i64>,
    pub problem: Option<&'static str>,
}

/// `computeValue(kind, snap, row)`: `row` is `(roleId, peakValue)`.
pub fn compute_value(kind: StatKind, snap: &Snapshot, role_id: Option<&str>, peak_value: Option<i64>) -> Computed {
    let ok = |v: i64| Computed { value: Some(clamp_int(v as f64)), problem: None };
    let bad = |p: &'static str| Computed { value: None, problem: Some(p) };
    let list = |v: Option<i64>| {
        if snap.members as u64 > MEMBER_LIST_MAX_MEMBERS {
            return bad("server-too-large");
        }
        if !snap.members_complete {
            return bad("members-incomplete");
        }
        ok(v.unwrap_or(0))
    };
    match kind {
        StatKind::Members => ok(snap.members),
        StatKind::Humans => list(snap.humans),
        StatKind::Bots => list(snap.bots),
        StatKind::Online => match snap.online {
            None => bad("presence-unavailable"),
            Some(o) => ok(o),
        },
        StatKind::PeakOnline => match snap.online {
            None => match peak_value {
                None => bad("presence-unavailable"),
                Some(p) => ok(p),
            },
            Some(o) => ok(peak_value.unwrap_or(0).max(o)),
        },
        StatKind::TextChannels => ok(snap.text),
        StatKind::VoiceChannels => ok(snap.voice),
        StatKind::TotalChannels => ok(snap.total),
        StatKind::Roles => ok(snap.roles),
        StatKind::RoleMembers => {
            let Some(r) = role_id.filter(|r| !r.is_empty() && !snap.roles_missing.contains(*r)) else { return bad("role-deleted") };
            if snap.members as u64 > MEMBER_LIST_MAX_MEMBERS {
                return bad("server-too-large");
            }
            if !snap.members_complete {
                return bad("members-incomplete");
            }
            ok(snap.role_counts.get(r).copied().unwrap_or(0))
        }
        StatKind::Boosts => ok(snap.boosts),
    }
}

/// Whether the channel may be renamed now (`decideRename`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameDecision {
    /// The name is already right ('same' also covers a name Discord normalised on an earlier rename).
    Same,
    Rename,
    Wait { retry_at: i64 },
}

/// `decideRename({ desiredName, currentName, alias, lastRenamedAt, now })`; `alias` is `(desired, actual)`.
pub fn decide_rename(desired: &str, current: &str, alias: Option<&(String, String)>, last_renamed_at: Option<i64>, now: i64) -> RenameDecision {
    if current == desired {
        return RenameDecision::Same;
    }
    if let Some((d, a)) = alias
        && d == desired
        && a == current
    {
        return RenameDecision::Same;
    }
    match last_renamed_at {
        None => RenameDecision::Rename,
        Some(t) if now - t >= MIN_RENAME_GAP_MS => RenameDecision::Rename,
        Some(t) => RenameDecision::Wait { retry_at: t + MIN_RENAME_GAP_MS },
    }
}

/// `computeNextRunAt({ now, retryAts, soon })`: every [`REFRESH_INTERVAL_MS`], earlier when a rename becomes allowed
/// (+5 s) or when `soon`, and never sooner than [`MIN_NEXT_RUN_MS`] from now. Epoch ms.
pub fn compute_next_run_at(now: i64, retry_ats: &[i64], soon: bool) -> i64 {
    let mut t = now + REFRESH_INTERVAL_MS;
    for r in retry_ats {
        t = t.min(r + 5_000);
    }
    if soon {
        t = t.min(now + MIN_NEXT_RUN_MS);
    }
    t.max(now + MIN_NEXT_RUN_MS)
}

/// `parseChannelRef(input)`: the channel id of a mention or a raw id.
pub fn parse_channel_ref(input: &str) -> Option<String> {
    let s = bot_util::js::trim(input);
    let s = s.strip_prefix("<#").unwrap_or(s);
    let s = s.strip_suffix('>').unwrap_or(s);
    crate::discord_io::is_snowflake(s).then(|| s.to_string())
}

/// Permission bits used by the overwrites.
const CONNECT: u64 = 1 << 20;
const VIEW_CHANNEL: u64 = 1 << 10;
const MANAGE_CHANNELS: u64 = 1 << 4;

/// `buildOverwrites(guild, botId)`: `@everyone` cannot connect; the bot keeps View Channel and Manage Channels.
pub fn build_overwrites(guild_id: &str, bot_id: &str) -> Vec<Overwrite> {
    vec![
        Overwrite { id: guild_id.to_string(), kind: 0, allow: 0, deny: CONNECT },
        Overwrite { id: bot_id.to_string(), kind: 1, allow: VIEW_CHANNEL | MANAGE_CHANNELS, deny: 0 },
    ]
}

/// `claimPull(guildId, now)`: true when the caller should pull the job now, false when a pull is already pending.
pub fn claim_pull(app: &App, guild_id: &str, now_ms: i64) -> bool {
    let st = state(app);
    let mut map = lock(&st.pending_pull);
    if let Some(t) = map.get(guild_id).copied()
        && t > now_ms
    {
        return false;
    }
    map.set(guild_id, now_ms + MEMBER_PULL_DELAY_MS);
    true
}

/// A pull or an ensure that arrives while a refresh is in flight cannot move the (RUNNING) job: remember it so
/// [`refresh_job_handler`] caps its next run. A no-op when no refresh is running.
fn note_pull_during_run(app: &App, guild_id: &str, run_at_ms: i64) {
    let st = state(app);
    if !lock(&st.busy_guilds).contains(guild_id) {
        return;
    }
    let n = now(app);
    let mut map = lock(&st.pending_pull);
    let prev = map.get(guild_id).copied();
    if prev.is_none_or(|p| p <= n || run_at_ms < p) {
        map.set(guild_id, run_at_ms);
    }
}

struct BusyGuard {
    app: App,
    guild_id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        lock(&state(&self.app).busy_guilds).remove(&self.guild_id);
    }
}

/// `runExclusive(guildId, fn)`: one refresh per guild at a time; `None` when one is already running (`{ busy: true }`).
pub async fn run_exclusive<T, F, Fut>(app: &App, guild_id: &str, f: F) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    {
        let st = state(app);
        let mut busy = lock(&st.busy_guilds);
        if busy.contains(guild_id) {
            return None;
        }
        busy.insert(guild_id.to_string());
    }
    let _guard = BusyGuard { app: app.clone(), guild_id: guild_id.to_string() };
    Some(f().await)
}

// ── A cache miss is not a deletion ────────────────────────────────────────────

/// What [`confirm_channel`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelCheck {
    Present(StatsChannel),
    /// Only after a REST 10003 on an available guild.
    Gone,
    /// Keep the row; the Discord code when there was one.
    Unknown(Option<u64>),
}

/// `confirmChannel(guild, channelId)`: whether a channel exists, decided by the cache or a REST fetch, never by a
/// cache miss alone.
pub async fn confirm_channel(app: &App, guild: &StatsGuild, channel_id: &str) -> ChannelCheck {
    if let Some(c) = guild.channel(channel_id) {
        return ChannelCheck::Present(c.clone());
    }
    if !guild.available {
        return ChannelCheck::Unknown(None);
    }
    let io = stats_io(app);
    // (JS `withTimeout`: the fetch is bounded; a timed-out lookup is "unknown".)
    let fetched = tokio::time::timeout(std::time::Duration::from_millis(CHANNEL_FETCH_TIMEOUT_MS), io.fetch_channel(app, &guild.id, channel_id)).await;
    match fetched {
        Err(_) => ChannelCheck::Unknown(None),
        Ok(Ok(Some(c))) => ChannelCheck::Present(c),
        Ok(Ok(None)) => ChannelCheck::Unknown(None),
        Ok(Err(e)) if e.code == Some(10003) => ChannelCheck::Gone,
        Ok(Err(e)) => ChannelCheck::Unknown(e.code),
    }
}

// ── Database ──────────────────────────────────────────────────────────────────

const PEAK_SQL: &str = concat!(
    r#"UPDATE "StatChannel" SET "peakValue" = $2, "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "id" = $1 AND ("peakValue" IS NULL OR "peakValue" < $2)"#
);
const LAST_VALUE_SQL: &str = concat!(r#"UPDATE "StatChannel" SET "lastValue" = $2, "updatedAt" = "#, sql_now!(), r#" WHERE "id" = $1"#);
const RESERVE_SQL: &str = concat!(
    r#"UPDATE "StatChannel" SET "lastRenamedAt" = $2, "updatedAt" = "#,
    sql_now!(),
    r#" WHERE "id" = $1 AND ("lastRenamedAt" IS NULL OR "lastRenamedAt" <= $3)"#
);
const DELETE_ROW_SQL: &str = r#"DELETE FROM "StatChannel" WHERE "id" = $1"#;
const DELETE_ROWS_SQL: &str = r#"DELETE FROM "StatChannel" WHERE "id" = ANY($1)"#;
const EDIT_TEMPLATE_SQL: &str =
    concat!(r#"UPDATE "StatChannel" SET "template" = $3, "updatedAt" = "#, sql_now!(), r#" WHERE "id" = $1 AND "guildId" = $2"#);
const DELETE_GUILD_ROW_SQL: &str = r#"DELETE FROM "StatChannel" WHERE "id" = $1 AND "guildId" = $2"#;
const COUNT_ROWS_SQL: &str = r#"SELECT COUNT(*)::bigint FROM "StatChannel" WHERE "guildId" = $1"#;
const DELETE_CHANNEL_ROWS_SQL: &str = r#"DELETE FROM "StatChannel" WHERE "guildId" = $1 AND "channelId" = $2"#;
const LIST_SAFE_SQL: &str = r#"SELECT "channelId", "kind"::text AS "kind", "roleId" FROM "StatChannel" WHERE "guildId" = $1 ORDER BY "createdAt" ASC LIMIT 25"#;

/// Every hand-written statement (name, SQL): the Prisma queries that need more than an equality filter.
pub const RAW_SQL: &[(&str, &str)] = &[
    ("stats.peak", PEAK_SQL),
    ("stats.lastValue", LAST_VALUE_SQL),
    ("stats.reserveRename", RESERVE_SQL),
    ("stats.deleteRow", DELETE_ROW_SQL),
    ("stats.deleteRows", DELETE_ROWS_SQL),
    ("stats.editTemplate", EDIT_TEMPLATE_SQL),
    ("stats.deleteGuildRow", DELETE_GUILD_ROW_SQL),
    ("stats.countRows", COUNT_ROWS_SQL),
    ("stats.deleteChannelRows", DELETE_CHANNEL_ROWS_SQL),
    ("stats.listSafe", LIST_SAFE_SQL),
];

fn q(sql: &'static str) -> sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(sql)
}

async fn exec(app: &App, query: sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments>) -> Result<u64, DbError> {
    Ok(query.execute(app.db()).await?.rows_affected())
}

fn ms_to_naive(ms: i64) -> chrono::NaiveDateTime {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now).naive_utc()
}

fn to_ms(dt: &DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

async fn count_rows(app: &App, guild_id: &str) -> Result<i64, DbError> {
    let (n,): (i64,) = sqlx::query_as(COUNT_ROWS_SQL).bind(guild_id.to_string()).fetch_one(app.db()).await?;
    Ok(n)
}

/// `loadRows(guildId)` / `listRows`: every row of a guild, oldest first; primes the memo.
pub async fn load_rows(app: &App, guild_id: &str) -> Result<Vec<StatChannel>, DbError> {
    let rows: Vec<StatChannel> =
        SelectBuilder::new(&STAT_CHANNEL_INFO).filter("guildId", json!(guild_id))?.order_by("createdAt", true)?.build()?.fetch_all(app.db()).await?;
    prime_memo(app, guild_id, &rows);
    Ok(rows)
}

/// `listRows(guildId)` (the same as [`load_rows`]).
pub async fn list_rows(app: &App, guild_id: &str) -> Result<Vec<StatChannel>, DbError> {
    load_rows(app, guild_id).await
}

/// One autocomplete row (`{ channelId, kind, roleId }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRef {
    pub channel_id: String,
    pub kind: StatKind,
    pub role_id: Option<String>,
}

/// `listRowsSafe(guildId, { timeoutMs })`: at most 25 rows, bounded in time, never fails (`[]` on error).
pub async fn list_rows_safe(app: &App, guild_id: &str, timeout_ms: u64) -> Vec<RowRef> {
    let fut = sqlx::query_as::<_, (String, String, Option<String>)>(LIST_SAFE_SQL).bind(guild_id.to_string()).fetch_all(app.db());
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(rows)) => rows
            .into_iter()
            .filter_map(|(channel_id, kind, role_id)| StatKind::parse(&kind).map(|kind| RowRef { channel_id, kind, role_id }))
            .collect(),
        Ok(Err(e)) => {
            bot_util::log_warn!(format!("stats autocomplete lookup failed for guild {guild_id}: {e}"));
            Vec::new()
        }
        Err(_) => {
            bot_util::log_warn!(format!("stats autocomplete lookup failed for guild {guild_id}: stats autocomplete timed out"));
            Vec::new()
        }
    }
}

/// `getGuildRows(guildId)`: the memo when fresh, else the database. `None` on a database error (the hook fails closed).
pub async fn get_guild_rows(app: &App, guild_id: &str) -> Option<Vec<RowLite>> {
    if let Some(m) = fresh_memo(app, guild_id) {
        return Some(m);
    }
    match load_rows(app, guild_id).await {
        Ok(rows) => Some(rows.iter().map(RowLite::from).collect()),
        Err(e) => {
            bot_util::log_error!(format!("stats: could not load the counters of guild {guild_id}"), e.to_string());
            None
        }
    }
}

/// `findRowByRef(guild, input)`: a counter by channel mention, channel id or typed channel name. Never fails.
pub async fn find_row_by_ref(app: &App, guild_id: &str, input: &str) -> Option<StatChannel> {
    let r: Result<Option<StatChannel>, DbError> = async {
        if let Some(id) = parse_channel_ref(input) {
            return SelectBuilder::new(&STAT_CHANNEL_INFO)
                .filter("guildId", json!(guild_id))?
                .filter("channelId", json!(id))?
                .limit(1)
                .build()?
                .fetch_optional(app.db())
                .await;
        }
        let wanted = bot_util::js::lower(bot_util::js::trim(input));
        if wanted.is_empty() {
            return Ok(None);
        }
        let rows = list_rows(app, guild_id).await?;
        let guild = stats_io(app).guild(app, guild_id);
        Ok(rows.into_iter().find(|r| guild.as_ref().and_then(|g| g.channel(&r.channel_id)).is_some_and(|c| bot_util::js::lower(&c.name) == wanted)))
    }
    .await;
    match r {
        Ok(row) => row,
        Err(e) => {
            bot_util::log_warn!(format!("stats: counter lookup failed in guild {guild_id}: {e}"));
            None
        }
    }
}

/// `inspectRow(guild, row)`: the first problem with a counter, from the caches only (display; never deletes anything).
pub fn inspect_row(app: &App, guild: &StatsGuild, row: &StatChannel) -> Option<&'static str> {
    if guild.channel(&row.channel_id).is_none() {
        return Some("channel-deleted");
    }
    if !stats_io(app).missing_bot_permissions(app, &guild.id, Some(&row.channel_id), &["ViewChannel", "ManageChannels"]).is_empty() {
        return Some("missing-permissions");
    }
    if row.kind == StatKind::RoleMembers && !row.role_id.as_deref().is_some_and(|r| guild.has_role(r)) {
        return Some("role-deleted");
    }
    if is_list_kind(row.kind) && guild.member_count > MEMBER_LIST_MAX_MEMBERS {
        return Some("server-too-large");
    }
    None
}

// ── Collecting a snapshot (never fails) ───────────────────────────────────────

/// What a snapshot is collected for: a row's kind and role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub kind: StatKind,
    pub role_id: Option<String>,
}

/// `collectSnapshot(guild, rows, { allowFetch })`: loads what the rows need (the full member list, Discord's approximate
/// online count) and builds the snapshot. `retry_at`: when to look again for an incomplete member list.
pub async fn collect_snapshot(app: &App, guild_id: &str, rows: &[Need], allow_fetch: bool) -> (Snapshot, Option<i64>) {
    let io = stats_io(app);
    let st = state(app);
    let need_list = rows.iter().any(|r| is_list_kind(r.kind));
    let need_online = rows.iter().any(|r| is_online_kind(r.kind));
    let mut role_ids: Vec<String> = Vec::new();
    for r in rows {
        if r.kind == StatKind::RoleMembers
            && let Some(id) = r.role_id.as_ref().filter(|id| !id.is_empty())
            && !role_ids.contains(id)
        {
            role_ids.push(id.clone());
        }
    }
    let guild = io.guild(app, guild_id).unwrap_or_else(|| StatsGuild { id: guild_id.to_string(), ..Default::default() });
    let members = guild.member_count;
    let listable = need_list && members <= MEMBER_LIST_MAX_MEMBERS;

    let blocked = lock(&st.fetch_blocked_until).get(guild_id).copied().unwrap_or(0);
    if listable && !is_member_cache_complete(&guild) && allow_fetch && now(app) >= blocked {
        // Claim first: the claim also stops concurrent fetches. It is lifted only by a fetch that completed the cache.
        lock(&st.fetch_blocked_until).set(guild_id, now(app) + MEMBER_FETCH_RETRY_MS);
        match io.fetch_all_members(app, guild_id, MEMBER_FETCH_TIMEOUT_MS, MEMBER_LIST_MAX_MEMBERS).await {
            Ok(()) => {
                if io.guild(app, guild_id).is_some_and(|g| is_member_cache_complete(&g)) {
                    lock(&st.fetch_blocked_until).remove(guild_id);
                }
            }
            Err(FetchFailure::RateLimited { retry_after_ms }) => {
                // Opcode-8 rate limit: back off for Discord's retry_after (at least 30 s).
                let until = now(app) + retry_after_ms.max(30_000);
                lock(&st.fetch_blocked_until).set(guild_id, until);
                if warn_once(app, &format!("{guild_id}:fetch")) {
                    bot_util::log_warn!(format!("Stat channels: the member list of guild {guild_id} is rate limited; trying again later"));
                }
            }
            // timeout / too-large / error: keep the 5-minute claim.
            Err(_) => {}
        }
    }
    let guild = io.guild(app, guild_id).unwrap_or(guild);

    let mut retry_at = None;
    if listable && !is_member_cache_complete(&guild) {
        let blocked = lock(&st.fetch_blocked_until).get(guild_id).copied().unwrap_or(0);
        retry_at = Some((now(app) + MIN_NEXT_RUN_MS).max(blocked));
    }

    let mut approx_online = None;
    let presences = bot_util::gateway_features::presences_enabled();
    if need_online && (!presences || members > PRESENCE_CACHE_MAX_MEMBERS) {
        match tokio::time::timeout(std::time::Duration::from_millis(GUILD_FETCH_TIMEOUT_MS), io.approximate_presence_count(app, guild_id)).await {
            Ok(Ok(n)) => approx_online = n.map(|v| v as i64),
            Ok(Err(e)) => {
                if warn_once(app, &format!("{guild_id}:guildfetch")) {
                    bot_util::log_warn!(format!("Stat channels: could not read the online count of guild {guild_id}: {e}"));
                }
            }
            Err(_) => {
                if warn_once(app, &format!("{guild_id}:guildfetch")) {
                    bot_util::log_warn!(format!("Stat channels: could not read the online count of guild {guild_id}: stats guild fetch timed out"));
                }
            }
        }
    }
    (build_snapshot(&guild, &role_ids, approx_online, presences), retry_at)
}

// ── The refresh engine ────────────────────────────────────────────────────────

/// What a refresh did (`{ renamed, unchanged, removed, deferred, waiting, skipped, nextRunAt }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshResult {
    pub renamed: i64,
    pub unchanged: i64,
    pub removed: i64,
    pub deferred: i64,
    /// `(channelId, retryAt)`.
    pub waiting: Vec<(String, i64)>,
    /// `(channelId, problem)`.
    pub skipped: Vec<(String, &'static str)>,
    /// Epoch ms.
    pub next_run_at: i64,
}

async fn drop_row(app: &App, guild_id: &str, row: &StatChannel, why: &str) -> Result<(), DbError> {
    exec(app, q(DELETE_ROW_SQL).bind(row.id.clone())).await?;
    forget_row(app, guild_id, &row.id);
    bot_util::log_info!(format!("Stat channel {} {why} in {guild_id}; counter removed", row.channel_id));
    Ok(())
}

/// `refreshGuild(guild, rows, { allowFetch, startedAt })`: recomputes every counter of a guild and renames the channels
/// whose name changed (never more often than [`MIN_RENAME_GAP_MS`] per channel). Never fails; call through
/// [`run_exclusive`].
pub async fn refresh_guild(app: &App, guild_id: &str, rows: &[StatChannel], allow_fetch: bool, started_at: i64) -> RefreshResult {
    let io = stats_io(app);
    let mut result = RefreshResult {
        renamed: 0,
        unchanged: 0,
        removed: 0,
        deferred: 0,
        waiting: Vec::new(),
        skipped: Vec::new(),
        next_run_at: now(app) + REFRESH_INTERVAL_MS,
    };
    let available = io.guild(app, guild_id).is_some_and(|g| g.available);
    if !available {
        result.next_run_at = now(app) + 5 * 60_000;
        return result;
    }
    let needs: Vec<Need> = rows.iter().map(|r| Need { kind: r.kind, role_id: r.role_id.clone() }).collect();
    let (snap, fetch_retry_at) = collect_snapshot(app, guild_id, &needs, allow_fetch).await;
    let Some(guild) = io.guild(app, guild_id) else {
        for row in rows {
            result.skipped.push((row.channel_id.clone(), "error"));
        }
        result.next_run_at = compute_next_run_at(now(app), &[], true);
        return result;
    };
    if !guild.has_me {
        for row in rows {
            result.skipped.push((row.channel_id.clone(), "error"));
        }
        result.next_run_at = compute_next_run_at(now(app), &[], true);
        return result;
    }

    let mut soon = false;
    let mut extra_retries: Vec<i64> = Vec::new();
    for row in rows {
        let step: anyhow::Result<()> = async {
            // Budget: a run starts no rename after REFRESH_BUDGET_MS, so it ends inside the scheduler's 60 s timeout.
            if now(app) - started_at > REFRESH_BUDGET_MS {
                result.deferred += 1;
                soon = true;
                return Ok(());
            }
            // A channel missing from the cache is NOT deleted. Discord is asked; only a 10003 removes the row.
            let current = io.guild(app, guild_id).unwrap_or_else(|| guild.clone());
            let channel = match current.channel(&row.channel_id) {
                Some(c) => c.clone(),
                None => match confirm_channel(app, &current, &row.channel_id).await {
                    ChannelCheck::Gone => {
                        drop_row(app, guild_id, row, "no longer exists").await?;
                        result.removed += 1;
                        return Ok(());
                    }
                    ChannelCheck::Unknown(code) => {
                        let perm = matches!(code, Some(50001) | Some(50013));
                        result.skipped.push((row.channel_id.clone(), if perm { "missing-permissions" } else { "channel-unknown" }));
                        if !perm {
                            extra_retries.push(now(app) + UNKNOWN_RETRY_MS);
                        }
                        if warn_once(app, &format!("{}:lookup", row.channel_id)) {
                            let detail = code.map(|c| format!(" (Discord {c})")).unwrap_or_default();
                            bot_util::log_warn!(format!(
                                "Stat channel {} ({guild_id}): could not be checked{detail}; the counter is kept",
                                row.channel_id
                            ));
                        }
                        return Ok(());
                    }
                    ChannelCheck::Present(c) => c,
                },
            };

            let Computed { value, problem } = compute_value(row.kind, &snap, row.role_id.as_deref(), row.peak_value.map(i64::from));

            // Peak: only ever increases; written even when the rename is blocked later on.
            if row.kind == StatKind::PeakOnline
                && let Some(v) = value
                && v > row.peak_value.map(i64::from).unwrap_or(-1)
            {
                exec(app, q(PEAK_SQL).bind(row.id.clone()).bind(v as i32)).await?;
            }

            let Some(value) = value else {
                let problem = problem.unwrap_or("error");
                result.skipped.push((row.channel_id.clone(), problem));
                if problem == "members-incomplete" {
                    soon = true;
                }
                if (problem == "role-deleted" || problem == "server-too-large") && warn_once(app, &format!("{}:{problem}", row.channel_id)) {
                    bot_util::log_warn!(format!("Stat channel {} ({guild_id}): {}", row.channel_id, describe_problem(problem)));
                }
                return Ok(());
            };

            let role_name = row.role_id.as_deref().and_then(|r| current.role_name(r)).map(str::to_string);
            let desired = render_stat_name(&row.template, Some(value), role_name.as_deref(), current.placeholder.as_ref());
            let alias = lock(&state(app).name_aliases).get(&channel.id).cloned();
            let d = decide_rename(&desired, &channel.name, alias.as_ref(), row.last_renamed_at.as_ref().map(to_ms), now(app));
            match d {
                RenameDecision::Same => {
                    result.unchanged += 1;
                    if row.last_value.map(i64::from) != Some(value) {
                        exec(app, q(LAST_VALUE_SQL).bind(row.id.clone()).bind(value as i32)).await?;
                    }
                    return Ok(());
                }
                RenameDecision::Wait { retry_at } => {
                    result.waiting.push((row.channel_id.clone(), retry_at));
                    return Ok(());
                }
                RenameDecision::Rename => {}
            }

            let needs = ["ViewChannel", "ManageChannels"];
            let missing = match channel.bot_permissions.filter(|_| current.channel(&channel.id).is_none()) {
                // A channel Discord returned while the cache missed it: its permissions as fetched.
                Some(bits) => bot_core::interaction_helpers::missing_from(Some(serenity::all::Permissions::from_bits_truncate(bits)), &needs),
                None => io.missing_bot_permissions(app, guild_id, Some(&channel.id), &needs),
            };
            if !missing.is_empty() {
                result.skipped.push((row.channel_id.clone(), "missing-permissions"));
                if warn_once(app, &format!("{}:perm", channel.id)) {
                    bot_util::log_warn!(format!("Stat channel {} ({guild_id}): missing {}; skipping", channel.id, missing.join(", ")));
                }
                return Ok(());
            }

            // Reserve the rename slot atomically, BEFORE the request.
            let n = now(app);
            let reserved = exec(app, q(RESERVE_SQL).bind(row.id.clone()).bind(ms_to_naive(n)).bind(ms_to_naive(n - MIN_RENAME_GAP_MS))).await?;
            if reserved == 0 {
                result.waiting.push((row.channel_id.clone(), now(app) + MIN_RENAME_GAP_MS));
                return Ok(());
            }

            let renamed =
                tokio::time::timeout(std::time::Duration::from_millis(RENAME_TIMEOUT_MS), io.rename_channel(app, guild_id, &channel.id, &desired, RENAME_REASON))
                    .await
                    .unwrap_or_else(|_| Err(DiscordError::timeout(format!("stats rename {}", channel.id))));
            match renamed {
                Ok(updated) => {
                    if let Some(actual) = updated.filter(|n| *n != desired) {
                        // Discord normalised the name; remember it so the next run does not rename in a loop.
                        lock(&state(app).name_aliases).set(&channel.id, (desired.clone(), actual));
                    }
                    exec(app, q(LAST_VALUE_SQL).bind(row.id.clone()).bind(value as i32)).await?;
                    result.renamed += 1;
                }
                Err(e) if e.timeout => {
                    // Probably queued behind a Discord rate limit; the reservation keeps us from piling on.
                    result.waiting.push((row.channel_id.clone(), now(app) + MIN_RENAME_GAP_MS));
                }
                Err(e) if e.code == Some(10003) => {
                    drop_row(app, guild_id, row, "was deleted (Discord 10003 on rename)").await?;
                    result.removed += 1;
                }
                Err(e) if matches!(e.code, Some(50013) | Some(50001)) => {
                    result.skipped.push((row.channel_id.clone(), "missing-permissions"));
                    if warn_once(app, &format!("{}:perm", channel.id)) {
                        bot_util::log_warn!(format!(
                            "Stat channel {} ({guild_id}): Discord refused the rename ({}); skipping",
                            channel.id,
                            e.code.unwrap_or(0)
                        ));
                    }
                }
                Err(e) if e.rate_limited.is_some() => {
                    let after = e.rate_limited.filter(|ms| *ms > 0).unwrap_or(MIN_RENAME_GAP_MS);
                    result.waiting.push((row.channel_id.clone(), now(app) + after));
                }
                Err(e) => {
                    result.skipped.push((row.channel_id.clone(), "error"));
                    bot_util::log_error!(format!("Stat channel {} ({guild_id}): rename failed", channel.id), e.to_string());
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = step {
            result.skipped.push((row.channel_id.clone(), "error"));
            bot_util::log_error!("stats refresh row failed", bot_core::error::log_arg(&e));
        }
    }

    let mut retries: Vec<i64> = result.waiting.iter().map(|(_, t)| *t).collect();
    retries.extend(extra_retries);
    if let Some(f) = fetch_retry_at {
        retries.push(f);
    }
    result.next_run_at = compute_next_run_at(now(app), &retries, soon);
    result
}

// ── Scheduling ────────────────────────────────────────────────────────────────

fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

/// `ensureRefreshJob(guildId, runAt)`: makes sure the recurring job exists. A healthy job is only ever pulled EARLIER
/// ('earliest'); a FAILED, DONE or CANCELLED row is revived ('replace'). Never fails; `true` on success.
pub async fn ensure_refresh_job(app: &App, guild_id: &str, run_at_ms: Option<i64>) -> bool {
    let run_at = run_at_ms.unwrap_or_else(|| now(app) + REFRESH_INTERVAL_MS);
    note_pull_during_run(app, guild_id, run_at);
    let job = app.scheduler().get_job(&refresh_job_key(guild_id)).await;
    let healthy = job.is_some_and(|j| matches!(j.status, JobStatus::Pending | JobStatus::Running));
    let opts = ScheduleJob::new(guild_id, bot_config::JobType::StatsRefresh, ms_to_utc(run_at))
        .payload(json!({}))
        .dedupe_key(refresh_job_key(guild_id))
        .on_conflict(if healthy { OnConflict::Earliest } else { OnConflict::Replace });
    match app.scheduler().schedule_job(opts).await {
        Ok(_) => true,
        Err(e) => {
            bot_util::log_error!(format!("stats: could not schedule the refresh of guild {guild_id}"), bot_core::error::log_arg(&e));
            false
        }
    }
}

/// `pullRefresh(guildId, delayMs)`: pulls the job forward to now + delay ('earliest' never pushes it later). Never fails.
pub async fn pull_refresh(app: &App, guild_id: &str, delay_ms: i64) -> bool {
    let at = now(app) + delay_ms;
    note_pull_during_run(app, guild_id, at);
    let opts = ScheduleJob::new(guild_id, bot_config::JobType::StatsRefresh, ms_to_utc(at))
        .payload(json!({}))
        .dedupe_key(refresh_job_key(guild_id))
        .on_conflict(OnConflict::Earliest);
    match app.scheduler().schedule_job(opts).await {
        Ok(_) => true,
        Err(e) => {
            lock(&state(app).pending_pull).remove(guild_id); // so a later event retries
            bot_util::log_error!(format!("stats: could not pull the refresh of guild {guild_id}"), bot_core::error::log_arg(&e));
            false
        }
    }
}

/// `cancelRefreshJob(guildId)`: the number of jobs cancelled.
pub async fn cancel_refresh_job(app: &App, guild_id: &str) -> u64 {
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(refresh_job_key(guild_id)), ..Default::default() }).await
}

/// `refreshJobHandler(job, ctx)`: the `stats.refresh` job body. Never fails, so the recurring job cannot go FAILED
/// through handler errors. `None` deletes the job (no counters left); `Some(ms)` is the next run.
pub async fn refresh_job_handler(app: &App, guild_id: &str) -> Option<i64> {
    let started = now(app);
    lock(&state(app).pending_pull).remove(guild_id); // later member events may pull again
    let io = stats_io(app);
    if !io.guild(app, guild_id).is_some_and(|g| g.available) {
        return Some(started + 5 * 60_000);
    }
    let rows = match load_rows(app, guild_id).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("stats.refresh failed for guild {guild_id}"), e.to_string());
            return Some(now(app) + 5 * 60_000);
        }
    };
    if rows.is_empty() {
        return None;
    }
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else { return Some(started + 2 * 60_000) };
    if !bot_core::settings::is_module_enabled(Some(&settings), Some(bot_config::ModuleKey::Stats)) {
        return Some(started + REFRESH_INTERVAL_MS); // idle while off
    }
    let Some(out) = run_exclusive(app, guild_id, || refresh_guild(app, guild_id, &rows, true, started)).await else {
        return Some(started + MIN_NEXT_RUN_MS);
    };
    let mut next = out.next_run_at;
    // A member event that claimed a pull while this run was in flight could not move the (RUNNING) job: cap the next
    // run at the claimed time so the trailing value is never dropped.
    if let Some(pulled) = lock(&state(app).pending_pull).get(guild_id).copied() {
        let cap = pulled.max(now(app) + PULL_FLOOR_MS);
        if cap < next {
            next = cap;
        }
    }
    Some(next)
}

// ── Hook bodies and filters ───────────────────────────────────────────────────

/// guildReady (always; database only): primes the memo and keeps the job alive.
pub async fn on_guild_ready(app: &App, guild_id: &str) {
    let rows = match load_rows(app, guild_id).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("stats: could not load the counters of guild {guild_id} on startup"), e.to_string());
            return;
        }
    };
    // Spread out so a restart is not a thundering herd. With no rows the memo is primed to [].
    if !rows.is_empty() {
        let at = now(app) + STARTUP_DELAY_MS + (js_random() * STARTUP_JITTER_MS as f64).floor() as i64;
        ensure_refresh_job(app, guild_id, Some(at)).await;
    }
}

/// `memberEventFilter(ctx)`: no or expired memo fails open (the body reads Postgres).
pub fn member_event_filter(app: &App, guild_id: &str) -> bool {
    match fresh_memo(app, guild_id) {
        None => true,
        Some(rows) => rows.iter().any(|r| is_member_event_kind(r.kind)),
    }
}

/// guildMemberAdd / guildMemberRemove (gated by the module). Bots joining count too.
pub async fn on_member_change(app: &App, guild_id: &str) {
    let Some(rows) = get_guild_rows(app, guild_id).await else { return };
    if !rows.iter().any(|r| is_member_event_kind(r.kind)) {
        return;
    }
    if !claim_pull(app, guild_id, now(app)) {
        return; // a pull is already pending and that run will see this change too
    }
    pull_refresh(app, guild_id, MEMBER_PULL_DELAY_MS).await;
}

/// `channelDeleteFilter(ctx)`: a category passes when it is the stats category (or the settings are not memoised); a
/// voice channel passes when it is a counter (or the memo is missing or expired).
pub fn channel_delete_filter(app: &App, guild_id: &str, channel_kind: u8, channel_id: &str) -> bool {
    if channel_kind == CATEGORY {
        return match app.settings().peek_settings(guild_id) {
            None => true,
            Some(s) => s.stats_category_id.as_deref() == Some(channel_id),
        };
    }
    if channel_kind == VOICE {
        return match fresh_memo(app, guild_id) {
            None => true,
            Some(rows) => rows.iter().any(|r| r.channel_id == channel_id),
        };
    }
    false
}

/// channelDelete (always: cleanup must happen while the module is off). A delete event is authoritative.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_kind: u8, channel_id: &str, settings: Option<bot_core::Settings>) {
    let r: anyhow::Result<()> = async {
        if channel_kind == VOICE {
            let n = exec(app, q(DELETE_CHANNEL_ROWS_SQL).bind(guild_id.to_string()).bind(channel_id.to_string())).await?;
            if n > 0 {
                if let Some((_, rows)) = lock(&state(app).rows_memo).get_mut(guild_id) {
                    rows.retain(|r| r.channel_id != channel_id);
                }
                bot_util::log_info!(format!("Stat channel {channel_id} deleted in {guild_id}; counter removed"));
                if count_rows(app, guild_id).await? == 0 {
                    cancel_refresh_job(app, guild_id).await;
                }
            }
        } else if channel_kind == CATEGORY {
            // Discord moves the child channels to the top level; it does not delete them.
            if settings.as_ref().and_then(|s| s.stats_category_id.as_deref()) == Some(channel_id) {
                app.settings()
                    .update_settings(guild_id, bot_core::SettingsPatch::new().set("statsCategoryId", serde_json::Value::Null))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        bot_util::log_error!(format!("stats: channelDelete cleanup failed in guild {guild_id}"), bot_core::error::log_arg(&e));
    }
}

// ── Service functions used by /stats ──────────────────────────────────────────

/// What [`create_stat_channel`] made.
#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub channel: StatsChannel,
    pub row: StatChannel,
    pub value: Option<i64>,
}

/// The role a ROLE_MEMBERS counter counts: `(id, name)`.
pub type RoleRef = (String, String);

/// `createStatChannel({ guild, client, kind, template, role, category, actorTag })`: creates the locked voice channel
/// and its row. Fails with a [`StatsError`] (LIMIT, ROLE_LIMIT, DUPLICATE, MISSING_PERMISSIONS, CHANNEL_LIMIT,
/// DISCORD_REJECTED) or the original database error.
pub async fn create_stat_channel(
    app: &App,
    guild_id: &str,
    kind: StatKind,
    template: &str,
    role: Option<RoleRef>,
    category: Option<String>,
    actor_tag: &str,
) -> anyhow::Result<Created> {
    let (a, g, t, tag) = (app.clone(), guild_id.to_string(), template.to_string(), actor_tag.to_string());
    let made = config_mutex(app)
        .run(guild_id, move || async move { create_locked(&a, &g, kind, &t, role, category, &tag).await })
        .await?;
    // After the mutex.
    let delay = if made.value.is_none() { MIN_NEXT_RUN_MS } else { REFRESH_INTERVAL_MS };
    ensure_refresh_job(app, guild_id, Some(now(app) + delay)).await;
    Ok(made)
}

async fn create_locked(
    app: &App,
    guild_id: &str,
    kind: StatKind,
    template: &str,
    role: Option<RoleRef>,
    category: Option<String>,
    actor_tag: &str,
) -> anyhow::Result<Created> {
    let io = stats_io(app);
    // 1. Load the rows and prune the ones whose channel Discord confirms is gone (a cache miss alone never prunes).
    let mut rows = list_rows(app, guild_id).await?;
    let guild = io.guild(app, guild_id).unwrap_or_else(|| StatsGuild { id: guild_id.to_string(), ..Default::default() });
    let mut gone: Vec<String> = Vec::new();
    for r in &rows {
        if guild.channel(&r.channel_id).is_some() {
            continue;
        }
        if confirm_channel(app, &guild, &r.channel_id).await == ChannelCheck::Gone {
            gone.push(r.id.clone());
        }
    }
    if !gone.is_empty() {
        exec(app, q(DELETE_ROWS_SQL).bind(gone.clone())).await?;
        rows.retain(|r| !gone.contains(&r.id));
        prime_memo(app, guild_id, &rows);
    }

    // 2. Caps (rows whose channel could not be checked still count).
    if rows.len() >= bot_config::limits::STAT_CHANNELS {
        return Err(StatsError::new("LIMIT").into());
    }
    if kind == StatKind::RoleMembers && rows.iter().filter(|r| r.kind == StatKind::RoleMembers).count() >= bot_config::limits::ROLE_STAT_CHANNELS {
        return Err(StatsError::new("ROLE_LIMIT").into());
    }

    // 3. One counter per kind (per role for ROLE_MEMBERS).
    let role_id = role.as_ref().map(|(id, _)| id.clone());
    if let Some(dup) = rows.iter().find(|r| r.kind == kind && (kind != StatKind::RoleMembers || r.role_id == role_id)) {
        return Err(StatsError { code: "DUPLICATE", channel_id: Some(dup.channel_id.clone()) }.into());
    }

    // 4. Initial value (may be missing: the name shows an ellipsis until the next refresh).
    let (snap, _) = collect_snapshot(app, guild_id, &[Need { kind, role_id: role_id.clone() }], true).await;
    let value = compute_value(kind, &snap, role_id.as_deref(), None).value;
    let guild = io.guild(app, guild_id).unwrap_or(guild);
    let name = render_stat_name(template, value, role.as_ref().map(|(_, n)| n.as_str()), guild.placeholder.as_ref());

    // 5-6. The settings row first (FK), then the channel.
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let bot_id = io.bot_user_id(app).unwrap_or_default();
    let req = CreateVoiceChannel {
        name,
        parent: category,
        overwrites: build_overwrites(guild_id, &bot_id),
        reason: clamp_text(&format!("Statistics channel created by {actor_tag}"), 512),
    };
    let channel = match io.create_channel(app, guild_id, &req).await {
        Ok(c) => c,
        Err(e) => {
            return Err(match e.code {
                Some(50013) | Some(50001) => StatsError::new("MISSING_PERMISSIONS").into(),
                Some(30013) => StatsError::new("CHANNEL_LIMIT").into(),
                Some(50035) => StatsError::new("DISCORD_REJECTED").into(),
                _ => e.into(),
            });
        }
    };

    // 7. Save the row; on failure remove the channel again so nothing is orphaned.
    let insert = InsertBuilder::new(&STAT_CHANNEL_INFO)
        .values([
            ("guildId", json!(guild_id)),
            ("channelId", json!(channel.id)),
            ("kind", json!(kind.as_str())),
            ("roleId", json!(role_id)),
            ("template", json!(template)),
            ("lastValue", json!(value)),
            ("lastRenamedAt", serde_json::Value::Null),
            ("peakValue", if kind == StatKind::PeakOnline { json!(value) } else { serde_json::Value::Null }),
        ])
        .and_then(InsertBuilder::build);
    let saved: Result<StatChannel, DbError> = match insert {
        Ok(q) => q.fetch_one(app.db()).await,
        Err(e) => Err(e),
    };
    let row = match saved {
        Ok(r) => r,
        Err(e) => {
            bot_core::deletion_tracker::mark_bot_deletion(app, &[channel.id.as_str()], Some("stats"), None);
            let _ = io.delete_channel(app, guild_id, &channel.id, "Statistics channel could not be saved").await;
            return Err(e.into());
        }
    };
    memo_add_row(app, guild_id, &row);
    Ok(Created { channel, row, value })
}

/// `editTemplate(guild, row, template)`: changes a counter's template; the rename follows through a pull of the job.
/// Fails with `StatsError('NOT_FOUND')` when the row vanished.
pub async fn edit_template(app: &App, guild_id: &str, row: &StatChannel, template: &str) -> anyhow::Result<()> {
    let (a, g, r, t) = (app.clone(), guild_id.to_string(), row.clone(), template.to_string());
    config_mutex(app)
        .run(guild_id, move || async move {
            let n = exec(&a, q(EDIT_TEMPLATE_SQL).bind(r.id.clone()).bind(g.clone()).bind(t)).await?;
            if n == 0 {
                return Err(anyhow::Error::from(StatsError::new("NOT_FOUND")));
            }
            memo_add_row(&a, &g, &r);
            Ok(())
        })
        .await?;
    pull_refresh(app, guild_id, EDIT_PULL_DELAY_MS).await;
    Ok(())
}

/// `deleteStatChannel(guild, row, { keepChannel, actorTag })`: removes a counter and (unless `keep_channel`) its Discord
/// channel. Fails with `StatsError('MISSING_PERMISSIONS')` and changes nothing when the bot cannot delete the channel.
/// Returns whether the channel was deleted.
pub async fn delete_stat_channel(app: &App, guild_id: &str, row: &StatChannel, keep_channel: bool, actor_tag: &str) -> anyhow::Result<bool> {
    let (a, g, r, tag) = (app.clone(), guild_id.to_string(), row.clone(), actor_tag.to_string());
    config_mutex(app)
        .run(guild_id, move || async move {
            let io = stats_io(&a);
            let mut channel_deleted = false;
            if !keep_channel {
                let reason = clamp_text(&format!("Statistics channel removed by {tag}"), 512);
                let cached = io.guild(&a, &g).and_then(|gd| gd.channel(&r.channel_id).cloned());
                if cached.is_some() && !io.missing_bot_permissions(&a, &g, Some(&r.channel_id), &["ViewChannel", "ManageChannels"]).is_empty() {
                    return Err(anyhow::Error::from(StatsError::new("MISSING_PERMISSIONS")));
                }
                bot_core::deletion_tracker::mark_bot_deletion(&a, &[r.channel_id.as_str()], Some("stats"), None);
                // A channel missing from the cache may still exist, so it is deleted by id and a 10003 counts as success.
                match io.delete_channel(&a, &g, &r.channel_id, &reason).await {
                    Ok(()) => channel_deleted = true,
                    Err(e) if e.code == Some(10003) => channel_deleted = false,
                    Err(e) if matches!(e.code, Some(50013) | Some(50001)) => return Err(StatsError::new("MISSING_PERMISSIONS").into()),
                    Err(e) => return Err(e.into()),
                }
            }
            exec(&a, q(DELETE_GUILD_ROW_SQL).bind(r.id.clone()).bind(g.clone())).await?;
            forget_row(&a, &g, &r.id);
            if count_rows(&a, &g).await? == 0 {
                cancel_refresh_job(&a, &g).await;
            }
            Ok(channel_deleted)
        })
        .await
}

/// What [`refresh_now`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshNow {
    Empty,
    Busy,
    Done(RefreshResult),
}

/// `refreshNow(guild)`: refreshes every counter now (still bound by the per-channel rename gap).
pub async fn refresh_now(app: &App, guild_id: &str) -> anyhow::Result<RefreshNow> {
    let rows = list_rows(app, guild_id).await?;
    if rows.is_empty() {
        return Ok(RefreshNow::Empty);
    }
    let started = now(app);
    let Some(out) = run_exclusive(app, guild_id, || refresh_guild(app, guild_id, &rows, true, started)).await else { return Ok(RefreshNow::Busy) };
    ensure_refresh_job(app, guild_id, Some(out.next_run_at)).await;
    Ok(RefreshNow::Done(out))
}
