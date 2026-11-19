//! Port of `src/services/rolePersistService.js`: Role Persistence's table access and rules.
//!
//! A member who leaves has their roles (and nickname) saved in `PersistedMember`; when they rejoin within
//! `GuildSettings.rolePersistDays` the roles that are still safe to give are given back. This file holds the table
//! access and the rules (what to save, what to give back, which gates hold a restore); the hooks and the restore are in
//! [`crate::plugins::role_persist`], the admin command is [`crate::commands::rolepersist`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::interaction_helpers::check_assignable_role;
use bot_core::sync::KeyedMutex;
use bot_db::{DbError, GuildSettings, InsertBuilder, PERSISTED_MEMBER_INFO, PersistedMember, SqlValue};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::json;
use serenity::all::{Guild, Member, RoleId, UserId};

/// `ROLEPERSIST`.
pub mod consts {
    pub const DEFAULT_DAYS: i64 = 90;
    pub const MIN_DAYS: i64 = 1;
    pub const MAX_DAYS: i64 = 365;
    pub const DAY_MS: i64 = 86_400_000;
    /// Expired rows of a guild are deleted at most this often (when a member leaves).
    pub const PRUNE_EVERY_MS: i64 = 3_600_000;
    pub const NICKNAME_MAX: usize = 32;
    /// Members whose restore is waiting for Rules Screening / the captcha (in memory).
    pub const WAIT_MAX: usize = 5000;
    pub const WAIT_TTL_MS: i64 = 7 * 86_400_000;
    /// A member update with an uncached old member is only looked at for members who joined this recently.
    pub const RECENT_JOIN_MS: i64 = 86_400_000;
    /// The most events one restore looks at when checking which saved roles came from an ended event.
    pub const EVENT_MARKERS_MAX: i64 = 200;
}
use consts as RP;

/// What the JS keeps in module-level state: the member lock, the prune stamps and the waiting set.
#[derive(Default)]
pub struct RolePersistState {
    lock: KeyedMutex,
    pruned: Mutex<HashMap<String, i64>>,
    waiting: Mutex<IndexMap<String, i64>>,
}

fn st(app: &App) -> Arc<RolePersistState> {
    app.ext::<RolePersistState>()
}

fn guard<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `runLocked(guildId, userId, fn)`: runs `f` while holding the member's lock (queued when called).
pub fn run_locked<T, F, Fut>(app: &App, guild_id: &str, user_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    st(app).lock.run(&format!("{guild_id}:{user_id}"), f)
}

// ── Settings ──────────────────────────────────────────────────────────────────

/// `retentionDays(settings)`: how many days a saved row stays valid (1..365).
pub fn retention_days(settings: Option<&GuildSettings>) -> i64 {
    match settings {
        None => RP::DEFAULT_DAYS,
        Some(s) => i64::from(s.role_persist_days).clamp(RP::MIN_DAYS, RP::MAX_DAYS),
    }
}

/// `retentionDays` of a raw JSON value (`Number(settings?.rolePersistDays)`), for dashboard input.
pub fn retention_days_of(value: Option<&serde_json::Value>) -> i64 {
    let Some(value) = value else { return RP::DEFAULT_DAYS };
    let n = match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let t = bot_util::js::trim(s);
            if t.is_empty() { Some(0.0) } else { t.parse::<f64>().ok() }
        }
        serde_json::Value::Null => Some(0.0),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    };
    match n.filter(|f| f.is_finite()) {
        None => RP::DEFAULT_DAYS,
        Some(f) => (f.trunc() as i64).clamp(RP::MIN_DAYS, RP::MAX_DAYS),
    }
}

/// `expiresAt(row, settings)`: when a row saved at `left_at` stops being valid, as epoch ms.
pub fn expires_at(left_at: &DateTime<Utc>, settings: Option<&GuildSettings>) -> i64 {
    left_at.timestamp_millis() + retention_days(settings) * RP::DAY_MS
}

/// `isStale(row, settings, now)`.
pub fn is_stale(left_at: &DateTime<Utc>, settings: Option<&GuildSettings>, now_ms: i64) -> bool {
    expires_at(left_at, settings) <= now_ms
}

// ── Table ─────────────────────────────────────────────────────────────────────

/// `loadRow(guildId, userId)`.
pub async fn load_row(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<Option<PersistedMember>> {
    Ok(sqlx::query_as::<_, PersistedMember>(r#"SELECT * FROM "PersistedMember" WHERE "guildId" = $1 AND "userId" = $2"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_optional(app.db())
        .await
        .map_err(DbError::from)?)
}

/// `countRows(guildId)`.
pub async fn count_rows(app: &App, guild_id: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "PersistedMember" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(app.db()).await.map_err(DbError::from)?;
    Ok(n)
}

/// `deleteRow(guildId, userId)`: how many rows went (0 or 1).
pub async fn delete_row(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<u64> {
    Ok(bot_db::delete_where(&PERSISTED_MEMBER_INFO, [("guildId", json!(guild_id)), ("userId", json!(user_id))])?.execute(app.db()).await?)
}

/// What is saved for a member: `{ roleIds, nickname }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub role_ids: Vec<String>,
    pub nickname: Option<String>,
}

/// `mergeSnapshot(old, snap)`: the role ids of both (no duplicates, the saved ones first), the newer nickname when one
/// is known and the older one otherwise.
pub fn merge_snapshot(old: Option<&Snapshot>, snap: &Snapshot) -> Snapshot {
    let Some(old) = old else { return snap.clone() };
    let mut role_ids: Vec<String> = Vec::new();
    for id in old.role_ids.iter().chain(snap.role_ids.iter()) {
        if !role_ids.contains(id) {
            role_ids.push(id.clone());
        }
    }
    Snapshot { role_ids, nickname: snap.nickname.clone().or_else(|| old.nickname.clone()) }
}

/// `saveRow(guildId, userId, snap, now, settings)`: created, or merged into the row that is still there, with
/// `leftAt` refreshed. An expired row is replaced, not merged. The caller holds the member's lock.
pub async fn save_row(app: &App, guild_id: &str, user_id: &str, snap: &Snapshot, now: DateTime<Utc>, settings: Option<&GuildSettings>) -> anyhow::Result<()> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let mut old = load_row(app, guild_id, user_id).await?;
    if let (Some(o), Some(s)) = (&old, settings)
        && is_stale(&o.left_at, Some(s), now.timestamp_millis())
    {
        old = None;
    }
    let old_snap = old.map(|o| Snapshot { role_ids: o.role_ids, nickname: o.nickname });
    let merged = merge_snapshot(old_snap.as_ref(), snap);
    InsertBuilder::new(&PERSISTED_MEMBER_INFO)
        .values([("guildId", json!(guild_id)), ("userId", json!(user_id)), ("roleIds", json!(merged.role_ids)), ("nickname", json!(merged.nickname))])?
        .typed("leftAt", SqlValue::timestamp(now))?
        .on_conflict_update(&["guildId", "userId"], [("roleIds", json!(merged.role_ids)), ("nickname", json!(merged.nickname)), ("leftAt", json!(bot_db::format_js_date(&now)))])?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    Ok(())
}

/// `pruneExpired(guildId, settings, now)`: deletes this guild's expired rows, at most once per guild per hour. Never
/// fails: `None` when it was skipped or failed.
pub async fn prune_expired(app: &App, guild_id: &str, settings: Option<&GuildSettings>, now_ms: i64) -> Option<u64> {
    {
        let s = st(app);
        let mut p = guard(&s.pruned);
        if let Some(last) = p.get(guild_id)
            && now_ms - last < RP::PRUNE_EVERY_MS
        {
            return None;
        }
        // Set first: a failing delete is not retried on every leave.
        p.insert(guild_id.to_string(), now_ms);
    }
    let cutoff = DateTime::from_timestamp_millis(now_ms - retention_days(settings) * RP::DAY_MS).unwrap_or_default();
    let res = sqlx::query(r#"DELETE FROM "PersistedMember" WHERE "guildId" = $1 AND "leftAt" < $2"#).bind(guild_id).bind(cutoff.naive_utc()).execute(app.db()).await;
    match res {
        Ok(r) => Some(r.rows_affected()),
        Err(e) => {
            bot_util::log_warn!(format!("[role-persist] {guild_id}: could not prune old rows: {e}"));
            None
        }
    }
}

/// `resetPruneStamps()` (tests).
pub fn reset_prune_stamps(app: &App) {
    guard(&st(app).pruned).clear();
}

// ── What to save ──────────────────────────────────────────────────────────────

/// `skippedRoleIds(settings)`: the ignored roles and the Voice Roles roles, never saved or given back.
pub fn skipped_role_ids(settings: Option<&GuildSettings>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |id: &str| {
        if !out.iter().any(|x| x == id) {
            out.push(id.to_string());
        }
    };
    let Some(s) = settings else { return Vec::new() };
    for id in &s.role_persist_ignored_role_ids {
        add(id);
    }
    for id in &s.voice_role_any_role_ids {
        add(id);
    }
    if let Some(links) = s.voice_role_links.as_array() {
        for l in links {
            if let Some(r) = l.get("roleId").and_then(|v| v.as_str()) {
                add(r);
            }
        }
    }
    out
}

/// `moduleRoleReasons(settings)`: roles another module gives and takes back on its own schedule (role id -> why).
pub fn module_role_reasons(settings: Option<&GuildSettings>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(s) = settings else { return out };
    if let Some(b) = s.birthday_role_id.as_deref().filter(|b| !b.is_empty()) {
        out.push((b.to_string(), "The birthday role is given and taken back by Birthdays.".into()));
    }
    if let Some(m) = s.mute_role_id.as_deref().filter(|m| !m.is_empty())
        && bot_core::settings::is_module_enabled(Some(s), Some(bot_config::ModuleKey::Moderation))
    {
        let reason = "The mute role is re-applied by Moderation while a mute is active.".to_string();
        match out.iter_mut().find(|(id, _)| id == m) {
            Some(e) => e.1 = reason,
            None => out.push((m.to_string(), reason)),
        }
    }
    out
}

fn reason_for<'a>(list: &'a [(String, String)], id: &str) -> Option<&'a str> {
    list.iter().find(|(r, _)| r == id).map(|(_, why)| why.as_str())
}

/// `snapshotMember(member, guild, settings)`: the roles and nickname to save for a member who left, or `None` when
/// there is nothing worth saving (an uncached member is never saved). @everyone, managed roles, roles that no longer
/// exist, ignored roles and roles another module owns are left out.
pub fn snapshot_member(member: Option<&Member>, guild: &Guild, settings: Option<&GuildSettings>) -> Option<Snapshot> {
    let member = member?;
    let ignored = skipped_role_ids(settings);
    let owned = module_role_reasons(settings);
    let mut role_ids = Vec::new();
    for id in &member.roles {
        let s = id.to_string();
        if id.get() == guild.id.get() || ignored.contains(&s) || reason_for(&owned, &s).is_some() {
            continue;
        }
        match guild.roles.get(id) {
            Some(r) if !r.managed => role_ids.push(s),
            _ => {}
        }
    }
    let nick = member.nick.as_deref().map(bot_util::js::trim).unwrap_or("");
    let nickname = (!nick.is_empty()).then(|| bot_util::text::clamp_text_with(nick, RP::NICKNAME_MAX, ""));
    if role_ids.is_empty() && nickname.is_none() {
        return None;
    }
    Some(Snapshot { role_ids, nickname })
}

// ── What to give back ─────────────────────────────────────────────────────────

/// A saved role that is not given back, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub role_id: String,
    pub reason: String,
    /// The role no longer exists.
    pub deleted: bool,
}

/// `planRoles`' result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub give: Vec<RoleId>,
    pub skipped: Vec<Skipped>,
}

/// `planRoles(guild, roleIds, settings, { held, skips })`: splits saved role ids into the roles to give back and the
/// ones skipped. `bot_id` is the bot's user id (for `checkAssignableRole`).
pub fn plan_roles(
    guild: &Guild,
    bot_id: UserId,
    role_ids: &[String],
    settings: Option<&GuildSettings>,
    held: Option<&[RoleId]>,
    skips: Option<&HashMap<String, String>>,
) -> Plan {
    let ignored = skipped_role_ids(settings);
    let owned = module_role_reasons(settings);
    let gate_role = settings.filter(|s| s.captcha_enabled).and_then(|s| s.captcha_role_id.clone());
    let mut plan = Plan::default();
    let mut seen = HashSet::new();
    for id in role_ids {
        if !seen.insert(id.clone()) || id == &guild.id.to_string() {
            continue;
        }
        let role = id.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new).and_then(|r| guild.roles.get(&r));
        let Some(role) = role else {
            plan.skipped.push(Skipped { role_id: id.clone(), reason: "The role no longer exists.".into(), deleted: true });
            continue;
        };
        if ignored.contains(id) {
            plan.skipped.push(Skipped { role_id: id.clone(), reason: "The role is on the never-give-back list.".into(), deleted: false });
            continue;
        }
        if let Some(why) = reason_for(&owned, id).map(str::to_string).or_else(|| skips.and_then(|s| s.get(id).cloned())) {
            plan.skipped.push(Skipped { role_id: id.clone(), reason: why, deleted: false });
            continue;
        }
        if gate_role.as_deref() == Some(id.as_str()) {
            plan.skipped.push(Skipped { role_id: id.clone(), reason: "The captcha role is only given by the captcha.".into(), deleted: false });
            continue;
        }
        if held.is_some_and(|h| h.contains(&role.id)) {
            continue;
        }
        let chk = check_assignable_role(guild, Some(role), bot_id, None, false);
        if chk.ok {
            plan.give.push(role.id);
        } else {
            plan.skipped.push(Skipped {
                role_id: id.clone(),
                reason: chk.reason.unwrap_or_else(|| "The role cannot be given.".into()),
                deleted: false,
            });
        }
    }
    plan
}

/// `lifecycleSkips`' result: role id -> why it is not given back, and the closed events behind an event-role skip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleSkips {
    pub skips: HashMap<String, String>,
    pub markers: Vec<String>,
}

/// `lifecycleSkips(guildId, userId, roleIds, settings, now)`: of the roles about to be given back, the ones whose
/// lifecycle is kept by the database and may have ended while the member was away (an event's role, the mute role
/// while Moderation is off). Fails on a database error (the restore then fails and the row is kept).
pub async fn lifecycle_skips(app: &App, guild_id: &str, user_id: &str, role_ids: &[String], settings: Option<&GuildSettings>, now: DateTime<Utc>) -> anyhow::Result<LifecycleSkips> {
    let mut out = LifecycleSkips::default();
    let mut ids: Vec<String> = Vec::new();
    for id in role_ids {
        if id != guild_id && !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        return Ok(out);
    }
    if let Some(s) = settings
        && let Some(mute) = s.mute_role_id.as_deref().filter(|m| !m.is_empty())
        && ids.iter().any(|i| i == mute)
        && !bot_core::settings::is_module_enabled(Some(s), Some(bot_config::ModuleKey::Moderation))
    {
        let active: Option<(i32,)> = sqlx::query_as(
            r#"SELECT "id" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 AND "type" = 'MUTE' AND "active" = true AND ("expiresAt" IS NULL OR "expiresAt" > $3) LIMIT 1"#,
        )
        .bind(guild_id)
        .bind(user_id)
        .bind(now.naive_utc())
        .fetch_optional(app.db())
        .await
        .map_err(DbError::from)?;
        if active.is_none() {
            out.skips.insert(mute.to_string(), "The mute has ended.".into());
        }
    }
    let granted: Vec<(String, Option<String>, String)> = sqlx::query_as(
        r#"SELECT e."id", e."roleId", e."status"::text FROM "GuildEvent" e WHERE e."guildId" = $1 AND e."roleId" = ANY($2) AND EXISTS (SELECT 1 FROM "EventRsvp" r WHERE r."eventId" = e."id" AND r."userId" = $3 AND r."roleGranted" = true) LIMIT $4"#,
    )
    .bind(guild_id)
    .bind(&ids)
    .bind(user_id)
    .bind(RP::EVENT_MARKERS_MAX)
    .fetch_all(app.db())
    .await
    .map_err(DbError::from)?;
    let open = |s: &str| s == "SCHEDULED" || s == "ACTIVE";
    let live: HashSet<String> = granted.iter().filter(|(_, _, s)| open(s)).filter_map(|(_, r, _)| r.clone()).collect();
    for (id, role, status) in &granted {
        let Some(role) = role else { continue };
        if open(status) || live.contains(role) {
            continue;
        }
        out.skips.insert(role.clone(), "The event this role was for is over.".into());
        out.markers.push(id.clone());
    }
    Ok(out)
}

/// `clearEventMarkers(userId, eventIds)`: clears the roleGranted flags lifecycle_skips acted on. Never fails.
pub async fn clear_event_markers(app: &App, user_id: &str, event_ids: &[String]) {
    if event_ids.is_empty() {
        return;
    }
    let res = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"UPDATE "EventRsvp" SET "roleGranted" = false, "updatedAt" = {} WHERE "userId" = $1 AND "eventId" = ANY($2) AND "roleGranted" = true"#,
        bot_db::SQL_NOW
    )))
    .bind(user_id)
    .bind(event_ids)
    .execute(app.db())
    .await;
    if let Err(e) = res {
        bot_util::log_warn!(format!("[role-persist] could not clear the event markers of {user_id}: {e}"));
    }
}

// ── Gates ─────────────────────────────────────────────────────────────────────

/// What holds a restore back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Rules Screening not passed.
    Pending,
    /// The captcha is not solved.
    Captcha,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Captcha => "captcha",
        }
    }
}

/// `gateOf(member, settings, { captchaPending })`. Never bypasses either gate.
pub fn gate_of(guild: Option<&Guild>, member: &Member, settings: Option<&GuildSettings>, captcha_pending: bool) -> Option<Gate> {
    if member.pending {
        return Some(Gate::Pending);
    }
    if captcha_pending || feat_welcome::welcome_service::is_captcha_gated(guild, member, settings) {
        return Some(Gate::Captcha);
    }
    None
}

fn wait_key(guild_id: &str, user_id: &str) -> String {
    format!("{guild_id}:{user_id}")
}

/// `markWaiting(guildId, userId, now)`: remembers that a member's restore waits for a gate (bounded: the oldest entry
/// goes first, entries expire).
pub fn mark_waiting(app: &App, guild_id: &str, user_id: &str, now_ms: i64) {
    let s = st(app);
    let mut w = guard(&s.waiting);
    let key = wait_key(guild_id, user_id);
    w.shift_remove(&key);
    while w.len() >= RP::WAIT_MAX {
        w.shift_remove_index(0);
    }
    w.insert(key, now_ms + RP::WAIT_TTL_MS);
}

/// `clearWaiting(guildId, userId)`.
pub fn clear_waiting(app: &App, guild_id: &str, user_id: &str) {
    guard(&st(app).waiting).shift_remove(&wait_key(guild_id, user_id));
}

/// `isWaiting(guildId, userId, now)`.
pub fn is_waiting(app: &App, guild_id: &str, user_id: &str, now_ms: i64) -> bool {
    let s = st(app);
    let mut w = guard(&s.waiting);
    let key = wait_key(guild_id, user_id);
    match w.get(&key).copied() {
        None => false,
        Some(until) if until <= now_ms => {
            w.shift_remove(&key);
            false
        }
        Some(_) => true,
    }
}

/// How many members are waiting (tests).
pub fn waiting_len(app: &App) -> usize {
    guard(&st(app).waiting).len()
}

/// `resetWaiting()` (tests).
pub fn reset_waiting(app: &App) {
    guard(&st(app).waiting).clear();
}
