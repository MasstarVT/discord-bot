//! Port of `src/plugins/liverole.js` (pkg-liverole): a role held while a member's Discord presence shows a Streaming
//! activity (Twitch or YouTube). The plugin declaration, the pure helpers and the engine ([`reconcile_member`],
//! [`sync_guild`], [`announce_live`]); [`crate::commands::liverole`] uses the public functions.
//!
//! Every read of the guild (roles, members, presences, channels) goes through [`crate::ops`]; the only state is per
//! app (`app.ext`): three keyed mutexes, the throttles and the two waiting sets.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bot_core::interaction_helpers::{RoleCheck, channel_usable_verdict, check_assignable_role, pingable_role_ids};
use bot_core::markdown::escape_markdown;
use bot_core::prelude::*;
use bot_core::sync::KeyedMutex;
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders};
use bot_util::text::{clamp_text, format_number};
use serenity::all::{ActivityType, Guild, GuildId, Presence, RoleId, UserId};

use crate::ops::{self, snowflake};

/// `LIVEROLE`.
pub mod limits {
    /// Role edits attempted per sync (adds + checks + removes).
    pub const SYNC_MAX_CHANGES: usize = 50;
    /// `/liverole set` refuses a role that would be stripped from more members than this.
    pub const SET_MAX_STRIP: usize = 50;
    /// Above this member count, syncs use the member cache only.
    pub const FULL_FETCH_MAX_MEMBERS: u64 = 25_000;
    pub const FULL_FETCH_TIMEOUT_MS: u64 = 60_000;
    pub const FULL_FETCH_INTERVAL_MS: u64 = 10 * 60_000;
    /// How long a command waits for its sync before answering "still syncing".
    pub const COMMAND_SYNC_WAIT_MS: u64 = 25_000;
    pub const WARN_INTERVAL_MS: u64 = 60 * 60_000;
    /// Holders listed by `/liverole status`.
    pub const STATUS_LIST_MAX: usize = 15;
    /// One go-live post per member per hour.
    pub const ANNOUNCE_COOLDOWN_MS: u64 = 60 * 60_000;
    /// `liveRoleAnnounceMessage` cap.
    pub const ANNOUNCE_MAX_CHARS: u16 = 1000;
}

pub const DEFAULT_LIVE_ANNOUNCE_MESSAGE: &str = "🔴 {user} is now live on {platform}!\n{url}";

/// Hosts a Streaming activity may link to in a go-live post (and their subdomains).
const STREAM_HOSTS: [&str; 3] = ["twitch.tv", "youtube.com", "youtu.be"];

/// `REASONS`: audit-log reasons.
pub mod reasons {
    pub const PRESENCE_ADD: &str = "Live role: started streaming";
    pub const PRESENCE_REMOVE: &str = "Live role: stopped streaming";
    pub const SYNC_ADD: &str = "Live role sync: member is streaming";
    pub const SYNC_REMOVE: &str = "Live role sync: member is not streaming or not eligible";
    pub const CLEAR: &str = "Live role turned off with /liverole disable";
}

/// The per-app state (the JS module-level mutexes, throttles and sets).
pub struct LiveRoleState {
    member_lock: KeyedMutex,
    sync_lock: KeyedMutex,
    warn: bot_util::async_util::Throttle,
    fetch: bot_util::async_util::Throttle,
    waiting: Mutex<HashSet<String>>,
    startup_queue: KeyedMutex,
    startup_queued: Mutex<HashSet<String>>,
    announce: bot_util::async_util::Throttle,
}

impl Default for LiveRoleState {
    fn default() -> Self {
        Self {
            member_lock: KeyedMutex::new(),
            sync_lock: KeyedMutex::new(),
            warn: bot_util::async_util::create_throttle(limits::WARN_INTERVAL_MS),
            fetch: bot_util::async_util::create_throttle(limits::FULL_FETCH_INTERVAL_MS),
            waiting: Mutex::new(HashSet::new()),
            startup_queue: KeyedMutex::new(),
            startup_queued: Mutex::new(HashSet::new()),
            announce: bot_util::async_util::create_throttle(limits::ANNOUNCE_COOLDOWN_MS),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn state(app: &App) -> Arc<LiveRoleState> {
    app.ext::<LiveRoleState>()
}

fn warn(app: &App, guild_id: GuildId, kind: &str, text: &str) {
    if state(app).warn.allow(&format!("{guild_id}:{kind}")) {
        bot_util::log_warn!(format!("[liverole] {guild_id}: {text}"));
    }
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

fn streaming_activity(presence: Option<&Presence>) -> Option<&serenity::all::Activity> {
    presence?.activities.iter().find(|a| a.kind == ActivityType::Streaming)
}

/// `isStreaming(presence)`: the presence has a Streaming activity.
pub fn is_streaming(presence: Option<&Presence>) -> bool {
    streaming_activity(presence).is_some()
}

/// `streamingChanged(old, new)`: a missing old presence counts as "not streaming".
pub fn streaming_changed(old: Option<&Presence>, new: Option<&Presence>) -> bool {
    is_streaming(old) != is_streaming(new)
}

/// `shouldHandlePresence(old, new, peeked)`: skip unless the streaming state changed, or when the memo proves the
/// feature is off or has no role. A missing memo never skips.
pub fn should_handle_presence(old: Option<&Presence>, new: Option<&Presence>, peeked: Option<&bot_db::GuildSettings>) -> bool {
    if !streaming_changed(old, new) {
        return false;
    }
    !peeked.is_some_and(|p| !p.live_role_enabled || p.live_role_id.as_deref().is_none_or(str::is_empty))
}

/// `streamLabel` of an activity name.
pub fn stream_label_of(name: &str) -> String {
    clamp_text(&escape_markdown(if name.is_empty() { "Streaming" } else { name }), 32)
}

/// `streamLabel(presence)`: the platform label of the Streaming activity ("Twitch", "YouTube").
pub fn stream_label(presence: Option<&Presence>) -> Option<String> {
    streaming_activity(presence).map(|a| stream_label_of(&a.name))
}

/// `safeStreamUrl(raw)`: an http(s) Twitch or YouTube URL (at most 512 characters), else `None`.
pub fn safe_stream_url(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    if bot_util::js::utf16_len(raw) > 512 {
        return None;
    }
    let u = url::Url::parse(raw).ok()?;
    if u.scheme() != "https" && u.scheme() != "http" {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    let host = u.host_str()?.to_lowercase();
    STREAM_HOSTS.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))).then(|| u.as_str().to_string())
}

/// The variables of a go-live post (`streamVars`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StreamVars {
    pub url: String,
    pub platform: String,
    pub title: String,
    pub game: String,
}

/// `streamVars` from the fields of a Streaming activity.
pub fn stream_vars_of(name: &str, url: Option<&str>, details: Option<&str>, state: Option<&str>) -> StreamVars {
    let text = |v: Option<&str>, max: usize| match v.map(bot_util::js::trim).filter(|t| !t.is_empty()) {
        Some(t) => clamp_text(&escape_markdown(t), max),
        None => String::new(),
    };
    StreamVars { url: safe_stream_url(url).unwrap_or_default(), platform: stream_label_of(name), title: text(details, 140), game: text(state, 100) }
}

/// `streamVars(presence)`: `None` when the presence is not streaming.
pub fn stream_vars(presence: Option<&Presence>) -> Option<StreamVars> {
    let a = streaming_activity(presence)?;
    Some(stream_vars_of(&a.name, a.url.as_ref().map(url::Url::as_str), a.details.as_deref(), a.state.as_deref()))
}

/// `meetsRequirement(...)`: whether a member satisfies `liveRoleRequiredRoleId` (fail closed when it was deleted).
pub fn meets_requirement(required_role_id: Option<&str>, required_role_exists: bool, member_roles: &[RoleId], guild_id: &str) -> bool {
    let Some(req) = required_role_id.filter(|r| !r.is_empty() && *r != guild_id) else { return true };
    if !required_role_exists {
        return false;
    }
    member_roles.iter().any(|r| r.to_string() == req)
}

/// A sync mode: `sync` follows the presences, `clear` removes the role from every holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Sync,
    Clear,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Clear => "clear",
        }
    }
}

/// A role change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add,
    Remove,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// `planMember({ mode, streaming, qualifies, holds })`.
pub fn plan_member(mode: Mode, streaming: bool, qualifies: bool, holds: bool) -> Option<Change> {
    if mode == Mode::Clear {
        return holds.then_some(Change::Remove);
    }
    let want = streaming && qualifies;
    match (want, holds) {
        (true, false) => Some(Change::Add),
        (false, true) => Some(Change::Remove),
        _ => None,
    }
}

/// `planSync`'s candidate lists (de-duplicated, input order kept).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub check: Vec<String>,
}

fn unique(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|id| seen.insert(id.clone())).collect()
}

/// `planSync({ mode, holderIds, streamerIds, unknownIds, qualifies })`.
pub fn plan_sync(mode: Mode, holder_ids: &[String], streamer_ids: &[String], unknown_ids: &[String], qualifies: impl Fn(&str) -> bool) -> SyncPlan {
    if mode == Mode::Clear {
        return SyncPlan { add: Vec::new(), remove: unique(holder_ids.iter().cloned()), check: Vec::new() };
    }
    let h: HashSet<&String> = holder_ids.iter().collect();
    let s: HashSet<&String> = streamer_ids.iter().collect();
    SyncPlan {
        add: unique(streamer_ids.iter().filter(|id| !h.contains(id) && qualifies(id)).cloned()),
        remove: unique(holder_ids.iter().filter(|id| !(s.contains(id) && qualifies(id))).cloned()),
        check: unique(unknown_ids.iter().filter(|id| !h.contains(id) && !s.contains(id)).cloned()),
    }
}

/// `classifyRoleError(err)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleErrorKind {
    Gone,
    RoleMissing,
    Perms,
    Error,
}

/// `classifyRoleError(err)` from the Discord error code.
pub fn classify_role_error(code: Option<u64>) -> RoleErrorKind {
    match code {
        Some(10007) => RoleErrorKind::Gone,
        Some(10011) => RoleErrorKind::RoleMissing,
        Some(50013) | Some(50001) => RoleErrorKind::Perms,
        _ => RoleErrorKind::Error,
    }
}

/// Why a sync did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailure {
    Settings,
    Off,
    NoRole,
    RoleMissing,
    CannotManage,
    PresenceUnavailable,
    Error,
}

impl SyncFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Off => "off",
            Self::NoRole => "no-role",
            Self::RoleMissing => "role-missing",
            Self::CannotManage => "cannot-manage",
            Self::PresenceUnavailable => "presence-unavailable",
            Self::Error => "error",
        }
    }
}

/// A finished sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDone {
    pub mode: Mode,
    pub added: usize,
    pub removed: usize,
    pub failed: usize,
    pub deferred: usize,
    /// Why nothing was added (the role has dangerous permissions).
    pub adds_blocked: Option<String>,
    /// Whether every member was checked (not only cached ones).
    pub complete: bool,
}

/// `syncGuild`'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncResult {
    Done(SyncDone),
    Failed { code: SyncFailure, reason: Option<String> },
}

impl SyncResult {
    fn fail(code: SyncFailure) -> Self {
        Self::Failed { code, reason: None }
    }
}

/// `formatSyncResult(r)`: text for command replies; `None` while the sync is still running.
pub fn format_sync_result(r: Option<&SyncResult>) -> String {
    let Some(r) = r else { return "⏳ Still syncing in the background. Check `/liverole status` in a minute.".into() };
    let d = match r {
        SyncResult::Failed { code, reason } => {
            return match code {
                SyncFailure::Settings => "⚠️ Could not sync right now (settings unavailable). It runs again on the next restart.".into(),
                SyncFailure::Off => "ℹ️ Not synced: the live role is off.".into(),
                SyncFailure::NoRole => "ℹ️ Not synced: no live role is set.".into(),
                SyncFailure::RoleMissing => "⚠️ Not synced: the live role was deleted.".into(),
                SyncFailure::CannotManage => format!("⚠️ Not synced: {}", reason.as_deref().unwrap_or("undefined")),
                SyncFailure::PresenceUnavailable => "⚠️ Not synced: this bot runs without presence data (`DISABLE_PRESENCES=1`).".into(),
                SyncFailure::Error => "⚠️ Sync failed. Check the bot logs.".into(),
            };
        }
        SyncResult::Done(d) => d,
    };
    let n = |x: usize| format_number(x as f64);
    let mut parts = Vec::new();
    if d.mode == Mode::Clear {
        parts.push(if d.removed > 0 { format!("Removed the live role from **{}** member(s).", n(d.removed)) } else { "Nobody had the live role.".into() });
    } else if d.added == 0 && d.removed == 0 {
        parts.push("Everyone is already in sync.".into());
    } else {
        parts.push(format!("Synced: gave it to **{}**, removed it from **{}**.", n(d.added), n(d.removed)));
    }
    if d.failed > 0 {
        parts.push(format!("⚠️ **{}** change(s) failed. Check that my role is above the live role and that I have **Manage Roles**.", n(d.failed)));
    }
    if d.deferred > 0 {
        let again = if d.mode == Mode::Clear { "/liverole disable" } else { "/liverole enable" };
        parts.push(format!(
            "ℹ️ **{}** more change(s) were left for later (at most {} per sync). Run `{again}` again to continue.",
            n(d.deferred),
            limits::SYNC_MAX_CHANGES
        ));
    }
    if let Some(b) = &d.adds_blocked {
        parts.push(format!("⚠️ Not giving the role to anyone: {b}"));
    }
    if !d.complete {
        parts.push("ℹ️ Only cached members were checked. Members who are offline may keep the role until they next stream.".into());
    }
    parts.join("\n")
}

/// What `liveRoleProblems` reads.
#[derive(Debug, Clone, Default)]
pub struct ProblemInput<'a> {
    pub enabled: bool,
    pub live_role_id: Option<&'a str>,
    pub role_exists: bool,
    pub manage: Option<&'a RoleCheck>,
    pub dangerous_names: Vec<String>,
    pub required_role_id: Option<&'a str>,
    pub required_role_exists: bool,
    pub holder_count: usize,
}

/// `liveRoleProblems(...)`: problem lines for `/liverole status`, in the spec's order.
pub fn live_role_problems(p: &ProblemInput<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let live = p.live_role_id.filter(|r| !r.is_empty());
    if p.enabled && live.is_none() {
        out.push("⚠️ The module is on but no live role is set. Use `/liverole set`.".into());
    }
    if live.is_some() && !p.role_exists {
        out.push("⚠️ The live role was deleted. Pick a new one with `/liverole set`.".into());
    }
    if p.role_exists
        && let Some(m) = p.manage.filter(|m| !m.ok)
    {
        out.push(format!("⚠️ {} I can't give or remove the live role until this is fixed.", m.reason.as_deref().unwrap_or("undefined")));
    }
    if p.role_exists && !p.dangerous_names.is_empty() {
        out.push(format!(
            "⚠️ The live role has dangerous permissions ({}). I won't give it to anyone until they are removed.",
            p.dangerous_names.join(", ")
        ));
    }
    if p.required_role_id.is_some_and(|r| !r.is_empty()) && !p.required_role_exists {
        out.push("⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.".into());
    }
    if !p.enabled && p.holder_count > 0 {
        out.push(format!(
            "ℹ️ **{}** member(s) still have the live role. `/liverole disable` removes it from them.",
            format_number(p.holder_count as f64)
        ));
    }
    out
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// `requiredRoleState(guild, settings)`: the required role "exists" when there is none, when it is @everyone, or when
/// the guild has it.
pub fn required_role_state(guild: &Guild, settings: &bot_db::GuildSettings) -> (Option<String>, bool) {
    let req = settings.live_role_required_role_id.clone().filter(|r| !r.is_empty());
    let exists = match &req {
        None => true,
        Some(r) => *r == guild.id.to_string() || snowflake(r).is_some_and(|v| guild.roles.contains_key(&RoleId::new(v))),
    };
    (req, exists)
}

/// discord.js `member.partial` (`joinedTimestamp === null`): serenity caches a member built from a presence update
/// (no roles, no `joined_at`) the way discord.js does, so its roles are unknown.
pub fn is_partial(member: &serenity::all::Member) -> bool {
    member.joined_at.is_none()
}

fn role_id_of(s: Option<&str>) -> Option<RoleId> {
    s.and_then(snowflake).map(RoleId::new)
}

/// What [`reconcile_member`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Added,
    Removed,
    None,
    Gone,
    Blocked,
    Failed,
    RoleMissing,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::None => "none",
            Self::Gone => "gone",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::RoleMissing => "role-missing",
        }
    }
}

/// Where a reconcile came from (it picks the audit reason; presence calls are coalesced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Presence,
    Sync,
}

async fn reconcile_locked(app: &App, guild_id: GuildId, user_id: UserId, mode: Mode, settings: &bot_db::GuildSettings, source: Source) -> Outcome {
    // 1. Role. A cache miss is never a deletion: report it, keep the setting.
    let role_id = role_id_of(settings.live_role_id.as_deref());
    let role = role_id.and_then(|r| ops::view(app, guild_id, |g| g.roles.get(&r).cloned()).flatten());
    let Some(role) = role else {
        let shown = settings.live_role_id.clone().unwrap_or_else(|| "null".into());
        warn(app, guild_id, "role-missing", &format!("live role {shown} not found; nothing is assigned until /liverole set picks a new one"));
        return Outcome::RoleMissing;
    };

    // A presence decision that waited in the lock behind /liverole disable or set must not act on the old setup.
    if source == Source::Presence
        && let Some(p) = app.settings().peek_settings(&guild_id.to_string())
        && (!p.live_role_enabled || p.live_role_id != settings.live_role_id)
    {
        return Outcome::None;
    }

    // 2. Member. A member built from a presence has no role data (partial), so fetch the real one.
    let o = ops::ops(app);
    let mut member = ops::view(app, guild_id, |g| g.members.get(&user_id).cloned()).flatten();
    if member.as_ref().is_none_or(is_partial) {
        member = o.fetch_member(app, guild_id, user_id).await;
    }
    let Some(member) = member else { return Outcome::Gone };
    if member.user.bot {
        return Outcome::None;
    }

    // 3. Current state, read fresh inside the lock rather than from the event.
    let holds = member.roles.contains(&role.id);
    let bot = o.bot_id(app);
    let read = ops::view(app, guild_id, |g| {
        let streaming = is_streaming(g.presences.get(&user_id));
        let (req, exists) = required_role_state(g, settings);
        (streaming, req, exists)
    });
    let Some((streaming, req, exists)) = read else { return Outcome::None };
    if !exists {
        warn(app, guild_id, "required-missing", &format!("required role {} was deleted; nobody qualifies (fix with /liverole require)", req.as_deref().unwrap_or("")));
    }
    let qualifies = meets_requirement(req.as_deref(), exists, &member.roles, &guild_id.to_string());

    // 4. Idempotent: add only when missing, remove only when present.
    let Some(action) = plan_member(mode, streaming, qualifies, holds) else { return Outcome::None };

    // 5. Safety check before every Discord call. Adds are refused when the role gained dangerous permissions.
    let check = match bot {
        Some(b) => ops::view(app, guild_id, |g| check_assignable_role(g, Some(&role), b, None, action == Change::Remove)),
        None => None,
    }
    .unwrap_or(RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() });
    if !check.ok {
        warn(app, guild_id, "blocked", &format!("cannot {} live role {}: {}", action.as_str(), role.id, check.reason.as_deref().unwrap_or("")));
        return Outcome::Blocked;
    }

    // 6. One role at a time, by id.
    let reason = match (mode, source, action) {
        (Mode::Clear, _, _) => reasons::CLEAR,
        (_, Source::Presence, Change::Add) => reasons::PRESENCE_ADD,
        (_, Source::Presence, Change::Remove) => reasons::PRESENCE_REMOVE,
        (_, Source::Sync, Change::Add) => reasons::SYNC_ADD,
        (_, Source::Sync, Change::Remove) => reasons::SYNC_REMOVE,
    };
    let result = match action {
        Change::Add => o.add_role(app, guild_id, user_id, role.id, reason.into()).await,
        Change::Remove => o.remove_role(app, guild_id, user_id, role.id, reason.into()).await,
    };
    match result {
        Ok(()) => {
            if action == Change::Add {
                Outcome::Added
            } else {
                Outcome::Removed
            }
        }
        // 7. Discord errors.
        Err(e) => match classify_role_error(e.code) {
            RoleErrorKind::Gone => {
                bot_util::log_debug!(format!("[liverole] {guild_id}: member {user_id} is gone (10007)"));
                Outcome::Gone
            }
            RoleErrorKind::RoleMissing => {
                warn(app, guild_id, "role-missing", &format!("live role {} no longer exists (10011); pick a new one with /liverole set", role.id));
                Outcome::RoleMissing
            }
            RoleErrorKind::Perms => {
                warn(
                    app,
                    guild_id,
                    "perms",
                    &format!(
                        "Missing Permissions while trying to {} live role {} (code {}); move my role above it and give me Manage Roles",
                        action.as_str(),
                        role.id,
                        e.code_text()
                    ),
                );
                Outcome::Failed
            }
            RoleErrorKind::Error => {
                warn(app, guild_id, "error", &format!("{} live role failed for {user_id}: {} {}", action.as_str(), e.code_text(), e.message));
                Outcome::Failed
            }
        },
    }
}

/// `reconcileMember(guild, userId, { mode, settings, source })`: brings one member's live role in line with their
/// current presence. Idempotent, serialised per member, never fails.
pub async fn reconcile_member(app: &App, guild_id: GuildId, user_id: UserId, mode: Mode, settings: Settings, source: Source) -> Outcome {
    // An unavailable guild has empty caches, so every role and member would look deleted.
    if ops::view(app, guild_id, |_| ()).is_none() {
        return Outcome::None;
    }
    // Without the Presence intent nobody looks like they stream: only the clear mode may act.
    if mode != Mode::Clear && !ops::presences_enabled(app) {
        return Outcome::None;
    }
    let st = state(app);
    let key = format!("{guild_id}:{user_id}");
    let coalesce = source == Source::Presence;
    if coalesce && !lock(&st.waiting).insert(key.clone()) {
        return Outcome::None; // a queued run reads the newest presence when it starts
    }
    let (app2, k2, st2) = (app.clone(), key.clone(), st.clone());
    st.member_lock
        .run(&key, move || async move {
            if coalesce {
                lock(&st2.waiting).remove(&k2);
            }
            reconcile_locked(&app2, guild_id, user_id, mode, &settings, source).await
        })
        .await
}

/// `countLosingHolders(guild, role, settings)`: non-bot members who hold the role but would lose it in a sync. Cache
/// only.
pub fn count_losing_holders(guild: &Guild, role_id: RoleId, settings: &bot_db::GuildSettings) -> usize {
    let (req, exists) = required_role_state(guild, settings);
    let g = guild.id.to_string();
    guild
        .members
        .values()
        .filter(|m| m.roles.contains(&role_id) && !m.user.bot)
        .filter(|m| {
            let qualifies = meets_requirement(req.as_deref(), exists, &m.roles, &g);
            plan_member(Mode::Sync, is_streaming(guild.presences.get(&m.user.id)), qualifies, true) == Some(Change::Remove)
        })
        .count()
}

/// `loadMembers(guild)`: makes the member cache complete so the role's holders are all known. Whether the cache is
/// believed complete; never fails.
pub async fn load_members(app: &App, guild_id: GuildId) -> bool {
    let Some((total, size)) = ops::view(app, guild_id, |g| (g.member_count, g.members.len() as u64)) else { return false };
    if size >= total {
        return true;
    }
    if total > limits::FULL_FETCH_MAX_MEMBERS {
        return false;
    }
    if !state(app).fetch.allow(&guild_id.to_string()) {
        return false;
    }
    match ops::ops(app).fetch_all_members(app, guild_id, limits::FULL_FETCH_TIMEOUT_MS + 5_000, limits::FULL_FETCH_MAX_MEMBERS).await {
        Ok(()) => true,
        Err(reason) => {
            bot_util::log_debug!(format!("[liverole] {guild_id}: full member fetch skipped ({reason})"));
            false
        }
    }
}

fn sorted_ids(mut ids: Vec<UserId>) -> Vec<String> {
    ids.sort();
    ids.into_iter().map(|i| i.to_string()).collect()
}

/// `syncGuild(guild, { mode, reason })`: whole-guild reconciliation; `clear` removes the role from every holder. Never
/// fails.
pub async fn sync_guild(app: &App, guild_id: GuildId, mode: Mode, reason: &str) -> SyncResult {
    // An unavailable guild has empty caches; nothing may be concluded from them.
    if ops::view(app, guild_id, |_| ()).is_none() {
        return SyncResult::fail(SyncFailure::Error);
    }
    // DISABLE_PRESENCES: the empty presence cache would make every holder look offline.
    if mode == Mode::Sync && !ops::presences_enabled(app) {
        return SyncResult::fail(SyncFailure::PresenceUnavailable);
    }
    let (app2, reason) = (app.clone(), reason.to_string());
    state(app).sync_lock.run(&guild_id.to_string(), move || async move { sync_locked(&app2, guild_id, mode, &reason).await }).await
}

async fn sync_locked(app: &App, guild_id: GuildId, mode: Mode, reason: &str) -> SyncResult {
    // Always the current values: a sync queued behind another one may run much later.
    let Some(settings) = app.settings().get_settings_safe(&guild_id.to_string()).await else { return SyncResult::fail(SyncFailure::Settings) };
    if mode == Mode::Sync && !settings.live_role_enabled {
        return SyncResult::fail(SyncFailure::Off);
    }
    let Some(role_id) = settings.live_role_id.as_deref().filter(|r| !r.is_empty()) else { return SyncResult::fail(SyncFailure::NoRole) };
    let Some(role_id) = role_id_of(Some(role_id)) else { return SyncResult::fail(SyncFailure::RoleMissing) };
    let bot = ops::ops(app).bot_id(app);
    let checks = ops::view(app, guild_id, |g| {
        let role = g.roles.get(&role_id)?;
        let check = |dangerous: bool| match bot {
            Some(b) => check_assignable_role(g, Some(role), b, None, dangerous),
            None => RoleCheck { ok: false, reason: Some("I need the Manage Roles permission.".into()), dangerous: Vec::new() },
        };
        Some((check(true), if mode == Mode::Sync { check(false) } else { RoleCheck { ok: true, reason: None, dangerous: Vec::new() } }))
    })
    .flatten();
    let Some((manage, add_check)) = checks else { return SyncResult::fail(SyncFailure::RoleMissing) };
    if !manage.ok {
        return SyncResult::Failed { code: SyncFailure::CannotManage, reason: manage.reason };
    }

    let complete = load_members(app, guild_id).await;

    let g = guild_id.to_string();
    let lists = ops::view(app, guild_id, |guild| {
        let holders =
            sorted_ids(guild.members.values().filter(|m| m.roles.contains(&role_id) && !m.user.bot && !is_partial(m)).map(|m| m.user.id).collect());
        let mut streamers = Vec::new();
        let mut unknown = Vec::new();
        for (id, p) in &guild.presences {
            if !is_streaming(Some(p)) {
                continue;
            }
            match guild.members.get(id) {
                Some(m) if m.user.bot => {}
                Some(m) if !is_partial(m) => streamers.push(*id),
                _ => unknown.push(*id),
            }
        }
        let (req, exists) = required_role_state(guild, &settings);
        let qualified: HashSet<String> = guild
            .members
            .values()
            .filter(|m| !is_partial(m) && meets_requirement(req.as_deref(), exists, &m.roles, &g))
            .map(|m| m.user.id.to_string())
            .collect();
        (holders, sorted_ids(streamers), sorted_ids(unknown), qualified)
    });
    let Some((holders, streamers, unknown, qualified)) = lists else { return SyncResult::fail(SyncFailure::Error) };

    let plan = plan_sync(mode, &holders, &streamers, &unknown, |id| qualified.contains(id));
    // Adds first, so streamers get their role even when the cap cuts the cleanup short.
    let queue: Vec<String> = if add_check.ok { plan.add.into_iter().chain(plan.check).chain(plan.remove).collect() } else { plan.remove };
    let work: Vec<String> = queue.iter().take(limits::SYNC_MAX_CHANGES).cloned().collect();
    let deferred = queue.len() - work.len();

    let (mut added, mut removed, mut failed) = (0, 0, 0);
    // One member at a time, never in parallel. Each call re-checks the member's live state inside the member lock.
    for id in work {
        let Some(uid) = snowflake(&id).map(UserId::new) else { continue };
        let out = reconcile_member(app, guild_id, uid, mode, settings.clone(), Source::Sync).await;
        match out {
            Outcome::Added => added += 1,
            Outcome::Removed => removed += 1,
            Outcome::Failed | Outcome::Blocked | Outcome::RoleMissing => failed += 1,
            _ => {}
        }
        if out == Outcome::RoleMissing {
            break; // the role vanished mid-sync
        }
    }

    if added + removed + failed + deferred > 0 {
        let failed_txt = if failed > 0 { format!(" ({failed} failed)") } else { String::new() };
        let deferred_txt = if deferred > 0 { format!(", {deferred} deferred") } else { String::new() };
        let cached = if complete { "" } else { " (cached members only)" };
        bot_util::log_info!(format!("[liverole] {guild_id} {reason}: +{added} -{removed}{failed_txt}{deferred_txt}{cached}"));
    }
    SyncResult::Done(SyncDone {
        mode,
        added,
        removed,
        failed,
        deferred,
        adds_blocked: if add_check.ok { None } else { add_check.reason },
        complete,
    })
}

/// What [`announce_live`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceOutcome {
    Sent,
    Off,
    Unusable,
    Cooldown,
    NotStreaming,
    Empty,
    Failed,
}

/// The render context of a go-live post.
pub fn announce_context(guild: &Guild, member: Option<&serenity::all::Member>, channel: Option<&serenity::all::GuildChannel>, vars: &StreamVars) -> RenderContext {
    RenderContext {
        member: member.map(|m| bot_core::adapters::placeholder_member(m, Some(guild))),
        user: member.map(|m| bot_core::adapters::placeholder_user(&m.user)),
        guild: Some(bot_core::adapters::placeholder_guild(guild)),
        channel: channel.map(bot_core::adapters::placeholder_channel),
        vars: Some(vec![
            ("url".into(), VarValue::from(vars.url.clone())),
            ("platform".into(), VarValue::from(vars.platform.clone())),
            ("title".into(), VarValue::from(vars.title.clone())),
            ("game".into(), VarValue::from(vars.game.clone())),
        ]),
        args: None,
    }
}

/// The bot's permissions in a channel of a cached guild.
fn bot_perms_in(guild: &Guild, channel: &serenity::all::GuildChannel, bot: Option<UserId>) -> Option<serenity::all::Permissions> {
    let b = bot?;
    let me = bot_core::permissions::me(guild, b)?;
    bot_core::permissions::member_permissions_in(guild, channel, b, &me.roles, true)
}

/// `announceLive(guild, userId, settings)`: posts "{user} is now live" in `liveRoleAnnounceChannelId`. Called only
/// after a presence-sourced add. Only `liveRoleAnnouncePingRoleId` can be pinged, and only when the bot may mention
/// it. Never fails.
pub async fn announce_live(app: &App, guild_id: GuildId, user_id: UserId, settings: &bot_db::GuildSettings) -> AnnounceOutcome {
    let Some(channel_id) = settings.live_role_announce_channel_id.clone().filter(|c| !c.is_empty()) else { return AnnounceOutcome::Off };
    let o = ops::ops(app);
    let bot = o.bot_id(app);
    let cid = snowflake(&channel_id).map(serenity::all::ChannelId::new);

    enum Prep {
        Unusable(String),
        NotStreaming,
        Ready { content: String, vars: StreamVars, embeds_ok: bool, ping: Option<(String, Vec<String>)> },
    }
    let st = state(app);
    let prepared = ops::view(app, guild_id, |g| {
        let channel = cid.and_then(|c| bot_core::permissions::find_channel(g, c));
        let perms = channel.and_then(|c| bot_perms_in(g, c, bot));
        let usable = channel_usable_verdict(channel, perms, &["ViewChannel", "SendMessages"]);
        if !usable.ok {
            return Prep::Unusable(usable.reason.unwrap_or_default());
        }
        let Some(vars) = stream_vars(g.presences.get(&user_id)) else { return Prep::NotStreaming };
        let member = g.members.get(&user_id);
        let template = settings.live_role_announce_message.as_deref().map(bot_util::js::trim).filter(|t| !t.is_empty()).unwrap_or(DEFAULT_LIVE_ANNOUNCE_MESSAGE);
        let content = render_placeholders(template, &announce_context(g, member, channel, &vars), Some(2000));
        let embeds_ok = channel_usable_verdict(channel, perms, &["EmbedLinks"]).ok;
        let ping = settings
            .live_role_announce_ping_role_id
            .clone()
            .filter(|p| !p.is_empty() && *p != g.id.to_string() && snowflake(p).is_some_and(|v| g.roles.contains_key(&RoleId::new(v))))
            .map(|p| {
                let me = bot.and_then(|b| bot_core::permissions::me(g, b).map(|m| (b, m.roles.clone())));
                let allowed = match channel {
                    Some(ch) => pingable_role_ids(g, ch, me.as_ref().map(|(b, r)| (*b, r.as_slice())), std::slice::from_ref(&p)).allowed,
                    None => Vec::new(),
                };
                (p, allowed)
            });
        Prep::Ready { content, vars, embeds_ok, ping }
    });
    let Some(prepared) = prepared else { return AnnounceOutcome::Failed };
    let (mut content, vars, embeds_ok, ping) = match prepared {
        Prep::Unusable(reason) => {
            warn(app, guild_id, "announce-channel", &format!("go-live post skipped: channel {channel_id} unusable ({reason})"));
            return AnnounceOutcome::Unusable;
        }
        Prep::NotStreaming => return AnnounceOutcome::NotStreaming, // stopped again before the post
        Prep::Ready { content, vars, embeds_ok, ping } => (content, vars, embeds_ok, ping),
    };
    if !st.announce.allow(&format!("{guild_id}:{user_id}")) {
        return AnnounceOutcome::Cooldown;
    }
    if bot_util::js::trim(&content).is_empty() {
        return AnnounceOutcome::Empty;
    }
    if !vars.url.is_empty() && !embeds_ok {
        warn(app, guild_id, "announce-embeds", &format!("go-live posts in {channel_id} show no stream preview: I lack Embed Links there"));
    }
    let mut roles = Vec::new();
    if let Some((ping_id, allowed)) = ping {
        content = clamp_text(&format!("<@&{ping_id}> {content}"), 2000);
        roles = allowed;
        if roles.is_empty() {
            warn(
                app,
                guild_id,
                "announce-ping",
                &format!("go-live posts cannot ping role {ping_id}: it is not mentionable and I lack Mention Everyone in {channel_id}"),
            );
        }
    }
    let Some(cid) = cid else { return AnnounceOutcome::Failed };
    match o.send(app, cid, content, roles).await {
        Ok(()) => AnnounceOutcome::Sent,
        Err(e) => {
            if matches!(e.code, Some(50013) | Some(50001)) {
                warn(app, guild_id, "announce-perms", &format!("go-live post failed: missing permission (code {})", e.code_text()));
            } else if e.code != Some(10003) {
                bot_util::log_warn!(format!("[liverole] {guild_id}: go-live post failed: {} {}", e.code_text(), e.message));
            }
            AnnounceOutcome::Failed
        }
    }
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

fn live_role_on(s: &bot_db::GuildSettings) -> bool {
    s.live_role_enabled && s.live_role_id.as_deref().is_some_and(|r| !r.is_empty())
}

async fn on_presence_update(ctx: Arc<HookCtx<PresenceUpdate>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) }; // memoised; the runner loaded it for gating
    if !live_role_on(&settings) {
        return Ok(());
    }
    let user_id = ctx.data.new_presence.user.id;
    let out = reconcile_member(&ctx.app, ctx.data.guild_id, user_id, Mode::Sync, settings.clone(), Source::Presence).await;
    if out == Outcome::Added {
        announce_live(&ctx.app, ctx.data.guild_id, user_id, &settings).await; // outside the member lock
    }
    Ok(())
}

/// Gaining or losing the required role changes who qualifies without any presence update: re-check that member.
async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let Some(required) = settings.live_role_required_role_id.as_deref().filter(|r| !r.is_empty()).and_then(snowflake).map(RoleId::new) else {
        return Ok(());
    };
    if !live_role_on(&settings) {
        return Ok(());
    }
    let had = ctx.data.old_member.as_ref().is_some_and(|m| m.roles.contains(&required));
    if had == ctx.data.member.roles.contains(&required) {
        return Ok(());
    }
    reconcile_member(&ctx.app, ctx.data.guild_id, ctx.data.member.user.id, Mode::Sync, settings.clone(), Source::Sync).await; // audit reason: "not eligible"
    Ok(())
}

async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    if !ops::presences_enabled(&ctx.app) {
        return Ok(());
    }
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !live_role_on(&settings) {
        return Ok(());
    }
    let guild_id = ctx.data.guild_id;
    let st = state(&ctx.app);
    if !lock(&st.startup_queued).insert(guild_id.to_string()) {
        return Ok(());
    }
    // 'available' behaves exactly like 'startup'; only a bot join is labelled differently.
    let reason = if ctx.data.reason == GuildReadyReason::Join { "join" } else { "startup" };
    // Detached, so a member fetch never holds up the later guildReady hooks or the next guild's startup.
    let (app, st2) = (ctx.app.clone(), st.clone());
    let queued = st.startup_queue.run("startup", move || async move {
        lock(&st2.startup_queued).remove(&guild_id.to_string());
        sync_guild(&app, guild_id, Mode::Sync, reason).await
    });
    tokio::spawn(queued);
    Ok(())
}

/// Tests only (`__startupSyncsIdle`): resolves once every startup sync queued so far has finished.
pub async fn startup_syncs_idle(app: &App) {
    state(app).startup_queue.run("startup", || async {}).await;
}

/// The plugin (`src/plugins/liverole.js`). No hook is `always`: while the module is off the bot never touches the
/// live role from events; `/liverole disable` is the cleanup path.
pub fn plugin() -> Plugin {
    Plugin::new("liverole", Some(ModuleKey::Liverole))
        .hook(
            Hook::<PresenceUpdate>::new("liverole", on_presence_update)
                .filter(|ctx| {
                    let peeked = ctx.app.settings().peek_settings(&ctx.data.guild_id.to_string());
                    should_handle_presence(ctx.data.old_presence.as_ref(), Some(&ctx.data.new_presence), peeked.as_deref())
                })
                .timeout_ms(30_000),
        )
        .hook(Hook::<GuildReady>::new("liverole", on_guild_ready)) // queues the sync and returns at once
        .hook(
            Hook::<GuildMemberUpdate>::new("liverole", on_member_update)
                // An uncached old member cannot be diffed.
                .filter(|ctx| ctx.data.old_known && !ctx.data.member.user.bot && ops::presences_enabled(&ctx.app))
                .timeout_ms(30_000),
        )
}
