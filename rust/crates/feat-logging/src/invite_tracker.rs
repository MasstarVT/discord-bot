//! Port of `src/services/inviteTracker.js`: everything that decides WHO invited a member. The invites plugin and the
//! `/invites` command are thin wrappers.
//!
//! Model: a per-guild [`Snapshot`] (every invite's use count, a 2-minute tombstone for invites that vanished, and the
//! vanity-URL baseline) is kept in memory and mirrored to Redis (`guild:{g}:invites`, 7 days). A join takes a fresh
//! observation (one invites fetch + one vanity fetch) and diffs it against the snapshot inside a per-guild mutex.
//! Exactly one candidate invite means a match; zero or several mean "unknown". Nothing is ever guessed: a missing,
//! legacy or pre-restart snapshot only re-snapshots.
//!
//! Discord I/O goes through [`InviteApi`] (serenity by default; tests install a fake with [`set_invite_api`]).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};

use bot_core::App;
use bot_core::discord::GuildState;
use bot_db::{GuildMemberCache, InsertBuilder, PersonalInvite, SelectBuilder};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Permissions, UserId};

use crate::log_events::InviteInfo;
use crate::logging_service::ApiError;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const SNAPSHOT_VERSION: u32 = 1;
/// 604800, re-set on every write.
pub const SNAPSHOT_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
/// Tombstones match and live this long.
pub const TOMBSTONE_TTL_MS: i64 = 2 * 60_000;
/// MEE6's 24 h validity rule.
pub const FAKE_THRESHOLD_MS: i64 = 24 * 60 * 60_000;
/// Waiters per guild before a join is treated as a burst.
pub const MAX_PENDING_JOINS: usize = 5;
/// Per invites / vanity fetch.
pub const FETCH_TIMEOUT_MS: u64 = 5_000;
/// The plugin stops waiting; tracking continues in the background.
pub const JOIN_HOOK_WAIT_MS: u64 = 7_000;
pub const MEMO_MAX_GUILDS: usize = 10_000;
pub const LINK_COOLDOWN_SECONDS: u64 = 60;

/// `snapshotKey(guildId)`.
pub fn snapshot_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:invites")
}

// ── Snapshot model ────────────────────────────────────────────────────────────

/// One code's state (`normState`): `{ uses, maxUses, inviterId, expiresAt }`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeState {
    pub uses: i64,
    pub max_uses: i64,
    pub inviter_id: Option<String>,
    pub expires_at: Option<i64>,
}

/// A vanished code: its last state and when it vanished.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tombstone {
    #[serde(flatten)]
    pub state: CodeState,
    pub deleted_at: i64,
}

/// The vanity URL baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Vanity {
    pub code: String,
    pub uses: i64,
}

/// The per-guild snapshot, as the JS stores it in Redis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub v: u32,
    pub taken_at: i64,
    pub codes: IndexMap<String, CodeState>,
    pub tombstones: IndexMap<String, Tombstone>,
    pub vanity: Option<Vanity>,
}

/// An observed invite (`inviteToState(invite)`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteState {
    pub code: String,
    pub uses: i64,
    pub max_uses: i64,
    pub inviter_id: Option<String>,
    pub expires_at: Option<i64>,
}

impl InviteState {
    fn norm(&self) -> CodeState {
        CodeState { uses: self.uses.max(0), max_uses: self.max_uses.max(0), inviter_id: self.inviter_id.clone(), expires_at: self.expires_at }
    }
}

/// One full observation: the invite list plus the vanity data (`None`: no vanity, or its fetch failed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation {
    pub invites: Vec<InviteState>,
    pub vanity: Option<Vanity>,
}

fn to_int(v: Option<&Value>) -> i64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(0.0) }
        }
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        _ => 0.0,
    };
    if !n.is_finite() { 0 } else { n.trunc().max(0.0) as i64 }
}

fn js_number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(Value::Null) | None => None,
        Some(Value::Bool(b)) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

/// `normState(e)`: one stored entry, read defensively.
fn norm_value(e: &Value) -> CodeState {
    CodeState {
        uses: to_int(e.get("uses")),
        max_uses: to_int(e.get("maxUses")),
        inviter_id: match e.get("inviterId") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        },
        expires_at: js_number(e.get("expiresAt")).filter(|x| x.is_finite()).map(|x| x as i64),
    }
}

fn is_int(v: &Value) -> bool {
    v.as_f64().is_some_and(|x| x.is_finite() && x.fract() == 0.0)
}

/// `isSnapshot(x)`: the snapshot shape check. The legacy value `{ [code]: uses }` fails it and counts as missing.
pub fn is_snapshot(x: &Value) -> bool {
    let Some(o) = x.as_object() else { return false };
    if o.get("v").and_then(Value::as_f64) != Some(f64::from(SNAPSHOT_VERSION)) {
        return false;
    }
    if !o.get("takenAt").and_then(Value::as_f64).is_some_and(f64::is_finite) {
        return false;
    }
    if !o.get("codes").is_some_and(Value::is_object) || !o.get("tombstones").is_some_and(Value::is_object) {
        return false;
    }
    match o.get("vanity") {
        Some(Value::Null) => true,
        Some(Value::Object(v)) => v.get("code").is_some_and(Value::is_string) && v.get("uses").is_some_and(is_int),
        _ => false,
    }
}

/// A stored snapshot, when it passes [`is_snapshot`] (entries normalised).
pub fn parse_snapshot(x: &Value) -> Option<Snapshot> {
    if !is_snapshot(x) {
        return None;
    }
    let codes = x["codes"].as_object().map(|m| m.iter().map(|(c, e)| (c.clone(), norm_value(e))).collect()).unwrap_or_default();
    let tombstones = x["tombstones"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(c, t)| {
                    let deleted = js_number(t.get("deletedAt")).filter(|d| d.is_finite()).map(|d| d as i64).unwrap_or(i64::MIN);
                    (c.clone(), Tombstone { state: norm_value(t), deleted_at: deleted })
                })
                .collect()
        })
        .unwrap_or_default();
    let vanity = x["vanity"].as_object().map(|v| Vanity { code: v["code"].as_str().unwrap_or("").into(), uses: v["uses"].as_f64().unwrap_or(0.0) as i64 });
    Some(Snapshot { v: SNAPSHOT_VERSION, taken_at: x["takenAt"].as_f64().unwrap_or(0.0) as i64, codes, tombstones, vanity })
}

impl Snapshot {
    /// The JSON the JS writes.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `isTrustedSnapshot(snap, trustedSince)`: observed by THIS process.
pub fn is_trusted_snapshot(snap: Option<&Snapshot>, trusted_since: Option<i64>) -> bool {
    match (snap, trusted_since) {
        (Some(s), Some(t)) => s.taken_at >= t,
        _ => false,
    }
}

/// `emptySnapshot(now)`.
pub fn empty_snapshot(now: i64) -> Snapshot {
    Snapshot { v: SNAPSHOT_VERSION, taken_at: now, ..Default::default() }
}

fn tombstone_alive(t: &Tombstone, now: i64) -> bool {
    t.deleted_at != i64::MIN && now - t.deleted_at <= TOMBSTONE_TTL_MS
}

/// `buildSnapshot(observation, { previous, now })`: a fresh snapshot from a full observation. `previous` must be
/// trusted (or `None`): its live tombstones and its vanished codes carry over. An unknown vanity is never kept.
pub fn build_snapshot(observation: &Observation, previous: Option<&Snapshot>, now: i64) -> Snapshot {
    let mut codes: IndexMap<String, CodeState> = IndexMap::new();
    for o in &observation.invites {
        codes.insert(o.code.clone(), o.norm());
    }
    let mut tombstones: IndexMap<String, Tombstone> = IndexMap::new();
    if let Some(p) = previous {
        for (c, t) in &p.tombstones {
            if codes.contains_key(c) || !tombstone_alive(t, now) {
                continue;
            }
            tombstones.insert(c.clone(), t.clone());
        }
        for (c, s) in &p.codes {
            if codes.contains_key(c) || tombstones.contains_key(c) {
                continue;
            }
            tombstones.insert(c.clone(), Tombstone { state: s.clone(), deleted_at: now }); // vanished since the last observation
        }
    }
    let vanity = observation.vanity.as_ref().map(|v| Vanity { code: v.code.clone(), uses: v.uses.max(0) });
    Snapshot { v: SNAPSHOT_VERSION, taken_at: now, codes, tombstones, vanity }
}

fn pruned(tombstones: &IndexMap<String, Tombstone>, now: i64) -> IndexMap<String, Tombstone> {
    tombstones.iter().filter(|(_, t)| tombstone_alive(t, now)).map(|(c, t)| (c.clone(), t.clone())).collect()
}

/// `applyInviteCreate(snapshot, inviteState, { now })`: adds the code unless it is known. `takenAt` is unchanged.
pub fn apply_invite_create(snapshot: &Snapshot, invite: &InviteState, now: i64) -> Snapshot {
    let mut codes = snapshot.codes.clone();
    if !codes.contains_key(&invite.code) {
        codes.insert(invite.code.clone(), invite.norm());
    }
    Snapshot { v: SNAPSHOT_VERSION, taken_at: snapshot.taken_at, codes, tombstones: pruned(&snapshot.tombstones, now), vanity: snapshot.vanity.clone() }
}

/// `applyInviteDelete(snapshot, code, { now })`: the code becomes a tombstone so its last use count survives for a
/// join that is still queued. `takenAt` is unchanged.
pub fn apply_invite_delete(snapshot: &Snapshot, code: &str, now: i64) -> Snapshot {
    let mut codes = snapshot.codes.clone();
    let mut tombstones = pruned(&snapshot.tombstones, now);
    if let Some(s) = codes.shift_remove(code) {
        tombstones.insert(code.to_string(), Tombstone { state: s, deleted_at: now });
    }
    Snapshot { v: SNAPSHOT_VERSION, taken_at: snapshot.taken_at, codes, tombstones, vanity: snapshot.vanity.clone() }
}

/// What a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateKind {
    Live,
    Vanished,
    Vanity,
}

/// One candidate invite of a join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub code: String,
    pub inviter_id: Option<String>,
    pub delta: i64,
    pub kind: CandidateKind,
}

/// The result of [`resolve_join`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub matched: Option<Candidate>,
    /// `matched` | `none` | `ambiguous`.
    pub reason: &'static str,
    pub candidates: Vec<Candidate>,
    pub next: Snapshot,
}

/// `resolveJoin(before, observation, { now })`: which invite did a join use? `before` must be trusted. Exactly one
/// candidate matches; none gives `none`, two or more `ambiguous`.
pub fn resolve_join(before: &Snapshot, observation: &Observation, now: i64) -> Resolved {
    let observed: HashSet<&str> = observation.invites.iter().map(|o| o.code.as_str()).collect();
    let mut candidates = Vec::new();

    // 1. Live increments. A code absent from `before` was created after the last full observation (baseline 0).
    for o in &observation.invites {
        let prev = before.codes.get(&o.code).cloned().or_else(|| before.tombstones.get(&o.code).map(|t| t.state.clone()));
        let delta = o.uses.max(0) - prev.as_ref().map(|p| p.uses).unwrap_or(0);
        if delta > 0 {
            let inviter = o.inviter_id.clone().or_else(|| prev.and_then(|p| p.inviter_id));
            candidates.push(Candidate { code: o.code.clone(), inviter_id: inviter, delta, kind: CandidateKind::Live });
        }
    }

    // 2. Vanished, consumable codes: one use left before exhaustion, not expired, tombstone fresh.
    let mut vanished: IndexMap<String, (CodeState, Option<i64>)> = IndexMap::new();
    for (c, s) in &before.codes {
        if !observed.contains(c.as_str()) {
            vanished.insert(c.clone(), (s.clone(), None));
        }
    }
    for (c, t) in &before.tombstones {
        if !observed.contains(c.as_str()) {
            vanished.insert(c.clone(), (t.state.clone(), Some(t.deleted_at)));
        }
    }
    for (c, (s, deleted)) in &vanished {
        if !(s.max_uses > 0 && s.uses == s.max_uses - 1) {
            continue;
        }
        if !(s.expires_at.is_none() || s.expires_at.is_some_and(|e| e > now)) {
            continue;
        }
        if let Some(d) = deleted
            && !(*d != i64::MIN && now - d <= TOMBSTONE_TTL_MS)
        {
            continue;
        }
        candidates.push(Candidate { code: c.clone(), inviter_id: s.inviter_id.clone(), delta: 1, kind: CandidateKind::Vanished });
    }

    // 3. Vanity URL: only against a KNOWN baseline for the same code.
    if let (Some(ov), Some(bv)) = (&observation.vanity, &before.vanity)
        && ov.code == bv.code
    {
        let delta = ov.uses.max(0) - bv.uses.max(0);
        if delta > 0 {
            candidates.push(Candidate { code: ov.code.clone(), inviter_id: None, delta, kind: CandidateKind::Vanity });
        }
    }

    // 4. Decide.
    let (matched, reason) = match candidates.len() {
        1 => (Some(candidates[0].clone()), "matched"),
        0 => (None, "none"),
        _ => (None, "ambiguous"),
    };

    // 5. Next snapshot. Every vanished candidate is marked exhausted so a consumed single-use code never matches twice.
    let mut next = build_snapshot(observation, Some(before), now);
    for c in &candidates {
        if c.kind != CandidateKind::Vanished {
            continue;
        }
        let Some((st, deleted)) = vanished.get(&c.code) else { continue };
        let base = next.tombstones.get(&c.code).cloned().unwrap_or_else(|| Tombstone { state: st.clone(), deleted_at: deleted.unwrap_or(now) });
        let mut t = base;
        t.state.uses = st.max_uses;
        next.tombstones.insert(c.code.clone(), t);
    }
    Resolved { matched, reason, candidates, next }
}

/// One tracked join, as `classifyJoin` and `computeStats` read it (times in epoch ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRow {
    pub user_id: String,
    pub inviter_id: Option<String>,
    pub joined_ms: i64,
    pub left_ms: Option<i64>,
}

impl From<&GuildMemberCache> for JoinRow {
    fn from(r: &GuildMemberCache) -> Self {
        Self { user_id: r.user_id.clone(), inviter_id: r.inviter_id.clone(), joined_ms: r.joined_at.timestamp_millis(), left_ms: r.left_at.map(|d| d.timestamp_millis()) }
    }
}

/// `classifyJoin(row)`: `active` | `left` | `fake` (the 24 h rule and the self-invite rule).
pub fn classify_join(row: &JoinRow) -> &'static str {
    if row.inviter_id.as_deref().is_some_and(|i| i == row.user_id) {
        return "fake";
    }
    match row.left_ms {
        None => "active",
        Some(l) => {
            if l - row.joined_ms >= FAKE_THRESHOLD_MS {
                "left"
            } else {
                "fake"
            }
        }
    }
}

/// Invite stats (`{ total, active, left, fake, valid, recent }`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InviteStats {
    pub total: i64,
    pub active: i64,
    pub left: i64,
    pub fake: i64,
    pub valid: i64,
    pub recent: i64,
}

/// `computeStats(rows, { now })`: the executable definition the SQL must match. Only rows with an inviter count.
pub fn compute_stats(rows: &[JoinRow], now: i64) -> InviteStats {
    let mut s = InviteStats::default();
    for row in rows {
        if row.inviter_id.is_none() {
            continue;
        }
        match classify_join(row) {
            "active" => {
                s.active += 1;
                if now - row.joined_ms < FAKE_THRESHOLD_MS {
                    s.recent += 1;
                }
            }
            "left" => s.left += 1,
            _ => s.fake += 1,
        }
    }
    s.valid = s.active + s.left;
    s.total = s.active + s.left + s.fake;
    s
}

// ── Discord I/O ───────────────────────────────────────────────────────────────

/// The Discord REST calls of the invite tracker.
#[async_trait::async_trait]
pub trait InviteApi: Send + Sync {
    /// `guild.invites.fetch({ cache: false })`, as invite states.
    async fn fetch_invites(&self, guild_id: GuildId) -> Result<Vec<InviteState>, ApiError>;
    /// `guild.fetchVanityData()`.
    async fn fetch_vanity(&self, guild_id: GuildId) -> Result<Option<Vanity>, ApiError>;
    /// `client.fetchInvite(code)`: the invite's guild id.
    async fn fetch_invite_guild(&self, code: &str) -> Result<Option<String>, ApiError>;
    /// `guild.invites.delete(code, reason)`.
    async fn delete_invite(&self, guild_id: GuildId, code: &str, reason: &str) -> Result<(), ApiError>;
    /// `channel.createInvite({ maxAge: 0, maxUses: 0, unique: true, temporary: false, reason })`: the new code.
    async fn create_invite(&self, channel_id: ChannelId, reason: &str) -> Result<String, ApiError>;
}

/// The production [`InviteApi`]: serenity's HTTP client.
pub struct SerenityInviteApi {
    pub http: Option<Arc<serenity::all::Http>>,
}

/// `inviteToState(invite)` for an invite from the invite list (`GET /guilds/{id}/invites`). The expiry is
/// `createdAt + maxAge` (discord.js's `expiresTimestamp`).
pub fn invite_to_state(i: &serenity::all::RichInvite) -> InviteState {
    let created = i.created_at.timestamp_millis();
    InviteState {
        code: i.code.clone(),
        uses: i.uses as i64,
        max_uses: i64::from(i.max_uses),
        inviter_id: i.inviter.as_ref().map(|u| u.id.to_string()),
        expires_at: if i.max_age > 0 { Some(created + i64::from(i.max_age) * 1000) } else { None },
    }
}

/// `inviteToState(invite)` for an INVITE_CREATE event.
pub fn invite_event_to_state(i: &serenity::all::InviteCreateEvent) -> InviteState {
    let created = i.created_at.timestamp_millis();
    InviteState {
        code: i.code.clone(),
        uses: i.uses as i64,
        max_uses: i64::from(i.max_uses),
        inviter_id: i.inviter.as_ref().map(|u| u.id.to_string()),
        expires_at: if i.max_age > 0 { Some(created + i64::from(i.max_age) * 1000) } else { None },
    }
}

/// `inviteToState(invite)` on a plain invite object (`{ code, uses, maxUses, inviterId | inviter.id,
/// expiresTimestamp }`), read defensively as the JS does.
pub fn invite_json_to_state(v: &Value) -> InviteState {
    InviteState {
        code: v.get("code").and_then(Value::as_str).unwrap_or_default().to_string(),
        uses: v.get("uses").and_then(Value::as_i64).unwrap_or(0),
        max_uses: v.get("maxUses").and_then(Value::as_i64).unwrap_or(0),
        inviter_id: v.get("inviterId").and_then(Value::as_str).or_else(|| v.get("inviter").and_then(|i| i.get("id")).and_then(Value::as_str)).map(String::from),
        expires_at: v.get("expiresTimestamp").and_then(Value::as_i64),
    }
}

#[async_trait::async_trait]
impl InviteApi for SerenityInviteApi {
    async fn fetch_invites(&self, guild_id: GuildId) -> Result<Vec<InviteState>, ApiError> {
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        let list = http.get_guild_invites(guild_id).await.map_err(|e| ApiError::from_serenity(&e))?;
        Ok(list.iter().map(invite_to_state).collect())
    }

    async fn fetch_vanity(&self, guild_id: GuildId) -> Result<Option<Vanity>, ApiError> {
        use serenity::http::{LightMethod, Request, Route};
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        let v: Value = http.fire(Request::new(Route::GuildVanityUrl { guild_id }, LightMethod::Get)).await.map_err(|e| ApiError::from_serenity(&e))?;
        Ok(v.get("code").and_then(Value::as_str).map(|c| Vanity { code: c.into(), uses: v.get("uses").and_then(Value::as_i64).unwrap_or(0) }))
    }

    async fn fetch_invite_guild(&self, code: &str) -> Result<Option<String>, ApiError> {
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        let inv = http.get_invite(code, false, false, None).await.map_err(|e| ApiError::from_serenity(&e))?;
        Ok(inv.guild.map(|g| g.id.to_string()))
    }

    async fn delete_invite(&self, _guild_id: GuildId, code: &str, reason: &str) -> Result<(), ApiError> {
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        http.delete_invite(code, Some(reason)).await.map(|_| ()).map_err(|e| ApiError::from_serenity(&e))
    }

    async fn create_invite(&self, channel_id: ChannelId, reason: &str) -> Result<String, ApiError> {
        let http = self.http.as_ref().ok_or_else(ApiError::offline)?;
        let body = json!({ "max_age": 0, "max_uses": 0, "unique": true, "temporary": false });
        let inv = http.create_invite(channel_id, &body, Some(reason)).await.map_err(|e| ApiError::from_serenity(&e))?;
        Ok(inv.code)
    }
}

#[derive(Default)]
struct ApiSlot(RwLock<Option<Arc<dyn InviteApi>>>);

/// Replaces the Discord I/O of the invite tracker for this app (tests).
pub fn set_invite_api(app: &App, api: Arc<dyn InviteApi>) {
    *app.ext::<ApiSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(api);
}

/// The app's [`InviteApi`]: the one set with [`set_invite_api`], else serenity.
pub fn invite_api(app: &App) -> Arc<dyn InviteApi> {
    if let Some(api) = app.ext::<ApiSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return api;
    }
    Arc::new(SerenityInviteApi { http: app.http() })
}

// ── Module-scope state (in memory only) ───────────────────────────────────────

/// The per-app state of the tracker: the snapshot memo, pending joins, the guild mutex and the warning throttles.
pub struct TrackerState {
    memo: Mutex<IndexMap<String, Snapshot>>,
    pending: Mutex<HashMap<String, usize>>,
    mutex: bot_core::sync::KeyedMutex,
    perm_warn: bot_util::async_util::Throttle,
    db_warn: bot_util::async_util::Throttle,
    trusted_since: Mutex<Option<i64>>,
}

impl Default for TrackerState {
    fn default() -> Self {
        Self {
            memo: Mutex::new(IndexMap::new()),
            pending: Mutex::new(HashMap::new()),
            mutex: bot_core::sync::KeyedMutex::new(),
            perm_warn: bot_util::async_util::create_throttle(6 * 60 * 60_000),
            db_warn: bot_util::async_util::create_throttle(60_000),
            trusted_since: Mutex::new(None),
        }
    }
}

fn st(app: &App) -> Arc<TrackerState> {
    app.ext::<TrackerState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `trustedSince(guild)` = `client.readyTimestamp`: snapshots taken before this time (by an earlier process) are
/// untrusted. `None` before the client is ready (the JS `+Infinity`: nothing is trusted yet). [`set_trusted_since`]
/// overrides it (tests).
pub fn trusted_since(app: &App) -> Option<i64> {
    let over = *lock(&st(app).trusted_since);
    over.or_else(|| app.ready_at_ms())
}

/// Sets the time from which snapshots are trusted, overriding `client.readyTimestamp` (tests).
pub fn set_trusted_since(app: &App, ms: i64) {
    *lock(&st(app).trusted_since) = Some(ms);
}

/// The guild mutex of the tracker (joins, leaves and snapshot edits queue on it in call order).
pub fn guild_mutex(app: &App) -> bot_core::sync::KeyedMutex {
    st(app).mutex.clone()
}

// ── Snapshot store (used only inside the guild mutex) ─────────────────────────
// FAIL-CLOSED for attribution: a cache miss with an empty memo means "no snapshot". Writes always land in the memo.

async fn load_snapshot(app: &App, guild_id: &str) -> Option<Snapshot> {
    if let Some(hit) = lock(&st(app).memo).get(guild_id).cloned() {
        return Some(hit);
    }
    let raw = app.cache().get(&snapshot_key(guild_id)).await?;
    let snap = parse_snapshot(&raw)?;
    memo_set(app, guild_id, snap.clone());
    Some(snap)
}

fn memo_set(app: &App, guild_id: &str, snap: Snapshot) {
    let s = st(app);
    let mut memo = lock(&s.memo);
    memo.shift_remove(guild_id);
    memo.insert(guild_id.to_string(), snap);
    while memo.len() > MEMO_MAX_GUILDS {
        memo.shift_remove_index(0);
    }
}

async fn save_snapshot(app: &App, guild_id: &str, snap: Snapshot) {
    let json = snap.to_json();
    memo_set(app, guild_id, snap);
    app.cache().set(&snapshot_key(guild_id), &json, SNAPSHOT_TTL_SECONDS).await; // a false result is fine: the memo holds it
}

async fn drop_snapshot(app: &App, guild_id: &str) {
    lock(&st(app).memo).shift_remove(guild_id);
    app.cache().del_one(&snapshot_key(guild_id)).await;
}

/// The snapshot the tracker would use now (memo, then Redis); tests and diagnostics.
pub async fn current_snapshot(app: &App, guild_id: &str) -> Option<Snapshot> {
    load_snapshot(app, guild_id).await
}

/// Forgets the in-memory snapshot of a guild (Redis is untouched): what a process restart does.
pub fn forget_memo(app: &App, guild_id: &str) {
    lock(&st(app).memo).shift_remove(guild_id);
}

fn guild_available(app: &App, guild_id: &str) -> bool {
    app.guild_directory().guild_state(guild_id) == GuildState::Available
}

fn cached_vanity_code(app: &App, guild_id: GuildId) -> Option<String> {
    app.discord()?.cache.guild(guild_id)?.vanity_url_code.clone().filter(|c| !c.is_empty())
}

fn bot_user_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.cache.current_user().id)
}

/// `botCanTrack(guild)`: Manage Server is required for the invite list and the vanity URL. An uncached bot member is
/// optimistic.
pub fn bot_can_track(app: &App, guild_id: GuildId) -> bool {
    let Some(d) = app.discord() else { return true };
    let bot = d.cache.current_user().id;
    let Some(g) = d.cache.guild(guild_id) else { return true };
    let Some(me) = bot_core::permissions::me(&g, bot) else { return true };
    bot_core::permissions::js_has(bot_core::permissions::guild_permissions(&g, bot, &me.roles), Permissions::MANAGE_GUILD)
}

fn warn_no_permission(app: &App, guild_id: &str) {
    if st(app).perm_warn.allow(guild_id) {
        bot_util::log_warn!(format!("Invite tracking unavailable in {guild_id}: missing Manage Server"));
    }
}

/// One full observation (`observe(guild)`): `None` when the invite list could not be fetched.
async fn observe(app: &App, guild_id: GuildId) -> Option<Observation> {
    let api = invite_api(app);
    let vanity_code = cached_vanity_code(app, guild_id);
    let timeout = std::time::Duration::from_millis(FETCH_TIMEOUT_MS);
    let a1 = api.clone();
    let inv = tokio::time::timeout(timeout, async move { a1.fetch_invites(guild_id).await });
    let van = async {
        vanity_code.as_ref()?;
        match tokio::time::timeout(timeout, api.fetch_vanity(guild_id)).await {
            Ok(Ok(v)) => v,
            _ => None, // the baseline is unknown
        }
    };
    let (inv, vanity) = tokio::join!(inv, van);
    match inv {
        Ok(Ok(invites)) => Some(Observation { invites, vanity }),
        Ok(Err(e)) => {
            if matches!(e.code, Some(50013) | Some(50001)) {
                warn_no_permission(app, &guild_id.to_string());
            } else {
                bot_util::log_debug!(format!("Invite fetch failed in {guild_id}: {}", e.message));
            }
            None
        }
        Err(_) => {
            bot_util::log_debug!(format!("Invite fetch failed in {guild_id}: invites fetch timed out"));
            None
        }
    }
}

// ── Runtime ───────────────────────────────────────────────────────────────────

/// The result of [`refresh_snapshot`] (`{ ok, reason? }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refresh {
    pub ok: bool,
    pub reason: Option<&'static str>,
}

/// `refreshSnapshot(guild)`: warms (or re-baselines) the snapshot. Called by the `guildReady` hook and by `/module
/// enable invites`. Never fails. A stored snapshot from a previous process is untrusted and contributes nothing.
pub fn refresh_snapshot(app: &App, guild_id: GuildId) -> impl Future<Output = Refresh> + Send + 'static + use<> {
    let a = app.clone();
    let g = guild_id.to_string();
    st(app).mutex.run(&g.clone(), move || async move {
        if !guild_available(&a, &g) {
            return Refresh { ok: false, reason: Some("unavailable") };
        }
        if !bot_can_track(&a, guild_id) {
            warn_no_permission(&a, &g);
            return Refresh { ok: false, reason: Some("no-permission") };
        }
        let Some(observation) = observe(&a, guild_id).await else { return Refresh { ok: false, reason: Some("fetch-failed") } };
        let prev = load_snapshot(&a, &g).await;
        let previous = prev.filter(|p| is_trusted_snapshot(Some(p), trusted_since(&a)));
        let now = a.clock().now_ms();
        save_snapshot(&a, &g, build_snapshot(&observation, previous.as_ref(), now)).await;
        Refresh { ok: true, reason: None }
    })
}

/// `recordInviteCreate(guild, invite)`: an incremental edit, no REST call, nothing without a snapshot. Never fails.
pub fn record_invite_create(app: &App, guild_id: GuildId, invite: InviteState) -> impl Future<Output = ()> + Send + 'static + use<> {
    let a = app.clone();
    let g = guild_id.to_string();
    st(app).mutex.run(&g.clone(), move || async move {
        let Some(snap) = load_snapshot(&a, &g).await else { return };
        let now = a.clock().now_ms();
        save_snapshot(&a, &g, apply_invite_create(&snap, &invite, now)).await;
    })
}

/// `recordInviteDelete(guild, invite)`: the code becomes a tombstone. Never refetches. Never fails.
pub fn record_invite_delete(app: &App, guild_id: GuildId, code: String) -> impl Future<Output = ()> + Send + 'static + use<> {
    let a = app.clone();
    let g = guild_id.to_string();
    st(app).mutex.run(&g.clone(), move || async move {
        let Some(snap) = load_snapshot(&a, &g).await else { return };
        let now = a.clock().now_ms();
        save_snapshot(&a, &g, apply_invite_delete(&snap, &code, now)).await;
    })
}

async fn find_personal_invite_by_code(app: &App, code: &str) -> anyhow::Result<Option<PersonalInvite>> {
    Ok(SelectBuilder::new(&bot_db::PERSONAL_INVITE_INFO).filter("code", json!(code))?.build()?.fetch_optional(app.db()).await?)
}

/// Attribution for one join; runs inside the guild mutex.
async fn attribute(app: &App, guild_id: GuildId) -> (Option<Candidate>, &'static str) {
    let gid = guild_id.to_string();
    if !guild_available(app, &gid) {
        return (None, "unavailable");
    }
    let before = load_snapshot(app, &gid).await;
    let Some(observation) = observe(app, guild_id).await else { return (None, "fetch-failed") };
    let now = app.clock().now_ms();
    let Some(before) = before.filter(|b| is_trusted_snapshot(Some(b), trusted_since(app))) else {
        // Missing, legacy or left over from a previous process: never diff, just re-baseline.
        save_snapshot(app, &gid, build_snapshot(&observation, None, now)).await;
        return (None, "no-snapshot");
    };
    let r = resolve_join(&before, &observation, now);
    save_snapshot(app, &gid, r.next.clone()).await;
    let Some(m) = r.matched else { return (None, r.reason) };
    if m.kind == CandidateKind::Vanity {
        return (Some(m), r.reason);
    }
    let mut inviter = m.inviter_id.clone();
    if let Ok(Some(p)) = find_personal_invite_by_code(app, &m.code).await
        && p.guild_id == gid
    {
        inviter = Some(p.user_id); // /invites link codes are created by the bot
    }
    if let (Some(i), Some(bot)) = (&inviter, bot_user_id(app))
        && *i == bot.to_string()
    {
        inviter = None; // never credit the bot itself
    }
    (Some(Candidate { inviter_id: inviter, ..m }), r.reason)
}

/// `isEnabled` of [`track_join`]: whether the invites module is on (asked inside the guild mutex).
pub type IsEnabled = Box<dyn FnOnce() -> BoxFuture<'static, bool> + Send>;

async fn upsert_join(app: &App, gid: &str, user_id: &str, joined_ms: i64, matched: Option<&Candidate>) -> anyhow::Result<()> {
    app.settings().ensure_guild_settings(gid).await?;
    let joined = bot_db::format_js_date(&chrono::DateTime::from_timestamp_millis(joined_ms).unwrap_or_default());
    let inviter = matched.and_then(|m| m.inviter_id.clone());
    let code = matched.map(|m| m.code.clone());
    let data = [("joinedAt", json!(joined)), ("inviterId", json!(inviter)), ("inviteCode", json!(code)), ("leftAt", Value::Null)];
    InsertBuilder::new(&bot_db::GUILD_MEMBER_CACHE_INFO)
        .values([("guildId", json!(gid)), ("userId", json!(user_id))])?
        .values(data.clone())?
        .on_conflict_update(&["guildId", "userId"], data)?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    Ok(())
}

/// `trackJoin(member, { isEnabled })`: records one join (always) and attributes it when possible. The synchronous
/// prologue enqueues on the guild mutex before returning, so the queue order equals the call order (a join followed
/// by a leave is processed join first). Never fails; `None` is "unknown". `joined_ms` is `member.joinedAt` (now when
/// unknown); `is_enabled` `None` counts as enabled.
pub fn track_join(app: &App, guild_id: GuildId, user_id: UserId, joined_ms: Option<i64>, is_enabled: Option<IsEnabled>) -> impl Future<Output = Option<InviteInfo>> + Send + 'static + use<> {
    let gid = guild_id.to_string();
    let s = st(app);
    let burst = {
        let mut pending = lock(&s.pending);
        let waiting = pending.get(&gid).copied().unwrap_or(0);
        pending.insert(gid.clone(), waiting + 1);
        waiting >= MAX_PENDING_JOINS
    };
    let joined = joined_ms.unwrap_or_else(|| app.clock().now_ms());
    let a = app.clone();
    s.mutex.run(&gid.clone(), move || async move {
        {
            let s = st(&a);
            let mut pending = lock(&s.pending);
            let left = pending.get(&gid).copied().unwrap_or(1).saturating_sub(1);
            if left > 0 {
                pending.insert(gid.clone(), left);
            } else {
                pending.remove(&gid);
            }
        }
        let enabled = match is_enabled {
            Some(f) => f().await,
            None => true,
        };
        let (matched, reason) = if !enabled {
            drop_snapshot(&a, &gid).await; // the snapshot drifts while the module is off: re-baseline on the next join
            (None, "disabled")
        } else if burst {
            drop_snapshot(&a, &gid).await; // a raid: never queue more than MAX_PENDING_JOINS REST diffs per guild
            (None, "burst")
        } else if !bot_can_track(&a, guild_id) {
            warn_no_permission(&a, &gid);
            (None, "no-permission")
        } else {
            attribute(&a, guild_id).await
        };

        // The row is written for EVERY join, with a null inviter when unknown (latest join wins).
        if let Err(e) = upsert_join(&a, &gid, &user_id.to_string(), joined, matched.as_ref()).await
            && st(&a).db_warn.allow(&gid)
        {
            bot_util::log_warn!(format!("Invite join record failed in {gid}: {e}"));
        }
        bot_util::log_debug!(format!(
            "Invite attribution {gid}/{user_id}: {reason}{}",
            matched.as_ref().map(|m| format!(" {}", m.code)).unwrap_or_default()
        ));
        matched.map(|m| InviteInfo { vanity: m.kind == CandidateKind::Vanity, code: Some(m.code), inviter_id: m.inviter_id })
    })
}

/// `trackLeave(guild, userId)`: sets `leftAt` on the member's tracked join. Shares the join queue (a join followed at
/// once by a leave is processed in that order). Never creates a row and never fails. The work is queued when this is
/// called; awaiting the future is optional.
pub fn track_leave(app: &App, guild_id: GuildId, user_id: UserId) -> impl Future<Output = ()> + Send + 'static + use<> {
    let gid = guild_id.to_string();
    // A JS `Date` has millisecond precision: store exactly that (the column is timestamp(3), which would round).
    let left = chrono::DateTime::from_timestamp_millis(app.clock().now_ms()).unwrap_or_default();
    let a = app.clone();
    st(app).mutex.run(&gid.clone(), move || async move {
        let sql = r#"UPDATE "GuildMemberCache" SET "leftAt" = $1 WHERE "guildId" = $2 AND "userId" = $3 AND "leftAt" IS NULL"#;
        let r = bot_db::sqlx::query(sql).bind(left.naive_utc()).bind(&gid).bind(user_id.to_string()).execute(a.db()).await;
        if let Err(e) = r
            && st(&a).db_warn.allow(&gid)
        {
            bot_util::log_warn!(format!("Invite leave record failed in {gid}: {e}"));
        }
    })
}

/// Whether a code is alive (`getCodeState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLife {
    Live,
    Gone,
    Unknown,
}

impl CodeLife {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Gone => "gone",
            Self::Unknown => "unknown",
        }
    }
}

/// `getCodeState(guild, code, { allowFetch, since })`: reads only (outside the mutex). `since` is when the code was
/// created (epoch ms). Never fails.
pub async fn get_code_state(app: &App, guild_id: GuildId, code: &str, allow_fetch: bool, since: Option<i64>) -> CodeLife {
    let g = guild_id.to_string();
    let snap = load_snapshot(app, &g).await;
    if let Some(s) = &snap {
        if s.codes.contains_key(code) {
            return CodeLife::Live;
        }
        if s.tombstones.contains_key(code) {
            return CodeLife::Gone;
        }
        // A full observation taken after the link was created did not contain it.
        if since.is_some_and(|t| s.taken_at > t) {
            return CodeLife::Gone;
        }
    }
    if !allow_fetch {
        return CodeLife::Unknown;
    }
    let api = invite_api(app);
    match tokio::time::timeout(std::time::Duration::from_millis(FETCH_TIMEOUT_MS), api.fetch_invite_guild(code)).await {
        Ok(Ok(found)) => {
            if found.as_deref() == Some(g.as_str()) {
                CodeLife::Live
            } else {
                CodeLife::Gone
            }
        }
        Ok(Err(e)) if e.code == Some(10006) => CodeLife::Gone,
        _ => CodeLife::Unknown,
    }
}

/// `deleteDiscordInvite(guild, code, reason)`: best-effort. Never fails.
pub async fn delete_discord_invite(app: &App, guild_id: GuildId, code: &str, reason: &str) {
    if let Err(e) = invite_api(app).delete_invite(guild_id, code, reason).await {
        bot_util::log_debug!(format!("Could not delete invite {code} in {guild_id}: {}", e.message));
    }
}

// ── Stats SQL (must match compute_stats) ──────────────────────────────────────
// Every COUNT is cast ::int. "left" is a reserved word, so the column is "leftCount".

fn filters() -> String {
    format!(
        r#"
  COUNT(*)::int AS "total",
  COUNT(*) FILTER (WHERE "userId" <> "inviterId" AND "leftAt" IS NULL)::int AS "active",
  COUNT(*) FILTER (WHERE "userId" <> "inviterId" AND "leftAt" IS NOT NULL
                     AND "leftAt" - "joinedAt" >= interval '24 hours')::int AS "leftCount",
  COUNT(*) FILTER (WHERE "userId" = "inviterId"
                     OR ("leftAt" IS NOT NULL AND "leftAt" - "joinedAt" < interval '24 hours'))::int AS "fake",
  COUNT(*) FILTER (WHERE "userId" <> "inviterId" AND "leftAt" IS NULL
                     AND "joinedAt" > {} - interval '24 hours')::int AS "recent""#,
        bot_db::SQL_NOW
    )
}

const VALID: &str = r#"COUNT(*) FILTER (WHERE "userId" <> "inviterId"
                     AND ("leftAt" IS NULL OR "leftAt" - "joinedAt" >= interval '24 hours'))"#;

/// `statsQuery(guildId, userId)`: binds `$1` guild id, `$2` inviter id.
pub fn stats_sql() -> String {
    format!(r#"SELECT {} FROM "GuildMemberCache" WHERE "guildId" = $1 AND "inviterId" = $2"#, filters())
}

/// `leaderboardQuery(guildId, limit, excludeIds)`: binds `$1` guild id, `$2` exclude ids (text[]), `$3` limit.
pub fn leaderboard_sql() -> String {
    format!(
        r#"SELECT "inviterId", {}, {VALID}::int AS "valid"
  FROM "GuildMemberCache"
  WHERE "guildId" = $1 AND "inviterId" IS NOT NULL AND NOT ("inviterId" = ANY($2::text[]))
  GROUP BY "inviterId"
  HAVING {VALID} > 0
  ORDER BY "valid" DESC, "inviterId" ASC
  LIMIT $3"#,
        filters()
    )
}

/// One entry per raw statement (`RAW_SQL`): its name and SQL.
pub fn raw_sql() -> Vec<(&'static str, String)> {
    vec![("invites.stats", stats_sql()), ("invites.leaderboard", leaderboard_sql())]
}

fn map_stats(row: &bot_db::sqlx::postgres::PgRow) -> anyhow::Result<InviteStats> {
    use bot_db::sqlx::Row;
    let get = |c: &str| -> anyhow::Result<i64> { Ok(i64::from(row.try_get::<Option<i32>, _>(c)?.unwrap_or(0))) };
    let active = get("active")?;
    let left = get("leftCount")?;
    Ok(InviteStats { total: get("total")?, active, left, fake: get("fake")?, valid: active + left, recent: get("recent")? })
}

/// `getInviteStats(guildId, userId)`. Fails on a database error (the command catches).
pub async fn get_invite_stats(pool: &bot_db::PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<InviteStats> {
    let sql = stats_sql();
    let row = bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(sql)).bind(guild_id).bind(user_id).fetch_one(pool).await?;
    map_stats(&row)
}

/// One leaderboard row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardRow {
    pub inviter_id: String,
    #[serde(flatten)]
    pub stats: InviteStats,
}

/// `getInviteLeaderboard(guildId, { limit, excludeIds })`: top inviters by valid invites (zero-valid rows left out).
/// The JS default limit is 200.
pub async fn get_invite_leaderboard(pool: &bot_db::PgPool, guild_id: &str, limit: i64, exclude_ids: &[String]) -> anyhow::Result<Vec<LeaderboardRow>> {
    use bot_db::sqlx::Row;
    let limit = limit.clamp(1, 200);
    let rows = bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(leaderboard_sql())).bind(guild_id).bind(exclude_ids).bind(limit).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let stats = map_stats(r)?;
        let inviter: String = r.try_get("inviterId")?;
        let valid: i64 = i64::from(r.try_get::<Option<i32>, _>("valid")?.unwrap_or(0));
        if valid != stats.valid {
            bot_util::log_warn!(format!("Invite leaderboard: valid mismatch for {inviter} ({valid} vs {})", stats.valid));
        }
        out.push(LeaderboardRow { inviter_id: inviter, stats });
    }
    Ok(out)
}

/// `getJoinRecord(guildId, userId)`: who invited this member, when they joined and left.
pub async fn get_join_record(pool: &bot_db::PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<GuildMemberCache>> {
    Ok(SelectBuilder::new(&bot_db::GUILD_MEMBER_CACHE_INFO)
        .filter("guildId", json!(guild_id))?
        .filter("userId", json!(user_id))?
        .build()?
        .fetch_optional(pool)
        .await?)
}

/// `getPersonalInvite(guildId, userId)`.
pub async fn get_personal_invite(pool: &bot_db::PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<PersonalInvite>> {
    Ok(SelectBuilder::new(&bot_db::PERSONAL_INVITE_INFO)
        .filter("guildId", json!(guild_id))?
        .filter("userId", json!(user_id))?
        .build()?
        .fetch_optional(pool)
        .await?)
}

/// `savePersonalInvite({ guildId, userId, code, channelId })`: upserts the member's personal link; the update branch
/// resets `createdAt` (`getCodeState` compares against it).
pub async fn save_personal_invite(app: &App, guild_id: &str, user_id: &str, code: &str, channel_id: &str) -> anyhow::Result<PersonalInvite> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let now = bot_db::format_js_date(&app.clock().now());
    Ok(InsertBuilder::new(&bot_db::PERSONAL_INVITE_INFO)
        .values([("guildId", json!(guild_id)), ("userId", json!(user_id)), ("code", json!(code)), ("channelId", json!(channel_id)), ("createdAt", json!(now))])?
        .on_conflict_update(&["guildId", "userId"], [("code", json!(code)), ("channelId", json!(channel_id)), ("createdAt", json!(now))])?
        .build()?
        .fetch_one(app.db())
        .await?)
}
