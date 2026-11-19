//! Port of `src/services/stickyService.js`: sticky messages, a message kept at the bottom of a channel.
//!
//! When members talk in a sticky channel the bot waits for the channel to go quiet, deletes its previous sticky message
//! and posts it again. One `StickyMessage` row per channel (text and/or a saved embed's embed), one pending timer and one
//! re-send at a time per channel. Hook handling and the modal: [`crate::plugins::sticky`]; the command:
//! [`crate::commands::sticky`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bot_core::App;
use bot_core::interaction_helpers::{PermTarget, cached_channel, check_channel_usable, missing_bot_permissions, with_cached_guild};
use bot_core::prelude::*;
use bot_db::{GuildSettings, InsertBuilder, PgPool, STICKY_MESSAGE_INFO, SelectBuilder, StickyMessage};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, MessageId};
use tokio::task::AbortHandle;
use tokio::time::Instant;

/// `STICKY`: the timings and memo sizes.
pub mod timing {
    /// A re-send waits until the channel has been quiet this long...
    pub const QUIET_MS: u64 = 3_000;
    /// ...but never longer than this after the first message that asked for it, so a busy channel still gets it.
    pub const MAX_WAIT_MS: u64 = 15_000;
    /// How long a guild's list of sticky channels is remembered (it is dropped at once by every change made here).
    pub const MEMO_TTL_MS: i64 = 60_000;
    pub const MEMO_MAX_GUILDS: usize = 2_000;
    /// A message the bot deleted itself is remembered this long, so its messageDelete event costs no query.
    pub const SELF_DELETE_TTL_MS: i64 = 60_000;
    /// A channel the bot cannot post in is complained about in the log at most this often.
    pub const PERMS_WARN_MS: u64 = 60 * 60_000;
}

/// Unknown channel, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES: [u64; 3] = [10003, 50001, 50013];
const UNKNOWN_MESSAGE: u64 = 10008;

/// `StickyError`: an admin-facing refusal; `title` and the message are shown as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickyError {
    pub code: &'static str,
    pub title: String,
    pub message: String,
}

impl StickyError {
    pub fn new(code: &'static str, title: &str, message: impl Into<String>) -> Self {
        Self { code, title: title.to_string(), message: message.into() }
    }
}

impl std::fmt::Display for StickyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StickyError {}

// ── Re-send scheduler ─────────────────────────────────────────────────────────

/// What the scheduler runs for a channel.
pub type RunFn<A> = Arc<dyn Fn(String, A) -> bot_core::error::BoxFuture<anyhow::Result<()>> + Send + Sync>;

struct SchedState<A> {
    first_at: Instant,
    timer: Option<AbortHandle>,
    running: bool,
    dirty: bool,
    arg: A,
}

struct SchedInner<A> {
    quiet: Duration,
    max_wait: Duration,
    run: RunFn<A>,
    states: Mutex<HashMap<String, Arc<Mutex<SchedState<A>>>>>,
}

/// `createScheduler({ quietMs, maxWaitMs, run })`: debounces re-sends per channel. `touch` (a message arrived) arms one
/// timer that fires `quiet` after the last touch, but at most `max_wait` after the first one. While `run` is in flight
/// another touch only marks the channel dirty; the next window opens when `run` finishes. A failing `run` is logged.
pub struct Debouncer<A: Clone + Send + 'static> {
    inner: Arc<SchedInner<A>>,
}

impl<A: Clone + Send + 'static> Clone for Debouncer<A> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<A: Clone + Send + 'static> Debouncer<A> {
    pub fn new(quiet_ms: u64, max_wait_ms: u64, run: RunFn<A>) -> Self {
        Self {
            inner: Arc::new(SchedInner {
                quiet: Duration::from_millis(quiet_ms),
                max_wait: Duration::from_millis(max_wait_ms),
                run,
                states: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn arm(inner: &Arc<SchedInner<A>>, id: &str, st_arc: &Arc<Mutex<SchedState<A>>>, st: &mut SchedState<A>) {
        if let Some(t) = st.timer.take() {
            t.abort();
        }
        let deadline = st.first_at + inner.max_wait;
        let delay = inner.quiet.min(deadline.saturating_duration_since(Instant::now()));
        let (inner2, id2, st2) = (inner.clone(), id.to_string(), st_arc.clone());
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            Self::fire(inner2, id2, st2).await;
        });
        st.timer = Some(task.abort_handle());
    }

    async fn fire(inner: Arc<SchedInner<A>>, id: String, st_arc: Arc<Mutex<SchedState<A>>>) {
        let arg = {
            let Ok(mut st) = st_arc.lock() else { return };
            st.timer = None;
            st.running = true;
            st.arg.clone()
        };
        // Detached from the timer, so a later touch (which aborts the timer) never cancels a run in flight.
        let run = (inner.run)(id.clone(), arg);
        let result = tokio::spawn(run).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bot_util::log_warn!(format!("[sticky] re-send in {id} failed: {e}")),
            Err(e) => bot_util::log_warn!(format!("[sticky] re-send in {id} failed: {e}")),
        }
        let Ok(mut states) = inner.states.lock() else { return };
        let Ok(mut st) = st_arc.lock() else { return };
        st.running = false;
        // Cancelled meanwhile.
        if !states.get(&id).is_some_and(|s| Arc::ptr_eq(s, &st_arc)) {
            return;
        }
        if st.dirty {
            st.dirty = false;
            st.first_at = Instant::now();
            Self::arm(&inner, &id, &st_arc, &mut st);
        } else {
            states.remove(&id);
        }
    }

    /// A message arrived in the channel. `arg` is handed to `run` (the latest one wins).
    pub fn touch(&self, id: &str, arg: A) {
        let Ok(mut states) = self.inner.states.lock() else { return };
        let st_arc = states
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(SchedState { first_at: Instant::now(), timer: None, running: false, dirty: false, arg: arg.clone() })))
            .clone();
        // The same lock order as the end of a run (states, then the channel), so a touch never arms a removed state.
        let Ok(mut st) = st_arc.lock() else { return };
        st.arg = arg;
        if st.running {
            st.dirty = true;
        } else {
            Self::arm(&self.inner, id, &st_arc, &mut st);
        }
    }

    /// Drops the pending timer (an in-flight run still finishes). Whether there was anything.
    pub fn cancel(&self, id: &str) -> bool {
        let Ok(mut states) = self.inner.states.lock() else { return false };
        let Some(st) = states.remove(id) else { return false };
        if let Ok(mut s) = st.lock()
            && let Some(t) = s.timer.take()
        {
            t.abort();
        }
        true
    }

    pub fn has(&self, id: &str) -> bool {
        self.inner.states.lock().map(|s| s.contains_key(id)).unwrap_or(false)
    }

    pub fn size(&self) -> usize {
        self.inner.states.lock().map(|s| s.len()).unwrap_or(0)
    }

    pub fn clear(&self) {
        let Ok(mut states) = self.inner.states.lock() else { return };
        for st in states.values() {
            if let Ok(mut s) = st.lock()
                && let Some(t) = s.timer.take()
            {
                t.abort();
            }
        }
        states.clear();
    }
}

/// The scheduler's argument: the app and the guild (`{ client, guildId }`).
#[derive(Clone)]
pub struct ResendArg {
    pub app: App,
    pub guild_id: String,
}

/// The per-app sticky state (the JS module-level scheduler, locks, memo and marks).
pub struct StickyState {
    scheduler: Debouncer<ResendArg>,
    lock: bot_core::sync::KeyedMutex,
    guild_lock: bot_core::sync::KeyedMutex,
    memo: Mutex<Memo>,
    self_deleted: Mutex<HashMap<String, i64>>,
    perms_warn: OnceLock<bot_util::async_util::Throttle>,
}

type ChannelMap = Arc<HashMap<String, bool>>;
type LoadCell = Arc<tokio::sync::OnceCell<Option<ChannelMap>>>;

#[derive(Default)]
struct Memo {
    /// guildId -> (at, insertion order, channelId -> enabled)
    entries: HashMap<String, (i64, u64, ChannelMap)>,
    order: u64,
    loading: HashMap<String, (LoadCell, u64)>,
    invalidated_at: HashMap<String, u64>,
    invalidated_order: Vec<String>,
    sequence: u64,
}

impl Default for StickyState {
    fn default() -> Self {
        let run: RunFn<ResendArg> = Arc::new(|channel_id: String, arg: ResendArg| {
            Box::pin(async move {
                let ResendArg { app, guild_id } = arg;
                let (a, g, c) = (app.clone(), guild_id.clone(), channel_id.clone());
                run_exclusive(&app, &channel_id, move || async move { resend_sticky(&a, &g, &c, false).await }).await?;
                Ok(())
            })
        });
        Self {
            scheduler: Debouncer::new(timing::QUIET_MS, timing::MAX_WAIT_MS, run),
            lock: Default::default(),
            guild_lock: Default::default(),
            memo: Mutex::new(Memo::default()),
            self_deleted: Mutex::new(HashMap::new()),
            perms_warn: OnceLock::new(),
        }
    }
}

fn state(app: &App) -> Arc<StickyState> {
    app.ext::<StickyState>()
}

/// `runExclusive(channelId, fn)`: runs `f` while holding the channel's sticky lock (one re-send or admin change at a
/// time per channel). The job is queued when this is called.
pub fn run_exclusive<T, F, Fut>(app: &App, channel_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    state(app).lock.run(channel_id, f)
}

/// `noteMessage(client, guildId, channelId)`: a message arrived in a sticky channel: schedule its re-send.
pub fn note_message(app: &App, guild_id: &str, channel_id: &str) {
    state(app).scheduler.touch(channel_id, ResendArg { app: app.clone(), guild_id: guild_id.to_string() });
}

/// `cancelPending(channelId)`: drops the channel's pending re-send.
pub fn cancel_pending(app: &App, channel_id: &str) -> bool {
    state(app).scheduler.cancel(channel_id)
}

/// Whether a re-send is pending or running for the channel (`_scheduler.has`).
pub fn has_pending(app: &App, channel_id: &str) -> bool {
    state(app).scheduler.has(channel_id)
}

// ── Sticky channel cache (in memory, TTL) ─────────────────────────────────────

/// `peekChannels(guildId)`: the guild's sticky channels from the memo (channelId -> enabled), without any I/O. `None`
/// when unknown or expired.
pub fn peek_channels(app: &App, guild_id: &str) -> Option<Arc<HashMap<String, bool>>> {
    let now = app.clock().now_ms();
    let st = state(app);
    let mut m = st.memo.lock().ok()?;
    let (at, _, channels) = m.entries.get(guild_id)?.clone();
    if now - at >= timing::MEMO_TTL_MS {
        m.entries.remove(guild_id);
        return None;
    }
    Some(channels)
}

fn remember(m: &mut Memo, guild_id: &str, channels: Arc<HashMap<String, bool>>, now: i64) {
    m.order += 1;
    let order = m.order;
    m.entries.insert(guild_id.to_string(), (now, order, channels));
    if m.entries.len() > timing::MEMO_MAX_GUILDS {
        m.entries.retain(|_, (at, _, _)| now - *at < timing::MEMO_TTL_MS);
        while m.entries.len() > timing::MEMO_MAX_GUILDS {
            let Some(oldest) = m.entries.iter().min_by_key(|(_, (_, o, _))| *o).map(|(k, _)| k.clone()) else { break };
            m.entries.remove(&oldest);
        }
    }
}

/// The guild's sticky rows as `channelId -> enabled`.
pub async fn load_channels(pool: &PgPool, guild_id: &str) -> anyhow::Result<HashMap<String, bool>> {
    let rows: Vec<(String, bool)> = bot_db::sqlx::query_as(sql("channels"))
        .bind(guild_id)
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::Error::from(bot_db::DbError::from(e)))?;
    Ok(rows.into_iter().collect())
}

/// `getChannels(guildId)`: the guild's sticky channels: the memo, else one query (shared by concurrent callers). An empty
/// map is remembered too. `None` when the database failed (not remembered).
pub async fn get_channels(app: &App, guild_id: &str) -> Option<Arc<HashMap<String, bool>>> {
    if let Some(hit) = peek_channels(app, guild_id) {
        return Some(hit);
    }
    let st = state(app);
    let (cell, started_at) = {
        let mut m = st.memo.lock().ok()?;
        let seq = m.sequence;
        m.loading.entry(guild_id.to_string()).or_insert_with(|| (Arc::new(tokio::sync::OnceCell::new()), seq)).clone()
    };
    let (app2, g2, st2) = (app.clone(), guild_id.to_string(), st.clone());
    let result = cell
        .get_or_init(|| async move {
            match load_channels(app2.db(), &g2).await {
                Ok(map) => {
                    let channels = Arc::new(map);
                    let now = app2.clock().now_ms();
                    if let Ok(mut m) = st2.memo.lock() {
                        // Not when a change came meanwhile.
                        if m.invalidated_at.get(&g2).copied().unwrap_or(0) <= started_at {
                            remember(&mut m, &g2, channels.clone(), now);
                        }
                    }
                    Some(channels)
                }
                Err(e) => {
                    bot_util::log_warn!(format!("[sticky] {g2}: could not load the sticky channels: {e}"));
                    None
                }
            }
        })
        .await
        .clone();
    if let Ok(mut m) = st.memo.lock()
        && m.loading.get(guild_id).is_some_and(|(c, _)| Arc::ptr_eq(c, &cell))
    {
        m.loading.remove(guild_id);
    }
    result
}

/// `invalidate(guildId)`: forgets the guild's sticky channels (after any change).
pub fn invalidate(app: &App, guild_id: &str) {
    let st = state(app);
    let Ok(mut m) = st.memo.lock() else { return };
    m.entries.remove(guild_id);
    m.loading.remove(guild_id);
    m.sequence += 1;
    let seq = m.sequence;
    if m.invalidated_at.insert(guild_id.to_string(), seq).is_none() {
        m.invalidated_order.push(guild_id.to_string());
    }
    if m.invalidated_at.len() > timing::MEMO_MAX_GUILDS * 2 && !m.invalidated_order.is_empty() {
        let first = m.invalidated_order.remove(0);
        m.invalidated_at.remove(&first);
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// `embedPart(template)`: what to keep of a saved embed's template: its embed and link buttons (its text is the
/// sticky's own text). `None` when it has neither.
pub fn embed_part(template: &Value) -> Option<Value> {
    let t = bot_core::custom_embed::normalize_template(template);
    let has_buttons = t["buttons"].as_array().is_some_and(|b| !b.is_empty());
    if !t["embed"].is_null() || has_buttons { Some(json!({ "embed": t["embed"], "buttons": t["buttons"] })) } else { None }
}

/// `renderSticky(row, { guild, channel, settings })`: the message to post for a row: text through the placeholders
/// (`{server}`, `{channel}`; nobody is "the user" here), the embed, and no pings whatsoever. `None` when nothing is left.
pub fn render_sticky(
    content: Option<&str>,
    embed: Option<&Value>,
    ctx: &bot_util::placeholder_parser::RenderContext,
    settings: Option<&GuildSettings>,
) -> Option<bot_core::custom_embed::Rendered> {
    let part = match embed.filter(|e| !e.is_null()) {
        Some(e) => bot_core::custom_embed::normalize_template(e),
        None => json!({ "embed": null, "buttons": [] }),
    };
    let tpl = json!({ "content": content, "embed": part["embed"], "buttons": part["buttons"] });
    bot_core::custom_embed::render_template(
        &tpl,
        ctx,
        bot_core::custom_embed::RenderOptions {
            default_color: Some(feat_embeds::embed_service::resolve_default_color(settings)),
            allowed_mentions: Some(serenity::builder::CreateAllowedMentions::new()),
            extra_components: Vec::new(),
        },
    )
}

/// `stickyNeeds(hasEmbed)`: the permissions the bot needs to keep a sticky in a channel.
pub fn sticky_needs(has_embed: bool) -> &'static [&'static str] {
    if has_embed { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] }
}

// ── Re-sending ────────────────────────────────────────────────────────────────

fn mark_self_deleted(app: &App, id: &str) {
    let now = app.clock().now_ms();
    let st = state(app);
    let Ok(mut s) = st.self_deleted.lock() else { return };
    if s.len() > 200 {
        s.retain(|_, t| now - *t < timing::SELF_DELETE_TTL_MS);
    }
    s.insert(id.to_string(), now);
}

/// `consumeSelfDeleted(messageId)`: true when the bot deleted this message itself (once: the mark is consumed).
pub fn consume_self_deleted(app: &App, message_id: &str) -> bool {
    let now = app.clock().now_ms();
    let st = state(app);
    let Ok(mut s) = st.self_deleted.lock() else { return false };
    match s.remove(message_id) {
        Some(t) => now - t < timing::SELF_DELETE_TTL_MS,
        None => false,
    }
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `deleteMessage(channel, messageId)`: deletes a sticky message, ignoring one that is already gone.
async fn delete_message(app: &App, guild_id: &str, channel_id: ChannelId, message_id: &str) -> bool {
    let (Some(http), Some(mid)) = (app.http(), parse_id(message_id)) else { return false };
    mark_self_deleted(app, message_id);
    match http.delete_message(channel_id, MessageId::new(mid), None).await {
        Ok(()) => true,
        Err(e) => {
            if let Ok(mut s) = state(app).self_deleted.lock() {
                s.remove(message_id);
            }
            if bot_core::discord::error_code(&e) != Some(UNKNOWN_MESSAGE) {
                bot_util::log_warn!(format!("[sticky] {guild_id}: could not delete {message_id}: {}", bot_core::discord::error_message(&e)));
            }
            false
        }
    }
}

/// What [`resend_sticky`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResendStatus {
    Sent,
    Current,
    Missing,
    Paused,
    Off,
    Gone,
    Perms,
    Empty,
    Failed,
}

impl ResendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Current => "current",
            Self::Missing => "missing",
            Self::Paused => "paused",
            Self::Off => "off",
            Self::Gone => "gone",
            Self::Perms => "perms",
            Self::Empty => "empty",
            Self::Failed => "failed",
        }
    }
}

/// The raw statements of this file (name, SQL), run in a rolled-back transaction by the tests.
pub const RAW_SQL: [(&str, &str); 7] = [
    ("channels", r#"SELECT "channelId", "enabled" FROM "StickyMessage" WHERE "guildId" = $1"#),
    ("count", r#"SELECT COUNT(*)::bigint FROM "StickyMessage" WHERE "guildId" = $1"#),
    (
        "clearLastOnFail",
        r#"UPDATE "StickyMessage" SET "lastMessageId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "channelId" = $1 AND "lastMessageId" = $2"#,
    ),
    ("deleteOne", r#"DELETE FROM "StickyMessage" WHERE "guildId" = $1 AND "channelId" = $2"#),
    (
        "setLast",
        r#"UPDATE "StickyMessage" SET "lastMessageId" = $3, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "channelId" = $1 AND "guildId" = $2"#,
    ),
    (
        "clearLastIf",
        r#"UPDATE "StickyMessage" SET "lastMessageId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "guildId" = $1 AND "channelId" = $2 AND "lastMessageId" = $3"#,
    ),
    (
        "setEnabled",
        r#"UPDATE "StickyMessage" SET "enabled" = $3, "lastMessageId" = CASE WHEN $3 THEN "lastMessageId" ELSE NULL END, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "guildId" = $1 AND "channelId" = $2"#,
    ),
];

fn sql(name: &str) -> &'static str {
    RAW_SQL.iter().find(|(n, _)| *n == name).map(|(_, s)| *s).unwrap_or("SELECT 1")
}

fn db(e: bot_db::sqlx::Error) -> anyhow::Error {
    anyhow::Error::from(bot_db::DbError::from(e))
}

/// The row of a channel (`findUnique({ where: { channelId } })`).
pub async fn find_by_channel(pool: &PgPool, channel_id: &str) -> anyhow::Result<Option<StickyMessage>> {
    let q = SelectBuilder::new(&STICKY_MESSAGE_INFO).filter("channelId", json!(channel_id))?.build()?;
    Ok(q.fetch_optional::<StickyMessage, _>(pool).await?)
}

async fn set_last(pool: &PgPool, channel_id: &str, guild_id: &str, message_id: &str) -> anyhow::Result<u64> {
    Ok(bot_db::sqlx::query(sql("setLast")).bind(channel_id).bind(guild_id).bind(message_id).execute(pool).await.map_err(db)?.rows_affected())
}

/// `resendSticky(client, guildId, channelId, { force })`: deletes the channel's previous sticky message and posts it
/// again, saving the new message id. Call it under [`run_exclusive`]. Never fails on Discord errors; a database error is
/// an error. `force`: also when the sticky already is the channel's last message.
pub async fn resend_sticky(app: &App, guild_id: &str, channel_id: &str, force: bool) -> anyhow::Result<ResendStatus> {
    let row = find_by_channel(app.db(), channel_id).await?;
    let Some(row) = row.filter(|r| r.guild_id == guild_id) else {
        invalidate(app, guild_id);
        return Ok(ResendStatus::Missing);
    };
    if !row.enabled {
        return Ok(ResendStatus::Paused);
    }
    let (Some(g), Some(c)) = (parse_id(guild_id), parse_id(channel_id)) else { return Ok(ResendStatus::Gone) };
    let (gid, cid) = (GuildId::new(g), ChannelId::new(c));
    // channelDelete drops the row.
    let Some(channel) = cached_channel(app, gid, cid) else { return Ok(ResendStatus::Gone) };

    let Some(settings) = app.settings().get_settings_safe(guild_id).await else { return Ok(ResendStatus::Failed) };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Sticky)) {
        return Ok(ResendStatus::Off);
    }
    let ctx = with_cached_guild(app, gid, |gd| crate::captcha_service::guild_render_context(Some(gd), Some(&channel))).unwrap_or_default();
    let Some(payload) = render_sticky(row.content.as_deref(), row.embed.as_ref(), &ctx, Some(&settings)) else {
        bot_util::log_warn!(format!("[sticky] {guild_id}: the sticky in {channel_id} renders to nothing"));
        return Ok(ResendStatus::Empty);
    };
    let missing = missing_bot_permissions(app, PermTarget::Channel(gid, cid), sticky_needs(!payload.embeds.is_empty()));
    if !missing.is_empty() {
        let st = state(app);
        let t = st.perms_warn.get_or_init(|| bot_core::clock::clock_throttle(app.clock(), timing::PERMS_WARN_MS));
        if t.allow(channel_id) {
            bot_util::log_warn!(format!("[sticky] {guild_id}: cannot post in {channel_id}: missing {}", missing.join(", ")));
        }
        return Ok(ResendStatus::Perms);
    }
    if !force
        && let Some(last) = &row.last_message_id
        && channel.last_message_id.map(|m| m.to_string()).as_deref() == Some(last.as_str())
    {
        return Ok(ResendStatus::Current);
    }

    if let Some(last) = &row.last_message_id {
        delete_message(app, guild_id, cid, last).await;
    }
    let Some(http) = app.http() else { return Ok(ResendStatus::Failed) };
    let sent = match cid.send_message(http.as_ref(), payload.to_payload().to_message()).await {
        Ok(m) => m,
        Err(e) => {
            if let Some(last) = &row.last_message_id {
                bot_db::sqlx::query(sql("clearLastOnFail"))
                    .bind(channel_id)
                    .bind(last)
                    .execute(app.db())
                    .await
                    .map_err(db)?;
            }
            let code = bot_core::discord::error_code(&e);
            if let Some(c) = code.filter(|c| GONE_CODES.contains(c)) {
                return Ok(if c == 50013 { ResendStatus::Perms } else { ResendStatus::Gone });
            }
            bot_util::log_warn!(format!("[sticky] {guild_id}: could not post in {channel_id}: {}", bot_core::discord::error_message(&e)));
            return Ok(ResendStatus::Failed);
        }
    };
    if set_last(app.db(), channel_id, guild_id, &sent.id.to_string()).await? == 0 {
        // Removed while the message was on its way: do not leave it behind.
        delete_message(app, guild_id, cid, &sent.id.to_string()).await;
        return Ok(ResendStatus::Missing);
    }
    Ok(ResendStatus::Sent)
}

// ── Admin operations ──────────────────────────────────────────────────────────

/// `listStickies(guildId)`: the guild's rows, oldest first.
pub async fn list_stickies(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<StickyMessage>> {
    let q = SelectBuilder::new(&STICKY_MESSAGE_INFO).filter("guildId", json!(guild_id))?.order_by("createdAt", true)?.build()?;
    Ok(q.fetch_all::<StickyMessage, _>(pool).await?)
}

/// `getSticky(guildId, channelId)`: one channel's row.
pub async fn get_sticky(pool: &PgPool, guild_id: &str, channel_id: &str) -> anyhow::Result<Option<StickyMessage>> {
    let q = SelectBuilder::new(&STICKY_MESSAGE_INFO).filter("guildId", json!(guild_id))?.filter("channelId", json!(channel_id))?.build()?;
    Ok(q.fetch_optional::<StickyMessage, _>(pool).await?)
}

/// `countStickies(guildId)`: how many channels have a sticky in the guild.
pub async fn count_stickies(pool: &PgPool, guild_id: &str) -> anyhow::Result<i64> {
    bot_db::sqlx::query_scalar(sql("count")).bind(guild_id).fetch_one(pool).await.map_err(db)
}

/// What [`set_sticky`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetResult {
    pub created: bool,
    pub has_embed: bool,
    pub status: ResendStatus,
}

/// What [`set_sticky`] is asked to store.
#[derive(Debug, Clone, Default)]
pub struct SetArgs {
    pub guild_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub content: Option<String>,
    /// `{ embed, buttons }` ([`embed_part`]).
    pub embed: Option<Value>,
    /// Keep the current embed (the text form edits the text only).
    pub keep_embed: bool,
}

/// `setSticky(...)`: creates or replaces a channel's sticky, enables it and posts it now (the old message is deleted
/// first). Refusals are a [`StickyError`] inside the error.
pub async fn set_sticky(app: &App, args: SetArgs) -> anyhow::Result<SetResult> {
    let SetArgs { guild_id, channel_id, user_id, content, embed, keep_embed } = args;
    let text = content.filter(|c| !bot_util::js::trim(c).is_empty());
    let max = bot_core::custom_embed::limits::CONTENT;
    if let Some(t) = &text
        && bot_util::js::utf16_len(t) > max
    {
        return Err(StickyError::new("TOO_LONG", "Message Too Long", format!("The text is {} characters; the limit is {max}.", bot_util::js::utf16_len(t))).into());
    }

    let existing = get_sticky(app.db(), &guild_id, &channel_id).await?;
    let part = if keep_embed { existing.as_ref().and_then(|e| e.embed.as_ref()).filter(|e| !e.is_null()).and_then(embed_part) } else { embed };
    let part_embed = part.as_ref().map(|p| p["embed"].clone()).unwrap_or(Value::Null);
    let part_buttons = part.as_ref().map(|p| p["buttons"].clone()).filter(Value::is_array).unwrap_or_else(|| json!([]));
    let check = bot_core::custom_embed::validate_template(&json!({ "content": text, "embed": part_embed, "buttons": part_buttons }), true);
    if !check.ok {
        let first: Vec<String> = check.errors.iter().take(3).cloned().collect();
        return Err(StickyError::new("INVALID", "Nothing To Stick", first.join("\n")).into());
    }

    let has_embed = !part_embed.is_null();
    let (g, c) = (GuildId::new(parse_id(&guild_id).unwrap_or(1)), ChannelId::new(parse_id(&channel_id).unwrap_or(1)));
    let usable = check_channel_usable(app, g, Some(c), sticky_needs(has_embed));
    if !usable.ok {
        return Err(StickyError::new("PERMS", "Cannot Use That Channel", format!("{} Fix my permissions there, or pick another channel.", usable.reason.unwrap_or_default())).into());
    }

    app.settings().ensure_guild_settings(&guild_id).await?;
    let (app2, g2, c2, u2, t2, p2) = (app.clone(), guild_id.clone(), channel_id.clone(), user_id.clone(), text.clone(), part.clone());
    let created: anyhow::Result<bool> = state(app)
        .guild_lock
        .run(&guild_id, move || async move {
            let row = get_sticky(app2.db(), &g2, &c2).await?;
            let limit = bot_config::limits::STICKY_MESSAGES;
            if row.is_none() && count_stickies(app2.db(), &g2).await? >= limit as i64 {
                return Err(StickyError::new(
                    "LIMIT",
                    "Limit Reached",
                    format!("A server can have at most **{limit}** sticky messages. Remove one with `/sticky remove` first."),
                )
                .into());
            }
            let data = [("content", json!(t2)), ("embed", p2.unwrap_or(Value::Null)), ("enabled", json!(true))];
            let q = InsertBuilder::new(&STICKY_MESSAGE_INFO)
                .values([("guildId", json!(g2)), ("channelId", json!(c2)), ("createdById", json!(u2))])?
                .values(data.clone())?
                .on_conflict_update(&["channelId"], data)?
                .build()?;
            q.execute(app2.db()).await?;
            Ok(row.is_none())
        })
        .await;
    let created = created?;
    invalidate(app, &guild_id);
    cancel_pending(app, &channel_id);

    let (a, g3, c3) = (app.clone(), guild_id.clone(), channel_id.clone());
    let status = match run_exclusive(app, &channel_id, move || async move { resend_sticky(&a, &g3, &c3, true).await }).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_warn!(format!("[sticky] {guild_id}: posting the new sticky in {channel_id} failed: {e}"));
            ResendStatus::Failed
        }
    };
    Ok(SetResult { created, has_embed, status })
}

/// `removeSticky(client, guildId, channelId)`: removes a channel's sticky and deletes its message. The removed row, or
/// `None` when the channel had none.
pub async fn remove_sticky(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<Option<StickyMessage>> {
    cancel_pending(app, channel_id);
    let (a, g, c) = (app.clone(), guild_id.to_string(), channel_id.to_string());
    run_exclusive(app, channel_id, move || async move {
        let Some(row) = get_sticky(a.db(), &g, &c).await? else { return Ok(None) };
        bot_db::sqlx::query(sql("deleteOne")).bind(&g).bind(&c).execute(a.db()).await.map_err(db)?;
        invalidate(&a, &g);
        if let (Some(gid), Some(cid)) = (parse_id(&g), parse_id(&c))
            && cached_channel(&a, GuildId::new(gid), ChannelId::new(cid)).is_some()
            && let Some(last) = &row.last_message_id
        {
            delete_message(&a, &g, ChannelId::new(cid), last).await;
        }
        Ok(Some(row))
    })
    .await
}

/// What [`set_sticky_enabled`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnabledResult {
    pub found: bool,
    pub changed: bool,
    pub status: Option<ResendStatus>,
}

/// `setStickyEnabled(client, guildId, channelId, enabled)`: pauses (the posted message is deleted) or resumes (it is
/// posted again at once) a channel's sticky.
pub async fn set_sticky_enabled(app: &App, guild_id: &str, channel_id: &str, enabled: bool) -> anyhow::Result<EnabledResult> {
    cancel_pending(app, channel_id);
    let (a, g, c) = (app.clone(), guild_id.to_string(), channel_id.to_string());
    run_exclusive(app, channel_id, move || async move {
        let Some(row) = get_sticky(a.db(), &g, &c).await? else { return Ok(EnabledResult { found: false, changed: false, status: None }) };
        let changed = row.enabled != enabled;
        if changed {
            bot_db::sqlx::query(sql("setEnabled")).bind(&g).bind(&c).bind(enabled).execute(a.db()).await.map_err(db)?;
            invalidate(&a, &g);
        }
        if !enabled {
            if let (Some(gid), Some(cid)) = (parse_id(&g), parse_id(&c))
                && cached_channel(&a, GuildId::new(gid), ChannelId::new(cid)).is_some()
                && let Some(last) = &row.last_message_id
            {
                delete_message(&a, &g, ChannelId::new(cid), last).await;
            }
            return Ok(EnabledResult { found: true, changed, status: Some(ResendStatus::Paused) });
        }
        let status = resend_sticky(&a, &g, &c, false).await?;
        Ok(EnabledResult { found: true, changed, status: Some(status) })
    })
    .await
}

// ── Cleanup (hooks) ───────────────────────────────────────────────────────────

/// `clearLastMessage(guildId, channelId, messageId)`: the sticky message was deleted by someone else: forget its id. Only
/// a row that still points at this message changes, so the delete of a replaced message never clears the new one.
pub async fn clear_last_message(app: &App, guild_id: &str, channel_id: &str, message_id: &str) -> anyhow::Result<u64> {
    if guild_id.is_empty() || channel_id.is_empty() || message_id.is_empty() || consume_self_deleted(app, message_id) {
        return Ok(0);
    }
    Ok(bot_db::sqlx::query(sql("clearLastIf")).bind(guild_id).bind(channel_id).bind(message_id).execute(app.db()).await.map_err(db)?.rows_affected())
}

/// `dropChannel(guildId, channelId)`: a channel was deleted: its sticky row goes with it.
pub async fn drop_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<u64> {
    if guild_id.is_empty() || channel_id.is_empty() {
        return Ok(0);
    }
    cancel_pending(app, channel_id);
    let n = bot_db::sqlx::query(sql("deleteOne")).bind(guild_id).bind(channel_id).execute(app.db()).await.map_err(db)?.rows_affected();
    if n > 0 {
        invalidate(app, guild_id);
    }
    Ok(n)
}

/// `missingFor(channel, hasEmbed)`: which of the bot's permissions a channel lacks, for `/sticky list` (names).
pub fn missing_for(app: &App, guild_id: GuildId, channel_id: ChannelId, has_embed: bool) -> Vec<String> {
    bot_core::permissions::permission_names(&missing_bot_permissions(app, PermTarget::Channel(guild_id, channel_id), sticky_needs(has_embed)))
}
