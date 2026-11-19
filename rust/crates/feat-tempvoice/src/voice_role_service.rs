//! Port of `src/services/voiceRoleService.js`: Voice Roles. A member has a role for as long as they are in a linked
//! voice channel (or in any voice channel of a linked category), and the "any voice channel" roles while they are in
//! voice at all. Settings: `GuildSettings.voiceRoleLinks` (`[{ channelId, roleId }]`), `voiceRoleAnyRoleIds`,
//! `voiceRoleIgnoreAfk`. The hooks are [`crate::plugins::voice_roles`], the admin command
//! [`crate::commands::voicerole`].
//!
//! Every change recomputes the member's roles from their CURRENT voice state (the serenity cache): never "add on
//! join, remove on leave", because fast hops would race. Calls are one role at a time, serialised per member.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::interaction_helpers::{PermTarget, check_assignable_role_cached, missing_bot_permissions};
use bot_core::settings::{Settings, SettingsPatch};
use bot_core::sync::KeyedMutex;
use bot_config::limits;
use bot_db::GuildSettings;
use bot_util::log_warn;
use indexmap::IndexMap;
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildId, Member, RoleId, UserId, VoiceState};

use crate::discord_port::rest;

pub const ADD_REASON: &str = "Voice Roles: joined a linked voice channel";
pub const REMOVE_REASON: &str = "Voice Roles: not in a linked voice channel";
/// Most members one reconcile pass looks at (voice members first, then cached holders of a managed role).
pub const RECONCILE_MAX: usize = 1000;
pub const RECONCILE_CONCURRENCY: usize = 3;
/// The role cache lags behind the bot's own role calls until the gateway echo arrives, so the roles changed here are
/// remembered for a short time (bounded).
pub const OVERLAY_TTL_MS: i64 = 20_000;
pub const OVERLAY_MAX: usize = 2000;

/// "Unknown member" and "Unknown user": the member is gone.
const LEFT_CODES: [u64; 2] = [10007, 10013];
/// "Unknown role" and "Missing permissions": this role cannot be changed; the others still can.
const ROLE_FAIL_CODES: [u64; 2] = [10011, 50013];

/// `${guildId}:${userId}` -> roleId -> (held, until): the roles this service changed recently.
type Overlay = IndexMap<String, HashMap<String, (bool, i64)>>;

/// The module-level state of the JS service, per app.
pub struct VoiceRoleState {
    lock: KeyedMutex,
    warn_role: bot_util::async_util::Throttle,
    /// `${guildId}:${userId}` -> roleId -> (held, until)
    overlay: Mutex<Overlay>,
    running: Mutex<HashSet<String>>,
}

impl Default for VoiceRoleState {
    fn default() -> Self {
        Self {
            lock: KeyedMutex::new(),
            warn_role: bot_util::async_util::create_throttle(3_600_000),
            overlay: Mutex::default(),
            running: Mutex::default(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn state(app: &App) -> Arc<VoiceRoleState> {
    app.ext::<VoiceRoleState>()
}

/// `runLocked(guildId, userId, fn)`: one member's role changes happen one at a time.
pub fn run_locked<T, F, Fut>(app: &App, guild_id: &str, user_id: &str, f: F) -> impl Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    state(app).lock.run(&format!("{guild_id}:{user_id}"), f)
}

// ── Settings shapes ───────────────────────────────────────────────────────────

/// One `voiceRoleLinks` entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct VoiceRoleLink {
    #[serde(rename = "channelId")]
    pub channel_id: String,
    #[serde(rename = "roleId")]
    pub role_id: String,
}

/// The link rows of a raw `voiceRoleLinks` value, tolerant of junk; duplicates dropped.
pub fn links_of_value(raw: &Value) -> Vec<VoiceRoleLink> {
    let mut out: Vec<VoiceRoleLink> = Vec::new();
    let Some(items) = raw.as_array() else { return out };
    for l in items {
        let (Some(c), Some(r)) = (l.get("channelId").and_then(Value::as_str), l.get("roleId").and_then(Value::as_str)) else { continue };
        if c.is_empty() || r.is_empty() {
            continue;
        }
        let link = VoiceRoleLink { channel_id: c.to_string(), role_id: r.to_string() };
        if !out.contains(&link) {
            out.push(link);
        }
    }
    out
}

/// `linksOf(settings)`.
pub fn links_of(settings: Option<&GuildSettings>) -> Vec<VoiceRoleLink> {
    settings.map(|s| links_of_value(&s.voice_role_links)).unwrap_or_default()
}

fn unique_ids(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in raw {
        if !id.is_empty() && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// `anyRolesOf(settings)`: the "any voice channel" role ids (no duplicates).
pub fn any_roles_of(settings: Option<&GuildSettings>) -> Vec<String> {
    settings.map(|s| unique_ids(&s.voice_role_any_role_ids)).unwrap_or_default()
}

/// `hasVoiceRoles(settings)`: a link or an any-role is configured.
pub fn has_voice_roles(settings: Option<&GuildSettings>) -> bool {
    !links_of(settings).is_empty() || !any_roles_of(settings).is_empty()
}

/// The managed roles of a link list plus an any-list (in order, unique).
pub fn managed_role_ids_of(links: &[VoiceRoleLink], any: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in links.iter().map(|l| &l.role_id).chain(any.iter()) {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// `managedRoleIds(settings)`: every role this module owns (the roles of all links plus the any-list). Role
/// Persistence uses it to leave these roles out of what it gives back.
pub fn managed_role_ids(settings: Option<&GuildSettings>) -> Vec<String> {
    managed_role_ids_of(&links_of(settings), &any_roles_of(settings))
}

// ── Voice state and the roles a member should have ────────────────────────────

/// The part of a voice state the rules need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSpot {
    pub channel_id: String,
    pub parent_id: Option<String>,
}

/// `voiceOf(state, guild)`: `{ channelId, parentId }`, or `None` when not in a voice channel.
pub fn voice_of(state: Option<&VoiceState>, guild: Option<&Guild>) -> Option<VoiceSpot> {
    let channel = state?.channel_id?;
    let parent = guild.and_then(|g| g.channels.get(&channel)).and_then(|c| c.parent_id).map(|p| p.to_string());
    Some(VoiceSpot { channel_id: channel.to_string(), parent_id: parent })
}

/// `desiredRoleIds(settings, guild, voice)`: the links whose channel is the member's channel or its category, plus
/// the any-list while in any voice channel; nothing when not in voice, or in the AFK channel while
/// `voiceRoleIgnoreAfk` is on.
pub fn desired_role_ids(settings: Option<&GuildSettings>, afk_channel_id: Option<&str>, voice: Option<&VoiceSpot>) -> Vec<String> {
    let mut want: Vec<String> = Vec::new();
    let Some(voice) = voice else { return want };
    if voice.channel_id.is_empty() {
        return want;
    }
    let ignore_afk = settings.map(|s| s.voice_role_ignore_afk).unwrap_or(true);
    if ignore_afk && afk_channel_id.is_some_and(|a| a == voice.channel_id) {
        return want;
    }
    let mut add = |id: &String| {
        if !want.contains(id) {
            want.push(id.clone());
        }
    };
    for l in links_of(settings) {
        if l.channel_id == voice.channel_id || voice.parent_id.as_deref().is_some_and(|p| p == l.channel_id) {
            add(&l.role_id);
        }
    }
    for id in any_roles_of(settings) {
        add(&id);
    }
    want
}

/// The guild's AFK channel.
pub fn afk_channel_of(guild: &Guild) -> Option<String> {
    guild.afk_metadata.as_ref().map(|m| m.afk_channel_id.to_string())
}

// ── Recently changed roles (the cache lags behind our own calls) ──────────────

fn held_now(app: &App, key: &str, cached: bool, role_id: &str, now: i64) -> bool {
    let st = state(app);
    let mut overlay = lock(&st.overlay);
    let Some(mine) = overlay.get_mut(key) else { return cached };
    let Some((held, until)) = mine.get(role_id).copied() else { return cached };
    if until <= now || held == cached {
        mine.remove(role_id); // expired, or the cache caught up
        if mine.is_empty() {
            overlay.shift_remove(key);
        }
        return cached;
    }
    held
}

fn note_role(app: &App, key: &str, role_id: &str, held: bool, now: i64) {
    let st = state(app);
    let mut overlay = lock(&st.overlay);
    let mut mine = match overlay.shift_remove(key) {
        Some(m) => m, // re-inserted below, so the oldest member is always first
        None => {
            while overlay.len() >= OVERLAY_MAX {
                overlay.shift_remove_index(0);
            }
            HashMap::new()
        }
    };
    mine.insert(role_id.to_string(), (held, now + OVERLAY_TTL_MS));
    overlay.insert(key.to_string(), mine);
}

// ── Changing a member's roles ─────────────────────────────────────────────────

/// What [`apply_member`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApplyStatus {
    #[default]
    Ok,
    Skipped,
    Blocked,
    Left,
    Failed,
}

impl ApplyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyStatus::Ok => "ok",
            ApplyStatus::Skipped => "skipped",
            ApplyStatus::Blocked => "blocked",
            ApplyStatus::Left => "left",
            ApplyStatus::Failed => "failed",
        }
    }
}

/// `{ status, added, removed, skipped }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyResult {
    pub status: ApplyStatus,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: usize,
}

/// The options of [`apply_member`].
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Used only when the guild is not cached.
    pub voice: Option<VoiceSpot>,
    /// A fixed clock (tests); by default the app clock is read at each step.
    pub now: Option<i64>,
    /// Extra roles treated as managed (the roles of links that were just dropped).
    pub also_managed: Vec<String>,
}

/// `applyMember(guild, member, settings, opts)`: brings one member's managed roles in line with their current voice
/// state. Serialised per member. Skips bots and roles the bot cannot change (a role with a dangerous permission is
/// never added). Never fails.
pub async fn apply_member(app: &App, guild_id: GuildId, member: &Member, settings: Option<Settings>, opts: ApplyOptions) -> ApplyResult {
    if member.user.bot {
        return ApplyResult { status: ApplyStatus::Skipped, ..Default::default() };
    }
    let (a, user) = (app.clone(), member.user.id);
    let roles = member.roles.clone();
    let fut = run_locked(app, &guild_id.to_string(), &user.to_string(), move || async move { apply_locked(&a, guild_id, user, roles, settings, opts).await });
    fut.await
}

async fn apply_locked(app: &App, guild_id: GuildId, user: UserId, fallback_roles: Vec<RoleId>, settings: Option<Settings>, opts: ApplyOptions) -> ApplyResult {
    let mut result = ApplyResult::default();
    let clock = |app: &App| opts.now.unwrap_or_else(|| app.clock().now_ms());
    let key = format!("{guild_id}:{user}");
    let s = settings.as_deref();
    // The guild's voice-state cache is the truth (it is updated before the event); the event's state only without it.
    let view = bot_core::interaction_helpers::cached_guild(app, guild_id);
    let (voice, afk, member_roles) = match &view {
        Some(g) => (
            voice_of(g.voice_states.get(&user), Some(g)),
            afk_channel_of(g),
            g.members.get(&user).map(|m| m.roles.clone()).unwrap_or(fallback_roles),
        ),
        None => (opts.voice.clone(), None, fallback_roles),
    };
    let want = desired_role_ids(s, afk.as_deref(), voice.as_ref());
    let mut managed = managed_role_ids(s);
    for id in &opts.also_managed {
        if !managed.contains(id) {
            managed.push(id.clone());
        }
    }
    let mut to_add: Vec<RoleId> = Vec::new();
    let mut to_remove: Vec<RoleId> = Vec::new();
    for role_id in managed {
        let Some(rid) = role_id.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new) else { continue };
        let exists = view.as_ref().is_some_and(|g| g.roles.contains_key(&rid));
        if !exists {
            continue; // deleted (the roleDelete hook drops it from the settings)
        }
        let cached = member_roles.contains(&rid);
        let held = held_now(app, &key, cached, &role_id, clock(app));
        if want.contains(&role_id) && !held {
            to_add.push(rid);
        } else if !want.contains(&role_id) && held {
            to_remove.push(rid);
        }
    }
    if to_add.is_empty() && to_remove.is_empty() {
        return result;
    }
    let st = state(app);
    if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageRoles"]).is_empty() {
        if st.warn_role.allow(&format!("{guild_id}:*:perm")) {
            log_warn!(format!("[voice-roles] {guild_id}: cannot change voice roles, the bot lacks Manage Roles"));
        }
        result.status = ApplyStatus::Blocked;
        return result;
    }
    let steps: Vec<(RoleId, bool)> = to_add.into_iter().map(|r| (r, true)).chain(to_remove.into_iter().map(|r| (r, false))).collect();
    for (role, adding) in steps {
        // A role that cannot be given is skipped (and one that became dangerous is never given); removing a role the
        // bot can reach is always fine.
        let chk = check_assignable_role_cached(app, guild_id, role, None, !adding);
        let verb = if adding { "add" } else { "remove" };
        if !chk.ok {
            result.skipped += 1;
            if st.warn_role.allow(&format!("{guild_id}:{role}:{verb}")) {
                log_warn!(format!("[voice-roles] {guild_id}: skipping role {role}: {}", chk.reason.unwrap_or_default()));
            }
            continue;
        }
        let call = if adding { rest(app).add_role(guild_id, user, role, ADD_REASON).await } else { rest(app).remove_role(guild_id, user, role, REMOVE_REASON).await };
        match call {
            Ok(()) => {
                note_role(app, &key, &role.to_string(), adding, clock(app));
                if adding {
                    result.added.push(role.to_string());
                } else {
                    result.removed.push(role.to_string());
                }
            }
            Err(e) => {
                let give = if adding { "give" } else { "take" };
                if e.code.is_some_and(|c| LEFT_CODES.contains(&c)) {
                    result.status = ApplyStatus::Left;
                    return result;
                }
                if e.code.is_some_and(|c| ROLE_FAIL_CODES.contains(&c)) {
                    result.skipped += 1;
                    if st.warn_role.allow(&format!("{guild_id}:{role}:{verb}")) {
                        log_warn!(format!("[voice-roles] {guild_id}: could not {give} role {role}: {}", e.message));
                    }
                    continue;
                }
                log_warn!(format!("[voice-roles] {guild_id}: could not {give} role {role} for {user}: {}", e.message));
                result.status = ApplyStatus::Failed;
                return result;
            }
        }
    }
    result
}

// ── Reconcile (startup and /voicerole sync) ───────────────────────────────────

/// `isReconciling(guildId)`: a reconcile of the guild is running.
pub fn is_reconciling(app: &App, guild_id: &str) -> bool {
    lock(&state(app).running).contains(guild_id)
}

/// The result of [`reconcile_guild`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileResult {
    pub busy: bool,
    pub checked: usize,
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    pub failed: usize,
    pub truncated: bool,
}

struct Targets {
    list: Vec<Member>,
    truncated: bool,
}

fn take(targets: &mut Vec<Member>, truncated: &mut bool, max: usize, member: Option<Member>) {
    let Some(m) = member else { return };
    if m.user.bot || targets.iter().any(|t| t.user.id == m.user.id) {
        return;
    }
    if targets.len() >= max {
        *truncated = true;
        return;
    }
    targets.push(m);
}

/// `collectTargets(guild, settings, max)`: everyone in a voice channel (bots excluded), then cached holders of a
/// managed role. Caches only.
fn collect_targets(guild: &Guild, settings: Option<&GuildSettings>, max: usize) -> Targets {
    let mut list = Vec::new();
    let mut truncated = false;
    let mut states: Vec<&VoiceState> = guild.voice_states.values().filter(|v| v.channel_id.is_some()).collect();
    states.sort_by_key(|v| v.user_id);
    for v in states {
        let m = v.member.clone().or_else(|| guild.members.get(&v.user_id).cloned());
        take(&mut list, &mut truncated, max, m);
    }
    let managed = managed_role_ids(settings);
    if !managed.is_empty() {
        let mut members: Vec<&Member> = guild.members.values().collect();
        members.sort_by_key(|m| m.user.id);
        for m in members {
            if m.roles.iter().any(|r| managed.contains(&r.to_string())) {
                take(&mut list, &mut truncated, max, Some(m.clone()));
            }
        }
    }
    Targets { list, truncated }
}

/// `reconcileGuild(guild, settings, { max })`: gives every member in voice their roles and takes the managed roles
/// from cached holders who are not in voice. Bounded, caches only, one pass per guild at a time. Never fails.
pub async fn reconcile_guild(app: &App, guild_id: GuildId, settings: Option<Settings>, max: usize) -> ReconcileResult {
    let mut out = ReconcileResult::default();
    let g = guild_id.to_string();
    {
        let st = state(app);
        let mut running = lock(&st.running);
        if running.contains(&g) {
            out.busy = true;
            return out;
        }
        running.insert(g.clone());
    }
    let targets = bot_core::interaction_helpers::cached_guild(app, guild_id).map(|gd| collect_targets(&gd, settings.as_deref(), max));
    if let Some(Targets { list, truncated }) = targets {
        out.truncated = truncated;
        out.checked = list.len();
        let results = bot_util::async_util::map_limit(list, RECONCILE_CONCURRENCY, |m, _| {
            let (a, s) = (app.clone(), settings.clone());
            async move { Ok::<_, std::convert::Infallible>(apply_member(&a, guild_id, &m, s, ApplyOptions::default()).await) }
        })
        .await;
        for v in results.into_iter().flatten() {
            out.added += v.added.len();
            out.removed += v.removed.len();
            if !v.added.is_empty() || !v.removed.is_empty() {
                out.changed += 1;
            }
            if matches!(v.status, ApplyStatus::Failed | ApplyStatus::Blocked) {
                out.failed += 1;
            }
        }
    }
    lock(&state(app).running).remove(&g);
    out
}

/// The result of [`release_dropped_roles`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReleaseResult {
    pub checked: usize,
    pub removed: usize,
    pub failed: usize,
    pub truncated: bool,
}

/// `releaseDroppedRoles(guild, settings, roleIds, { max })`: a linked channel or category was deleted and its links
/// dropped; the cached members who hold one of those roles (or just got it, per the overlay) and are not entitled to
/// it by `settings` (the row after the drop) lose it. Bounded like reconcile. Never fails.
pub async fn release_dropped_roles(app: &App, guild_id: GuildId, settings: Option<Settings>, role_ids: &[String], max: usize) -> ReleaseResult {
    let mut out = ReleaseResult::default();
    let dropped = unique_ids(role_ids);
    if dropped.is_empty() {
        return out;
    }
    let Some(guild) = bot_core::interaction_helpers::cached_guild(app, guild_id) else { return out };
    let mut list: Vec<Member> = Vec::new();
    let mut members: Vec<&Member> = guild.members.values().collect();
    members.sort_by_key(|m| m.user.id);
    for m in members {
        if m.roles.iter().any(|r| dropped.contains(&r.to_string())) {
            take(&mut list, &mut out.truncated, max, Some(m.clone()));
        }
    }
    let prefix = format!("{guild_id}:");
    let overlay_users: Vec<String> = lock(&state(app).overlay)
        .iter()
        .filter(|(k, mine)| k.starts_with(&prefix) && dropped.iter().any(|id| mine.get(id).is_some_and(|(held, _)| *held)))
        .map(|(k, _)| k[prefix.len()..].to_string())
        .collect();
    for u in overlay_users {
        let m = u.parse::<u64>().ok().filter(|v| *v != 0).and_then(|v| guild.members.get(&UserId::new(v)).cloned());
        take(&mut list, &mut out.truncated, max, m);
    }
    out.checked = list.len();
    let results = bot_util::async_util::map_limit(list, RECONCILE_CONCURRENCY, |m, _| {
        let (a, s, d) = (app.clone(), settings.clone(), dropped.clone());
        async move { Ok::<_, std::convert::Infallible>(apply_member(&a, guild_id, &m, s, ApplyOptions { also_managed: d, ..Default::default() }).await) }
    })
    .await;
    for v in results.into_iter().flatten() {
        out.removed += v.removed.len();
        if matches!(v.status, ApplyStatus::Failed | ApplyStatus::Blocked) {
            out.failed += 1;
        }
    }
    out
}

/// `resetState()`: forgets the overlay and the running set (tests).
pub fn reset_state(app: &App) {
    let st = state(app);
    lock(&st.overlay).clear();
    lock(&st.running).clear();
}

// ── Settings edits (command) ──────────────────────────────────────────────────

/// The outcome of an add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Changed,
    Same,
    Full,
}

/// The outcome of a remove: whether it changed anything, and whether the role is still managed (another link or
/// the any-list), so the bot keeps taking it from members outside voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub changed: bool,
    pub still_managed: bool,
}

/// `addLink(guildId, channelId, roleId)`: links a channel (voice, stage or category) to a role; the cap and the
/// duplicate check run under the settings row lock.
pub async fn add_link(app: &App, guild_id: &str, channel_id: &str, role_id: &str) -> anyhow::Result<AddOutcome> {
    let outcome = Arc::new(Mutex::new(AddOutcome::Changed));
    let (o, c, r) = (outcome.clone(), channel_id.to_string(), role_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            let mut list = links_of(Some(&cur));
            if list.iter().any(|l| l.channel_id == c && l.role_id == r) {
                *lock(&o) = AddOutcome::Same;
                return Ok(SettingsPatch::new());
            }
            if list.len() >= limits::VOICE_ROLE_LINKS {
                *lock(&o) = AddOutcome::Full;
                return Ok(SettingsPatch::new());
            }
            *lock(&o) = AddOutcome::Changed;
            list.push(VoiceRoleLink { channel_id: c, role_id: r });
            Ok(SettingsPatch::new().set("voiceRoleLinks", json!(list)))
        })
        .await?;
    let v = *lock(&outcome);
    Ok(v)
}

/// `removeLink(guildId, channelId, roleId)`.
pub async fn remove_link(app: &App, guild_id: &str, channel_id: &str, role_id: &str) -> anyhow::Result<RemoveOutcome> {
    let outcome = Arc::new(Mutex::new(RemoveOutcome { changed: false, still_managed: false }));
    let (o, c, r) = (outcome.clone(), channel_id.to_string(), role_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            let list = links_of(Some(&cur));
            let kept: Vec<VoiceRoleLink> = list.iter().filter(|l| !(l.channel_id == c && l.role_id == r)).cloned().collect();
            let still = managed_role_ids_of(&kept, &unique_ids(&cur.voice_role_any_role_ids)).contains(&r);
            let mut out = lock(&o);
            out.still_managed = still;
            if kept.len() == list.len() {
                return Ok(SettingsPatch::new());
            }
            out.changed = true;
            Ok(SettingsPatch::new().set("voiceRoleLinks", json!(kept)))
        })
        .await?;
    let v = *lock(&outcome);
    Ok(v)
}

/// `addAnyRole(guildId, roleId)`: adds a role to the any-voice-channel list.
pub async fn add_any_role(app: &App, guild_id: &str, role_id: &str) -> anyhow::Result<AddOutcome> {
    let outcome = Arc::new(Mutex::new(AddOutcome::Changed));
    let (o, r) = (outcome.clone(), role_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            let mut list = any_roles_of(Some(&cur));
            if list.contains(&r) {
                *lock(&o) = AddOutcome::Same;
                return Ok(SettingsPatch::new());
            }
            if list.len() >= limits::VOICE_ROLE_ANY {
                *lock(&o) = AddOutcome::Full;
                return Ok(SettingsPatch::new());
            }
            *lock(&o) = AddOutcome::Changed;
            list.push(r);
            Ok(SettingsPatch::new().set("voiceRoleAnyRoleIds", json!(list)))
        })
        .await?;
    let v = *lock(&outcome);
    Ok(v)
}

/// `removeAnyRole(guildId, roleId)`.
pub async fn remove_any_role(app: &App, guild_id: &str, role_id: &str) -> anyhow::Result<RemoveOutcome> {
    let outcome = Arc::new(Mutex::new(RemoveOutcome { changed: false, still_managed: false }));
    let (o, r) = (outcome.clone(), role_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            let list = any_roles_of(Some(&cur));
            let kept: Vec<String> = list.iter().filter(|id| **id != r).cloned().collect();
            let still = managed_role_ids_of(&links_of_value(&cur.voice_role_links), &kept).contains(&r);
            let mut out = lock(&o);
            out.still_managed = still;
            if kept.len() == list.len() {
                return Ok(SettingsPatch::new());
            }
            out.changed = true;
            Ok(SettingsPatch::new().set("voiceRoleAnyRoleIds", json!(kept)))
        })
        .await?;
    let v = *lock(&outcome);
    Ok(v)
}

// ── Cleanup patches (pure; used by the always-on delete hooks) ────────────────

/// `patchDropChannel(cur, channelId)`: the new `voiceRoleLinks` without the links of a deleted channel or category,
/// or `None` when there is none.
pub fn patch_drop_channel(cur: &GuildSettings, channel_id: &str) -> Option<SettingsPatch> {
    let raw = cur.voice_role_links.as_array().cloned().unwrap_or_default();
    let kept: Vec<Value> = raw.iter().filter(|l| l.get("channelId").and_then(Value::as_str) != Some(channel_id)).cloned().collect();
    (kept.len() != raw.len()).then(|| SettingsPatch::new().set("voiceRoleLinks", Value::Array(kept)))
}

/// `patchDropRole(cur, roleId)`: drops a deleted role from the links and the any-list, or `None` when it is in
/// neither.
pub fn patch_drop_role(cur: &GuildSettings, role_id: &str) -> Option<SettingsPatch> {
    let raw_links = cur.voice_role_links.as_array().cloned().unwrap_or_default();
    let links: Vec<Value> = raw_links.iter().filter(|l| l.get("roleId").and_then(Value::as_str) != Some(role_id)).cloned().collect();
    let any: Vec<String> = cur.voice_role_any_role_ids.iter().filter(|id| *id != role_id).cloned().collect();
    let mut patch = SettingsPatch::new();
    if links.len() != raw_links.len() {
        patch.insert("voiceRoleLinks", Value::Array(links));
    }
    if any.len() != cur.voice_role_any_role_ids.len() {
        patch.insert("voiceRoleAnyRoleIds", json!(any));
    }
    (!patch.is_empty()).then_some(patch)
}

/// The channel id of a voice state (for callers that only have the serenity type).
pub fn channel_of(state: Option<&VoiceState>) -> Option<ChannelId> {
    state.and_then(|s| s.channel_id)
}
