//! Port of `src/services/afkService.js`. AFK: members mark themselves away (`/afk set`), whoever mentions them is
//! told, and the next message they write ends it. State is one `AfkStatus` row per (guild, member);
//! `GuildSettings.afkNicknamePrefix` and `afkIgnoredChannelIds` configure it. The message handling is
//! [`crate::plugins::afk`], `/afk` is [`crate::commands::afk`]. This file holds the rules: the per-guild set of AFK
//! member ids (so chat from everyone else never queries Postgres), the row writes, the `[AFK] ` nickname prefix and
//! the text clean-up for reasons (member text: never pings, one line, length-capped).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bot_core::error::BoxFuture;
use bot_core::prelude::*;
use bot_db::query::delete_where;
use bot_db::{AFK_STATUS_INFO, AfkStatus, InsertBuilder, PgPool, SelectBuilder};
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use bot_util::text::{clamp_text, clamp_text_with};
use futures::FutureExt;
use futures::future::Shared;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
use serenity::all::{Guild, GuildId, Member, UserId};

use crate::ops::{self, RestError};

/// `AFK`.
pub mod afk {
    pub const EMOJI: &str = "💤";
    pub const PREFIX: &str = "[AFK] ";
    pub const NICKNAME_MAX: usize = 32;
    /// A message this soon after `/afk set` does not end the AFK.
    pub const GRACE_MS: i64 = 10_000;
    /// The "Welcome back" reply deletes itself after this long.
    pub const WELCOME_DELETE_MS: u64 = 10_000;
    /// One "X is AFK" reply per channel and AFK member this often.
    pub const NOTICE_COOLDOWN_MS: u64 = 60_000;
    /// AFK members announced per message, and ping links shown on return.
    pub const MAX_NOTICES: usize = 5;
    pub const MAX_LINKS: usize = 5;
    /// `/afk list` shows this many members.
    pub const LIST_MAX: i64 = 25;
    /// How long the per-guild set of AFK ids is trusted; a failed load is retried after the short one.
    pub const IDS_TTL_MS: i64 = 120_000;
    pub const IDS_FAILED_TTL_MS: i64 = 15_000;
    pub const IDS_MEMO_MAX: usize = 5_000;
}

/// `GONE_CODES`: unknown channel, message or member, missing access or permission.
pub const GONE_CODES: [u64; 5] = [10003, 10007, 10008, 50001, 50013];

/// Whether a REST error is one of [`GONE_CODES`].
pub fn is_gone(err: &RestError) -> bool {
    err.code.is_some_and(|c| GONE_CODES.contains(&c))
}

type IdSet = Arc<HashSet<String>>;

/// The in-process state (one per app): the id memo, the loads in flight, the throttles and the ping lock.
pub struct AfkState {
    memo: Mutex<IndexMap<String, (IdSet, i64)>>,
    loads: Mutex<HashMap<String, Shared<BoxFuture<IdSet>>>>,
    writes: AtomicU64,
    notify: bot_util::async_util::Throttle,
    warn_load: bot_util::async_util::Throttle,
    ping_lock: bot_core::sync::KeyedMutex,
}

impl Default for AfkState {
    fn default() -> Self {
        Self {
            memo: Mutex::new(IndexMap::new()),
            loads: Mutex::new(HashMap::new()),
            writes: AtomicU64::new(0),
            notify: bot_util::async_util::create_throttle(afk::NOTICE_COOLDOWN_MS),
            warn_load: bot_util::async_util::create_throttle(60_000),
            ping_lock: bot_core::sync::KeyedMutex::new(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn state(app: &App) -> Arc<AfkState> {
    app.ext::<AfkState>()
}

/// `mayNotify(channelId, userId)`: whether a notice about the member may be posted in the channel now (at most once
/// a minute per pair). Consumes the slot.
pub fn may_notify(app: &App, channel_id: &str, user_id: &str) -> bool {
    state(app).notify.allow(&format!("{channel_id}:{user_id}"))
}

// ── Text ──────────────────────────────────────────────────────────────────────

static WS_RUNS: LazyRe = lazy_re!(r"\s+", "g");
static EVERYONE_HERE: LazyRe = lazy_re!(r"@(everyone|here)", "gi");
static MENTION_START: LazyRe = lazy_re!(r"<(?=[@#])", "g");

/// `cleanReason(raw)`: one line, trimmed, at most `LIMITS.AFK_REASON_LENGTH` characters; `None` when empty.
pub fn clean_reason(raw: Option<&str>) -> Option<String> {
    let s = WS_RUNS.replace_all(raw?, " ");
    let text = bot_util::js::trim(&s);
    (!text.is_empty()).then(|| clamp_text(text, bot_config::limits::AFK_REASON_LENGTH))
}

/// `plainText(raw, max)`: member text made safe to repeat (mention syntax broken with a zero-width space), cut to
/// `max` characters.
pub fn plain_text(raw: &str, max: usize) -> String {
    let s = WS_RUNS.replace_all(raw, " ");
    let text = clamp_text(bot_util::js::trim(&s), max);
    let text = EVERYONE_HERE.replace_all_with(&text, |m, t| format!("@\u{200B}{}", m.group(t, 1).unwrap_or_default()));
    MENTION_START.replace_all(&text, "<\u{200B}")
}

/// `plainText(raw)` with the default cap (`LIMITS.AFK_REASON_LENGTH`).
pub fn plain_text_default(raw: &str) -> String {
    plain_text(raw, bot_config::limits::AFK_REASON_LENGTH)
}

/// `nameWithoutPrefix(name)`: a display name without the `[AFK] ` prefix the bot added.
pub fn name_without_prefix(name: &str) -> &str {
    name.strip_prefix(afk::PREFIX).unwrap_or(name)
}

/// `afkLength(ms)`: how long a member was AFK, in at most two units (`2h 5m`).
pub fn afk_length(ms: i64) -> String {
    if ms < 1000 {
        return "0s".into(); // formatDuration reads 0 as "Permanent"
    }
    bot_util::parse_duration::format_duration(ms as f64).split(' ').take(2).collect::<Vec<_>>().join(" ")
}

fn ser_js_number<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    bot_core::command::js_number(*v).serialize(s)
}

/// One saved mention of an AFK member (`{ channelId, messageId, authorId, at }` in the `pings` Json column).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ping {
    pub channel_id: String,
    pub message_id: String,
    pub author_id: Option<String>,
    #[serde(serialize_with = "ser_js_number")]
    pub at: f64,
}

/// `readPings(pings)`: the entries of the Json column that have a channel and message id, oldest first.
pub fn read_pings(pings: &Value) -> Vec<Ping> {
    let Some(list) = pings.as_array() else { return Vec::new() };
    list.iter()
        .filter_map(|p| {
            let o = p.as_object()?;
            let channel_id = o.get("channelId")?.as_str()?.to_string();
            let message_id = o.get("messageId")?.as_str()?.to_string();
            let at = match o.get("at") {
                Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                Some(Value::String(s)) => bot_util::js::trim(s).parse::<f64>().unwrap_or(0.0),
                Some(Value::Bool(true)) => 1.0,
                _ => 0.0,
            };
            Some(Ping { channel_id, message_id, author_id: o.get("authorId").and_then(Value::as_str).map(str::to_string), at: if at.is_nan() { 0.0 } else { at } })
        })
        .collect()
}

/// `jumpLink(guildId, channelId, messageId)`.
pub fn jump_link(guild_id: &str, channel_id: &str, message_id: &str) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}")
}

/// The ids `isIgnoredChannel` reads: the channel, its parent (a thread's channel or a channel's category) and that
/// parent's category.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelPlace {
    pub id: String,
    pub parent_id: Option<String>,
    pub grandparent_id: Option<String>,
}

impl ChannelPlace {
    /// From a cached guild.
    pub fn of(guild: &Guild, channel_id: serenity::all::ChannelId) -> Self {
        let ch = bot_core::permissions::find_channel(guild, channel_id);
        let parent = ch.and_then(|c| c.parent_id);
        let grand = parent.and_then(|p| bot_core::permissions::find_channel(guild, p)).and_then(|p| p.parent_id);
        Self { id: channel_id.to_string(), parent_id: parent.map(|p| p.to_string()), grandparent_id: grand.map(|g| g.to_string()) }
    }
}

/// `isIgnoredChannel(settings, channel)`: the channel, its parent or that parent's category is on
/// `GuildSettings.afkIgnoredChannelIds`.
pub fn is_ignored_channel(settings: Option<&bot_db::GuildSettings>, channel: Option<&ChannelPlace>) -> bool {
    let (Some(s), Some(c)) = (settings, channel) else { return false };
    let list = &s.afk_ignored_channel_ids;
    if list.is_empty() {
        return false;
    }
    [Some(&c.id), c.parent_id.as_ref(), c.grandparent_id.as_ref()].into_iter().flatten().any(|id| list.contains(id))
}

// ── Nickname prefix ───────────────────────────────────────────────────────────

/// `prefixedNickname(displayName)`: `[AFK] ` in front, cut to Discord's 32 characters; `None` when it already has it.
pub fn prefixed_nickname(display_name: &str) -> Option<String> {
    if display_name.starts_with(afk::PREFIX) {
        return None;
    }
    Some(format!("{}{}", afk::PREFIX, clamp_text_with(display_name, afk::NICKNAME_MAX - afk::PREFIX.len(), "")))
}

/// `restoredNickname(nickname, previousNick)`: the nickname to set when the prefix comes off (`Some(None)` clears
/// it); `None` (the JS `undefined`) when the nickname no longer starts with the prefix.
pub fn restored_nickname(nickname: Option<&str>, previous_nick: Option<&str>) -> Option<Option<String>> {
    if !nickname.is_some_and(|n| n.starts_with(afk::PREFIX)) {
        return None;
    }
    Some(previous_nick.filter(|p| !p.is_empty()).map(str::to_string))
}

/// discord.js `member.manageable`: not the owner, not the bot itself, and the bot's highest role is above theirs.
pub fn manageable(guild: &Guild, bot_id: UserId, member: &Member) -> bool {
    if member.user.id == guild.owner_id || member.user.id == bot_id {
        return false;
    }
    if bot_id == guild.owner_id {
        return true;
    }
    let Some(me) = bot_core::permissions::me(guild, bot_id) else { return false };
    bot_core::permissions::highest_position(guild, &me.roles) > bot_core::permissions::highest_position(guild, &member.roles)
}

/// `canRename(member)`: Manage Nicknames, a role above theirs, not the owner.
pub fn can_rename(app: &App, guild_id: GuildId, member: &Member) -> bool {
    let o = ops::ops(app);
    let Some(bot) = o.bot_id(app) else { return false };
    ops::view(app, guild_id, |g| {
        let perms = bot_core::permissions::me(g, bot).map(|me| bot_core::permissions::guild_permissions(g, bot, &me.roles));
        bot_core::interaction_helpers::missing_from(perms, &["ManageNicknames"]).is_empty() && manageable(g, bot, member)
    })
    .unwrap_or(false)
}

/// Why the prefix could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NickProblem {
    Cannot,
    Failed,
}

/// `addNicknamePrefix`'s result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NickChange {
    /// The bot set the nickname (so it must take it off again).
    pub changed: bool,
    pub problem: Option<NickProblem>,
    /// The nickname before the prefix (`None` when there was none).
    pub previous_nick: Option<String>,
}

/// `addNicknamePrefix(member, settings)`: puts `[AFK] ` in front of the member's name when the setting is on and the
/// bot can.
pub async fn add_nickname_prefix(app: &App, guild_id: GuildId, member: Option<&Member>, settings: Option<&bot_db::GuildSettings>) -> NickChange {
    let (Some(member), Some(s)) = (member, settings) else { return NickChange::default() };
    if !s.afk_nickname_prefix {
        return NickChange::default();
    }
    let Some(next) = prefixed_nickname(&bot_core::adapters::member_display_name(member)) else { return NickChange::default() };
    if !can_rename(app, guild_id, member) {
        return NickChange { changed: false, problem: Some(NickProblem::Cannot), previous_nick: None };
    }
    let previous = member.nick.clone().filter(|n| !n.is_empty());
    match ops::ops(app).set_nickname(app, guild_id, member.user.id, Some(next), "Went AFK".into()).await {
        Ok(()) => NickChange { changed: true, problem: None, previous_nick: previous },
        Err(e) => {
            if !is_gone(&e) {
                bot_util::log_warn!(format!("[afk] {guild_id}: could not add the prefix: {}", e.message));
            }
            NickChange { changed: false, problem: Some(NickProblem::Failed), previous_nick: None }
        }
    }
}

/// `removeNicknamePrefix(member, previousNick)`: gives back the nickname the member had, only while the nickname still
/// starts with the prefix. Never fails; whether the nickname was changed.
pub async fn remove_nickname_prefix(app: &App, guild_id: GuildId, member: Option<&Member>, previous_nick: Option<&str>) -> bool {
    let Some(member) = member else { return false };
    let Some(next) = restored_nickname(member.nick.as_deref(), previous_nick) else { return false };
    if !can_rename(app, guild_id, member) {
        return false;
    }
    match ops::ops(app).set_nickname(app, guild_id, member.user.id, next, "No longer AFK".into()).await {
        Ok(()) => true,
        Err(e) => {
            if !is_gone(&e) {
                bot_util::log_warn!(format!("[afk] {guild_id}: could not remove the prefix: {}", e.message));
            }
            false
        }
    }
}

// ── The per-guild set of AFK ids ──────────────────────────────────────────────

fn memo_put(app: &App, guild_id: &str, ids: IdSet, ttl_ms: i64) {
    let expires = app.clock().now_ms() + ttl_ms;
    let st = state(app);
    let mut memo = lock(&st.memo);
    memo.shift_remove(guild_id);
    memo.insert(guild_id.to_string(), (ids, expires));
    while memo.len() > afk::IDS_MEMO_MAX {
        memo.shift_remove_index(0);
    }
}

/// `peekAfkIds(guildId)`: the memoised set of AFK member ids of a guild, no I/O (`None` when it is not known).
pub fn peek_afk_ids(app: &App, guild_id: &str) -> Option<IdSet> {
    let now = app.clock().now_ms();
    let st = state(app);
    let mut memo = lock(&st.memo);
    let (ids, expires) = memo.get(guild_id)?.clone();
    if expires <= now {
        memo.shift_remove(guild_id);
        return None;
    }
    Some(ids)
}

/// The query behind [`load_afk_ids`].
pub const AFK_IDS_SQL: &str = r#"SELECT "userId" FROM "AfkStatus" WHERE "guildId" = $1"#;

async fn query_ids(pool: &PgPool, guild_id: &str) -> anyhow::Result<HashSet<String>> {
    let rows: Vec<(String,)> = bot_db::sqlx::query_as(AFK_IDS_SQL).bind(guild_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

/// `loadAfkIds(guildId)`: the memo, else one query (shared by concurrent callers). A failed query reads as "nobody is
/// AFK" for a few seconds.
pub async fn load_afk_ids(app: &App, guild_id: &str) -> IdSet {
    if let Some(hit) = peek_afk_ids(app, guild_id) {
        return hit;
    }
    let st = state(app);
    let fut = {
        let mut loads = lock(&st.loads);
        match loads.get(guild_id) {
            Some(f) => f.clone(),
            None => {
                let started_at = st.writes.load(Ordering::SeqCst);
                let (app2, g) = (app.clone(), guild_id.to_string());
                let f: BoxFuture<IdSet> = Box::pin(async move {
                    let st = state(&app2);
                    let ids = match query_ids(app2.db(), &g).await {
                        Ok(ids) => {
                            let ids = Arc::new(ids);
                            if st.writes.load(Ordering::SeqCst) == started_at {
                                memo_put(&app2, &g, ids.clone(), afk::IDS_TTL_MS);
                            }
                            ids
                        }
                        Err(e) => {
                            if st.warn_load.allow(&g) {
                                bot_util::log_warn!(format!("[afk] {g}: could not load the AFK members: {e}"));
                            }
                            let ids: IdSet = Arc::new(HashSet::new());
                            memo_put(&app2, &g, ids.clone(), afk::IDS_FAILED_TTL_MS);
                            ids
                        }
                    };
                    lock(&st.loads).remove(&g);
                    ids
                });
                let shared = f.shared();
                loads.insert(guild_id.to_string(), shared.clone());
                shared
            }
        }
    };
    fut.await
}

/// `noteAfk`: keeps the memoised set in step with a write made here.
fn note_afk(app: &App, guild_id: &str, user_id: &str, is_afk: bool) {
    let st = state(app);
    st.writes.fetch_add(1, Ordering::SeqCst);
    let Some(ids) = peek_afk_ids(app, guild_id) else { return };
    let mut next: HashSet<String> = (*ids).clone();
    if is_afk {
        next.insert(user_id.to_string());
    } else {
        next.remove(user_id);
    }
    let mut memo = lock(&st.memo);
    if let Some(entry) = memo.get_mut(guild_id) {
        entry.0 = Arc::new(next);
    }
}

/// `forgetAfkMember(guildId, userId)`: removes a member from the memoised set (the memo said AFK but the row is gone).
pub fn forget_afk_member(app: &App, guild_id: &str, user_id: &str) {
    note_afk(app, guild_id, user_id, false);
}

/// `forgetAfkIds(guildId)`: drops a guild's memo.
pub fn forget_afk_ids(app: &App, guild_id: &str) {
    lock(&state(app).memo).shift_remove(guild_id);
}

// ── Rows ──────────────────────────────────────────────────────────────────────

/// `loadStatus(guildId, userId)`: the member's `AfkStatus` row.
pub async fn load_status(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<AfkStatus>> {
    let q = SelectBuilder::new(&AFK_STATUS_INFO).filter("guildId", json!(guild_id))?.filter("userId", json!(user_id))?.build()?;
    Ok(q.fetch_optional::<AfkStatus, _>(pool).await?)
}

/// The query behind [`load_statuses`] (`userId: { in }`).
pub const AFK_STATUSES_SQL: &str = r#"SELECT * FROM "AfkStatus" WHERE "guildId" = $1 AND "userId" = ANY($2)"#;

/// `loadStatuses(guildId, userIds)`: the rows of these members, in the order of `user_ids`.
pub async fn load_statuses(pool: &PgPool, guild_id: &str, user_ids: &[String]) -> anyhow::Result<Vec<AfkStatus>> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<AfkStatus> = bot_db::sqlx::query_as(AFK_STATUSES_SQL).bind(guild_id).bind(user_ids.to_vec()).fetch_all(pool).await?;
    let mut by_id: HashMap<String, AfkStatus> = rows.into_iter().map(|r| (r.user_id.clone(), r)).collect();
    Ok(user_ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// `setAfk({ guildId, userId, reason, nickChanged, previousNick })`: marks a member AFK, or changes the reason of one
/// who already is (the start time and saved pings stay). `previous_nick` is saved only along with a new prefix.
pub async fn set_afk(app: &App, guild_id: &str, user_id: &str, reason: Option<&str>, nick_changed: bool, previous_nick: Option<&str>) -> anyhow::Result<AfkStatus> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let update: Vec<(&str, Value)> = if nick_changed {
        vec![("reason", json!(reason)), ("nickChanged", json!(true)), ("previousNick", json!(previous_nick))]
    } else {
        vec![("reason", json!(reason))]
    };
    let q = InsertBuilder::new(&AFK_STATUS_INFO)
        .values([
            ("guildId", json!(guild_id)),
            ("userId", json!(user_id)),
            ("reason", json!(reason)),
            ("nickChanged", json!(nick_changed)),
            ("previousNick", if nick_changed { json!(previous_nick) } else { Value::Null }),
        ])?
        .on_conflict_update(&["guildId", "userId"], update)?
        .build()?;
    let row = q.fetch_one::<AfkStatus, _>(app.db()).await?;
    note_afk(app, guild_id, user_id, true);
    Ok(row)
}

/// `clearAfk(guildId, userId)`: ends a member's AFK. The row that was deleted, or `None` when there was none: of two
/// callers racing only one gets the row.
pub async fn clear_afk(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<Option<AfkStatus>> {
    let Some(row) = load_status(app.db(), guild_id, user_id).await? else {
        note_afk(app, guild_id, user_id, false);
        return Ok(None);
    };
    let count = delete_where(&AFK_STATUS_INFO, [("guildId", json!(guild_id)), ("userId", json!(user_id))])?.execute(app.db()).await?;
    note_afk(app, guild_id, user_id, false);
    Ok((count > 0).then_some(row))
}

/// The statement behind [`record_ping`] (`pings`, `pingCount: { increment: 1 }`, clamped to the Int column).
pub const RECORD_PING_SQL: &str = r#"UPDATE "AfkStatus" SET "pings" = $3, "pingCount" = LEAST("pingCount"::bigint + 1, 2147483647)::int WHERE "guildId" = $1 AND "userId" = $2"#;

/// `recordPing(guildId, userId, ping)`: records a mention of an AFK member, keeping the last `LIMITS.AFK_PINGS_KEPT`
/// and counting all. Serialised per member; never fails. The future is `'static` (its place in the lock is taken now).
pub fn record_ping(app: &App, guild_id: &str, user_id: &str, channel_id: &str, message_id: &str, author_id: &str) -> impl std::future::Future<Output = ()> + Send + 'static {
    let (app2, g, u) = (app.clone(), guild_id.to_string(), user_id.to_string());
    let ping = Ping { channel_id: channel_id.into(), message_id: message_id.into(), author_id: Some(author_id.into()), at: 0.0 };
    state(app).ping_lock.run(&format!("{guild_id}:{user_id}"), move || async move {
        let result: anyhow::Result<()> = async {
            let Some(row) = load_status(app2.db(), &g, &u).await? else { return Ok(()) };
            let mut kept = read_pings(&row.pings);
            kept.push(Ping { at: app2.clock().now_ms() as f64, ..ping });
            let skip = kept.len().saturating_sub(bot_config::limits::AFK_PINGS_KEPT);
            let kept: Vec<Ping> = kept.into_iter().skip(skip).collect();
            bot_db::sqlx::query(RECORD_PING_SQL).bind(&g).bind(&u).bind(serde_json::to_value(&kept)?).execute(app2.db()).await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            bot_util::log_warn!(format!("[afk] {g}: could not record a ping: {e}"));
        }
    })
}

/// `listStatuses(guildId)`: the longest-AFK members first (at most `AFK.LIST_MAX`), and the total.
pub async fn list_statuses(pool: &PgPool, guild_id: &str) -> anyhow::Result<(Vec<AfkStatus>, i64)> {
    let q = SelectBuilder::new(&AFK_STATUS_INFO).filter("guildId", json!(guild_id))?.order_by("since", true)?.limit(afk::LIST_MAX).build()?;
    let rows = q.fetch_all::<AfkStatus, _>(pool).await?;
    let (total,): (i64,) = bot_db::sqlx::query_as(AFK_COUNT_SQL).bind(guild_id).fetch_one(pool).await?;
    Ok((rows, total))
}

/// The count behind [`list_statuses`].
pub const AFK_COUNT_SQL: &str = r#"SELECT COUNT(*)::bigint FROM "AfkStatus" WHERE "guildId" = $1"#;

/// Every raw statement of this module (run against the schema by the database tests).
pub const RAW_SQL: [(&str, &str); 4] =
    [("afkIds", AFK_IDS_SQL), ("loadStatuses", AFK_STATUSES_SQL), ("recordPing", RECORD_PING_SQL), ("count", AFK_COUNT_SQL)];
