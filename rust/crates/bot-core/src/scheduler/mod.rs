//! Port of `src/services/scheduler.js`: durable timed work in the `ScheduledJob` table.
//!
//! * [`Scheduler::schedule_job`] with every `onConflict` mode ([`OnConflict`]), [`Scheduler::settle_merged_job`],
//!   [`Scheduler::cancel_jobs`], [`Scheduler::get_job`], [`Scheduler::get_jobs_by_keys`].
//! * The ordinary loop ([`Scheduler::fill_slots`], at most [`JOB_CONCURRENCY`] jobs per shard, claims only for this
//!   shard's available guilds with `FOR UPDATE SKIP LOCKED`), the interactive loop
//!   ([`Scheduler::fill_interactive_slots`], claims by shard, one job per guild), retries with back-off
//!   (30 s x 2^n, max 1 h), [`PermanentJobError`](crate::PermanentJobError), leases (5 min) and
//!   [`Scheduler::tick`] for tests.
//!
//! Handlers are registered by plugins (`Plugin::job`). A handler returns [`JobOutcome::Done`] (the row is deleted) or
//! [`JobOutcome::Reschedule`]; an error retries, a `PermanentJobError` fails at once. Handlers must be idempotent.

mod memory;
mod pg;
mod store;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bot_config::JobType;
use bot_db::{DbError, JobStatus, ScheduledJob};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::clock::SharedClock;
use crate::context::App;
use crate::discord::GuildState;
use crate::error::{BotResult, BoxFuture, PermanentJobError, log_arg, start_guarded};
use crate::settings::SettingsService;

pub use memory::{MemoryJobStore, shard_of};
pub use pg::{GC_SQL, JOB_COLUMNS, LOCK_DEDUPE_SQL, LOCK_JOB_SQL, ONE_PER_GUILD_SQL, PgJobStore, claim_sql, job_from_row, raw_sql};
pub use store::{
    CancelWhere, ClaimRequest, JobStore, JobUpdate, MergeFn, NewJob, OnConflict, Owned, Scheduled, SettleNextFn, SettleOutcome, is_live,
};

pub const POLL_INTERVAL_MS: u64 = 5_000;
/// The interactive loop's fast poll.
pub const INTERACTIVE_POLL_INTERVAL_MS: u64 = 1_000;
/// The interactive loop polls fast only this long after it last claimed a job.
pub const INTERACTIVE_ACTIVE_WINDOW_MS: i64 = 2 * 60_000;
pub const INTERACTIVE_IDLE_POLL_INTERVAL_MS: u64 = 5_000;
/// An interactive job this late is failed instead of run.
pub const INTERACTIVE_MAX_LATE_MS: i64 = 60_000;
pub const CLAIM_BATCH: i64 = 25;
pub const JOB_CONCURRENCY: usize = 5;
/// Interactive jobs one shard runs at once, at most one per guild.
pub const INTERACTIVE_CONCURRENCY: usize = 10;
pub const JOB_TIMEOUT_MS: u64 = 60_000;
pub const LOCK_MS: i64 = 5 * 60_000;
/// `LIVE_STATUSES`.
pub const LIVE_STATUSES: [JobStatus; 2] = [JobStatus::Pending, JobStatus::Running];

const GC_INTERVAL_MS: u64 = 60 * 60_000;
const NO_HANDLER_BACKOFF_MS: i64 = 10 * 60_000;
const RETRY_BASE_MS: i64 = 30_000;
const RETRY_MAX_MS: i64 = 60 * 60_000;
const LAST_ERROR_MAX: usize = 2000;
/// Keys per query of [`Scheduler::get_jobs_by_keys`].
pub const JOB_LOOKUP_CHUNK: usize = 500;

/// What `scheduleJob` does with an existing row (`resolveConflict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Insert,
    Replace,
    Keep,
    Merge,
}

/// `resolveConflict(existing, incoming, mode)`: pure. A row that is not live counts as absent and is replaced.
pub fn resolve_conflict(existing: Option<(JobStatus, DateTime<Utc>)>, incoming_run_at: DateTime<Utc>, mode: &OnConflict) -> ConflictAction {
    let Some((status, run_at)) = existing else { return ConflictAction::Insert };
    if !is_live(status) {
        return ConflictAction::Replace;
    }
    match mode {
        OnConflict::Replace => ConflictAction::Replace,
        OnConflict::Keep => ConflictAction::Keep,
        OnConflict::Merge(_) => ConflictAction::Merge,
        OnConflict::Earliest => {
            if incoming_run_at < run_at {
                ConflictAction::Replace
            } else {
                ConflictAction::Keep
            }
        }
    }
}

/// The retry delay after a failed attempt (`min(30 s * 2^(attempts-1), 1 h)`).
pub fn retry_delay_ms(attempts: i32) -> i64 {
    let exp = (attempts - 1).clamp(0, 30) as u32;
    RETRY_BASE_MS.saturating_mul(2i64.saturating_pow(exp)).min(RETRY_MAX_MS)
}

/// `interactivePollDelay(sinceLastClaimMs)`: the interactive loop's next delay before jitter (`None`: nothing claimed
/// yet, which counts as idle).
pub fn interactive_poll_delay(since_last_claim_ms: Option<i64>) -> u64 {
    match since_last_claim_ms {
        Some(ms) if ms < INTERACTIVE_ACTIVE_WINDOW_MS => INTERACTIVE_POLL_INTERVAL_MS,
        _ => INTERACTIVE_IDLE_POLL_INTERVAL_MS,
    }
}

/// `jitter(ms)`: +-20 %.
pub fn jitter(ms: u64) -> u64 {
    let f = 0.8 + rand::random::<f64>() * 0.4;
    (ms as f64 * f).round() as u64
}

/// `truncate(text)`: the first 2000 UTF-16 units, like `String(text).slice(0, 2000)`.
pub fn truncate_error(text: &str) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        units += ch.len_utf16();
        if units > LAST_ERROR_MAX {
            break;
        }
        out.push(ch);
    }
    out
}

/// A claimed job as its handler sees it (`toJob(row)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub guild_id: String,
    pub job_type: String,
    pub payload: Value,
    pub run_at: DateTime<Utc>,
    /// 1-based: this run's attempt.
    pub attempts: i32,
    pub max_attempts: i32,
    pub dedupe_key: Option<String>,
    pub version: i32,
}

impl Job {
    pub fn from_row(row: &ScheduledJob) -> Self {
        Self {
            id: row.id.clone(),
            guild_id: row.guild_id.clone(),
            job_type: row.r#type.clone(),
            payload: row.payload.clone(),
            run_at: row.run_at,
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            dedupe_key: row.dedupe_key.clone(),
            version: row.version,
        }
    }

    /// The payload deserialized (an error is a handler error: the job retries).
    pub fn payload_as<T: DeserializeOwned>(&self) -> anyhow::Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }

    /// The job type, when it is a registered one.
    pub fn kind(&self) -> Option<JobType> {
        JobType::from_name(&self.job_type)
    }
}

/// What a handler gets besides the job (`ctx = { client, guild, lateByMs, attempt }`). The guild is cached and
/// available when the handler runs; read it through `app.discord()`.
#[derive(Clone)]
pub struct JobCtx {
    pub app: App,
    pub guild_id: String,
    pub late_by_ms: i64,
    /// 1-based; `attempt >= job.max_attempts` means the final try.
    pub attempt: i32,
}

/// What a handler returns.
#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    /// Finished: the row is deleted.
    Done,
    /// Run again at `next_run_at` (attempts reset, version bumped), with `payload` or the same payload.
    Reschedule { next_run_at: DateTime<Utc>, payload: Option<Value> },
}

/// A job handler: `async (job, ctx) => void | { nextRunAt, payload? }`.
pub type JobHandler = Arc<dyn Fn(Job, JobCtx) -> BoxFuture<BotResult<JobOutcome>> + Send + Sync>;

/// Wraps an async closure as a [`JobHandler`].
pub fn job_handler<F, Fut>(f: F) -> JobHandler
where
    F: Fn(Job, JobCtx) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = BotResult<JobOutcome>> + Send + 'static,
{
    Arc::new(move |job, ctx| Box::pin(f(job, ctx)))
}

/// The arguments of `scheduleJob`.
#[derive(Debug, Clone)]
pub struct ScheduleJob {
    pub guild_id: String,
    pub job_type: JobType,
    pub run_at: DateTime<Utc>,
    pub payload: Value,
    pub dedupe_key: Option<String>,
    /// Default 5.
    pub max_attempts: i32,
    /// Default [`OnConflict::Replace`].
    pub on_conflict: OnConflict,
}

impl ScheduleJob {
    pub fn new(guild_id: impl Into<String>, job_type: JobType, run_at: DateTime<Utc>) -> Self {
        Self {
            guild_id: guild_id.into(),
            job_type,
            run_at,
            payload: Value::Object(Default::default()),
            dedupe_key: None,
            max_attempts: 5,
            on_conflict: OnConflict::Replace,
        }
    }

    pub fn payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }

    /// The payload from any serializable value (JSON as the JS wrote it).
    pub fn payload_of<T: Serialize>(mut self, payload: &T) -> anyhow::Result<Self> {
        self.payload = serde_json::to_value(payload).map_err(|e| anyhow::anyhow!("Job payload is not JSON-serialisable: {e}"))?;
        Ok(self)
    }

    pub fn dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    pub fn max_attempts(mut self, n: i32) -> Self {
        self.max_attempts = n;
        self
    }

    pub fn on_conflict(mut self, mode: OnConflict) -> Self {
        self.on_conflict = mode;
        self
    }

    /// `onConflict: 'merge'` with `merge(existingPayload, payload)`.
    pub fn merge(mut self, merge: impl Fn(&Value, &Value) -> Value + Send + Sync + 'static) -> Self {
        self.on_conflict = OnConflict::Merge(Arc::new(merge));
        self
    }
}

/// The `cancelJobs` filter. At least one field is required; `guild_id` alone cancels every job of that guild; an
/// empty `types` or `ids` list matches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancelJobs {
    pub dedupe_key: Option<String>,
    pub guild_id: Option<String>,
    pub job_type: Option<String>,
    pub types: Option<Vec<String>>,
    pub ids: Option<Vec<String>>,
}

#[derive(Default)]
struct LoopState {
    started: bool,
    running: usize,
    filling: bool,
    refill_asked: bool,
    backlog: bool,
    interactive_running: usize,
    interactive_filling: bool,
    interactive_refill_asked: bool,
    last_interactive_claim: Option<i64>,
    timers: Vec<JoinHandle<()>>,
}

struct Inner {
    store: Arc<dyn JobStore>,
    settings: SettingsService,
    clock: SharedClock,
    handlers: RwLock<HashMap<String, JobHandler>>,
    warned_missing: Mutex<HashSet<String>>,
    loops: Mutex<LoopState>,
    inflight: AtomicUsize,
    idle: Notify,
    interactive_fill_done: Notify,
    tag: u32,
}

static NEXT_TAG: AtomicU32 = AtomicU32::new(1);

/// The scheduler. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

/// Counts a running piece of work so `stop` can wait for it.
struct Inflight(Arc<Inner>);

impl Inflight {
    fn new(inner: &Arc<Inner>) -> Self {
        inner.inflight.fetch_add(1, Ordering::SeqCst);
        Self(inner.clone())
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Scheduler {
    pub fn new(store: Arc<dyn JobStore>, settings: SettingsService, clock: SharedClock) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                settings,
                clock,
                handlers: RwLock::new(HashMap::new()),
                warned_missing: Mutex::new(HashSet::new()),
                loops: Mutex::new(LoopState::default()),
                inflight: AtomicUsize::new(0),
                idle: Notify::new(),
                interactive_fill_done: Notify::new(),
                tag: NEXT_TAG.fetch_add(1, Ordering::Relaxed),
            }),
        }
    }

    /// The storage (tests reach the in-memory store through it).
    pub fn store(&self) -> &Arc<dyn JobStore> {
        &self.inner.store
    }

    fn loops(&self) -> std::sync::MutexGuard<'_, LoopState> {
        self.inner.loops.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_ms(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    fn at(&self, ms_from_now: i64) -> DateTime<Utc> {
        self.inner.clock.now() + chrono::Duration::milliseconds(ms_from_now)
    }

    // ── Handlers ──────────────────────────────────────────────────────────────

    /// `registerJobHandler(type, handler)`. Registering the same handler (`Arc::ptr_eq`) again is fine; a different
    /// one for the same type is refused.
    pub fn register_job_handler(&self, job_type: JobType, handler: JobHandler) -> Result<(), String> {
        let mut map = self.inner.handlers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(job_type.as_str())
            && !Arc::ptr_eq(existing, &handler)
        {
            return Err(format!("A handler is already registered for job type \"{job_type}\""));
        }
        map.insert(job_type.as_str().to_string(), handler);
        Ok(())
    }

    /// Whether a handler is registered for the type.
    pub fn has_handler(&self, job_type: &str) -> bool {
        self.inner.handlers.read().unwrap_or_else(|e| e.into_inner()).contains_key(job_type)
    }

    fn handler(&self, job_type: &str) -> Option<JobHandler> {
        self.inner.handlers.read().unwrap_or_else(|e| e.into_inner()).get(job_type).cloned()
    }

    // ── Scheduling ────────────────────────────────────────────────────────────

    /// `scheduleJob(opts)`: creates the job, or with a dedupe key upserts it by `on_conflict`. Calls
    /// `ensureGuildSettings` first. A unique-violation race on a new key is retried once.
    pub async fn schedule_job(&self, opts: ScheduleJob) -> anyhow::Result<Scheduled> {
        if opts.guild_id.is_empty() {
            anyhow::bail!("scheduleJob needs a guildId");
        }
        if matches!(opts.on_conflict, OnConflict::Merge(_)) && opts.dedupe_key.as_deref().is_none_or(str::is_empty) {
            anyhow::bail!("onConflict 'merge' needs a dedupeKey and a merge function");
        }
        if opts.max_attempts < 1 {
            anyhow::bail!("maxAttempts must be a positive integer");
        }
        let payload = if opts.payload.is_null() { Value::Object(Default::default()) } else { opts.payload.clone() };

        self.inner.settings.ensure_guild_settings(&opts.guild_id).await?;

        let dedupe_key = opts.dedupe_key.clone().filter(|k| !k.is_empty());
        let job = NewJob {
            guild_id: opts.guild_id.clone(),
            job_type: opts.job_type.as_str().to_string(),
            run_at: opts.run_at,
            payload,
            dedupe_key: dedupe_key.clone(),
            max_attempts: opts.max_attempts,
        };
        if dedupe_key.is_none() {
            return Ok(self.inner.store.insert(&job).await?);
        }
        let mut attempt = 0;
        loop {
            match self.inner.store.upsert_dedupe(&job, &opts.on_conflict).await {
                Ok(s) => return Ok(s),
                Err(e) if e.is_unique_violation() && attempt == 0 => attempt += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// `cancelJobs(filter)`: deletes matching jobs whatever their status. Never fails (0 on error; the error is logged).
    /// Use [`Scheduler::try_cancel_jobs`] to see the database error.
    pub async fn cancel_jobs(&self, filter: CancelJobs) -> u64 {
        match self.try_cancel_jobs(filter).await {
            Ok(n) => n,
            Err(e) => {
                bot_util::log_error!("cancelJobs failed", bot_util::logger::LogArg::error(&e));
                0
            }
        }
    }

    /// `cancelJobs(filter)` as the JS has it: a database error is returned (the JS call throws), so a caller can fail
    /// as the JS does instead of reading it as "nothing cancelled". An empty or missing filter cancels nothing
    /// (`Ok(0)`, with the same warning as [`Scheduler::cancel_jobs`]).
    pub async fn try_cancel_jobs(&self, filter: CancelJobs) -> Result<u64, bot_db::DbError> {
        let mut w = CancelWhere::default();
        if let Some(k) = filter.dedupe_key.filter(|k| !k.is_empty()) {
            w.dedupe_key = Some(k);
        }
        if let Some(g) = filter.guild_id.filter(|g| !g.is_empty()) {
            w.guild_id = Some(g);
        }
        if filter.job_type.is_some() || filter.types.is_some() {
            let mut list = filter.types.unwrap_or_default();
            if let Some(t) = filter.job_type.filter(|t| !t.is_empty()) {
                list.push(t);
            }
            if list.is_empty() {
                return Ok(0);
            }
            w.types = Some(list);
        }
        if let Some(ids) = filter.ids {
            if ids.is_empty() {
                return Ok(0);
            }
            w.ids = Some(ids);
        }
        if w == CancelWhere::default() {
            bot_util::log_warn!("cancelJobs called without a filter; nothing cancelled");
            return Ok(0);
        }
        self.inner.store.cancel(&w).await
    }

    /// `getJob(dedupeKey)`: the row, or `None` (also on error).
    pub async fn get_job(&self, dedupe_key: &str) -> Option<ScheduledJob> {
        match self.inner.store.get(dedupe_key).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("getJob failed", bot_util::logger::LogArg::error(&e));
                None
            }
        }
    }

    /// `getJob(dedupeKey)` as the JS has it: a database error is returned (the JS call throws), so a caller can fail
    /// closed instead of reading it as "no job".
    pub async fn try_get_job(&self, dedupe_key: &str) -> Result<Option<ScheduledJob>, bot_db::DbError> {
        self.inner.store.get(dedupe_key).await
    }

    /// `getJobsByKeys(keys)`: one query per 500 distinct keys; dedupeKey -> row (a key with no job is absent).
    /// `None` on a database error, so a caller can tell "no jobs" from "could not look".
    pub async fn get_jobs_by_keys<S: AsRef<str>>(&self, dedupe_keys: &[S]) -> Option<HashMap<String, ScheduledJob>> {
        let mut seen = HashSet::new();
        let keys: Vec<String> = dedupe_keys.iter().map(|k| k.as_ref().to_string()).filter(|k| seen.insert(k.clone())).collect();
        let mut out = HashMap::new();
        for chunk in keys.chunks(JOB_LOOKUP_CHUNK) {
            match self.inner.store.get_many(chunk).await {
                Ok(rows) => {
                    for row in rows {
                        if let Some(k) = row.dedupe_key.clone() {
                            out.insert(k, row);
                        }
                    }
                }
                Err(e) => {
                    bot_util::log_error!("getJobsByKeys failed", bot_util::logger::LogArg::error(&e));
                    return None;
                }
            }
        }
        Some(out)
    }

    /// `settleMergedJob(job, next, runAt)`: the last step of a handler whose job takes merge changes while it runs.
    /// Under the row lock, `next(livePayload)` returns `None` (the row is deleted) or the payload to run again with
    /// (back to PENDING at `run_at`, default now). Does nothing when the job was replaced or cancelled meanwhile. The
    /// handler then returns [`JobOutcome::Done`]: the scheduler's own settle finds the row gone or changed.
    pub async fn settle_merged_job(
        &self,
        job: &Job,
        next: impl FnOnce(&Value) -> Option<Value> + Send + 'static,
        run_at: Option<DateTime<Utc>>,
    ) -> Result<SettleOutcome, DbError> {
        let run_at = run_at.unwrap_or_else(|| self.inner.clock.now());
        self.inner.store.settle_merged(&job.id, job.version, Box::new(next), run_at).await
    }

    // ── Claim and execute ─────────────────────────────────────────────────────

    /// `ownerOf(client)`: `<shard ids or 0>:<pid>:<tag>`.
    pub fn owner_of(&self, app: &App) -> String {
        let dir = app.guild_directory();
        let ids = dir.shard_ids().map(|ids| ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")).unwrap_or_else(|| "0".into());
        format!("{ids}:{}:{}", std::process::id(), self.inner.tag)
    }

    /// `shardScope(client)`: the shards for claims by shard; `None` when this process holds every guild.
    pub fn shard_scope(app: &App) -> Owned {
        let dir = app.guild_directory();
        let count = dir.shard_count();
        let ids = dir.shard_ids().unwrap_or_default();
        if count > 1 && !ids.is_empty() { Owned::Shards { count, ids } } else { Owned::All }
    }

    fn interactive_types() -> Vec<String> {
        bot_config::INTERACTIVE_JOB_TYPES.iter().map(|t| t.as_str().to_string()).collect()
    }

    async fn settle(&self, job: &Job, update: JobUpdate) -> Result<u64, DbError> {
        self.inner.store.settle(&job.id, job.version, &update).await
    }

    /// Gives the claim back without spending an attempt.
    async fn release(&self, job: &Job, run_at: Option<DateTime<Utc>>) -> Result<u64, DbError> {
        self.settle(
            job,
            JobUpdate { status: Some(JobStatus::Pending), attempts: Some((job.attempts - 1).max(0)), run_at, clear_lock: true, ..Default::default() },
        )
        .await
    }

    async fn fail(&self, job: &Job, message: String, permanent: bool) -> Result<u64, DbError> {
        let message = truncate_error(&message);
        if permanent || job.attempts >= job.max_attempts {
            return self
                .settle(job, JobUpdate { status: Some(JobStatus::Failed), last_error: Some(Some(message)), clear_lock: true, ..Default::default() })
                .await;
        }
        let delay = retry_delay_ms(job.attempts);
        self.settle(
            job,
            JobUpdate {
                status: Some(JobStatus::Pending),
                run_at: Some(self.at(delay)),
                last_error: Some(Some(message)),
                clear_lock: true,
                ..Default::default()
            },
        )
        .await
    }

    async fn refuse(&self, job: &Job, last_error: &str) {
        let r = self
            .settle(job, JobUpdate { status: Some(JobStatus::Failed), last_error: Some(Some(last_error.to_string())), clear_lock: true, ..Default::default() })
            .await;
        if let Err(e) = r {
            bot_util::log_error!(format!("Could not record the failure of job {}", job.id), bot_util::logger::LogArg::error(&e));
        }
    }

    /// `runJob(client, row)`. Never fails.
    pub async fn run_job(&self, app: &App, row: ScheduledJob) {
        if let Err(e) = self.run_job_inner(app, row.clone()).await {
            bot_util::log_error!(format!("Job {} {} failed outside its handler", row.r#type, row.id), bot_util::logger::LogArg::error(&e));
        }
    }

    async fn run_job_inner(&self, app: &App, row: ScheduledJob) -> Result<(), DbError> {
        let job = Job::from_row(&row);

        if job.attempts > job.max_attempts {
            self.settle(
                &job,
                JobUpdate {
                    status: Some(JobStatus::Failed),
                    last_error: Some(Some(format!("claimed {} times without completing (worker crashed or timed out)", job.attempts))),
                    clear_lock: true,
                    ..Default::default()
                },
            )
            .await?;
            bot_util::log_error!(format!("Job {} {} (guild {}) abandoned after {} claims", job.job_type, job.id, job.guild_id, job.attempts));
            return Ok(());
        }

        let interactive = JobType::from_name(&job.job_type).is_some_and(JobType::is_interactive);
        let Some(handler) = self.handler(&job.job_type) else {
            if interactive {
                self.refuse(&job, "This bot version cannot do that yet. Update the bot, then try again.").await;
                return Ok(());
            }
            // A plugin may not have landed yet: wait without spending an attempt.
            self.release(&job, Some(self.at(NO_HANDLER_BACKOFF_MS))).await?;
            let first = self.inner.warned_missing.lock().unwrap_or_else(|e| e.into_inner()).insert(job.job_type.clone());
            if first {
                bot_util::log_warn!(format!("No handler registered for job type {}; its jobs wait", job.job_type));
            }
            return Ok(());
        };

        if app.guild_directory().guild_state(&job.guild_id) != GuildState::Available {
            if interactive {
                self.refuse(&job, "Discord has this server unavailable right now, so nothing was done. Try again in a few minutes.").await;
                return Ok(());
            }
            // The claim filter skips the guild until it is available again; runAt stays as it was.
            self.release(&job, None).await?;
            return Ok(());
        }

        let late_by_ms = (self.now_ms() - job.run_at.timestamp_millis()).max(0);
        if interactive && late_by_ms > INTERACTIVE_MAX_LATE_MS {
            self.refuse(&job, "This request waited too long for the bot and was dropped, so nothing was done. Try again.").await;
            return Ok(());
        }

        let ctx = JobCtx { app: app.clone(), guild_id: job.guild_id.clone(), late_by_ms, attempt: job.attempts };
        let label = format!("job {}", job.job_type);
        let j = job.clone();
        let fut = start_guarded(move || handler(j, ctx));
        let result = bot_util::async_util::with_timeout(fut, JOB_TIMEOUT_MS, &label).await;
        let outcome = match result {
            Ok(o) => o,
            Err(err) => {
                let (message, permanent, arg) = match &err {
                    bot_util::async_util::WithTimeoutError::Timeout(t) => (t.to_string(), false, bot_util::logger::LogArg::error(t)),
                    bot_util::async_util::WithTimeoutError::Inner(e) => (e.to_string(), PermanentJobError::is(e), log_arg(e)),
                };
                bot_util::log_error!(
                    format!("Job {} {} (guild {}) failed on attempt {}/{}", job.job_type, job.id, job.guild_id, job.attempts, job.max_attempts),
                    arg
                );
                if let Err(db) = self.fail(&job, message, permanent).await {
                    bot_util::log_error!(format!("Could not record the failure of job {}", job.id), bot_util::logger::LogArg::error(&db));
                }
                return Ok(());
            }
        };

        let done = match outcome {
            JobOutcome::Reschedule { next_run_at, payload } => {
                self.settle(
                    &job,
                    JobUpdate {
                        status: Some(JobStatus::Pending),
                        run_at: Some(next_run_at),
                        attempts: Some(0),
                        last_error: Some(None),
                        payload: Some(payload.unwrap_or_else(|| job.payload.clone())),
                        clear_lock: true,
                        bump_version: true,
                    },
                )
                .await
            }
            JobOutcome::Done => self.inner.store.delete_version(&job.id, job.version).await,
        };
        if let Err(e) = done {
            // The row stays RUNNING and is re-claimed when its lock expires.
            bot_util::log_error!(format!("Could not complete job {} {}", job.job_type, job.id), bot_util::logger::LogArg::error(&e));
        }
        Ok(())
    }

    fn claim_request(&self, app: &App, owned: Owned, interactive: bool, limit: i64) -> ClaimRequest {
        ClaimRequest { owned, owner: self.owner_of(app), interactive, limit, interactive_types: Self::interactive_types() }
    }

    /// `tick(client, { interactive })`: one claim (up to 25) and run of the claimed jobs, for tests. Never fails;
    /// the jobs claimed.
    pub async fn tick(&self, app: &App, interactive: bool) -> usize {
        let _flight = Inflight::new(&self.inner);
        let owned = if interactive {
            Self::shard_scope(app)
        } else {
            let ids = app.guild_directory().available_guild_ids();
            if ids.is_empty() {
                return 0;
            }
            Owned::Guilds(ids)
        };
        let req = self.claim_request(app, owned, interactive, CLAIM_BATCH);
        let rows = match self.inner.store.claim(&req).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("Scheduler tick failed", bot_util::logger::LogArg::error(&e));
                return 0;
            }
        };
        let n = rows.len();
        if n == 0 {
            return 0;
        }
        let this = self.clone();
        let app2 = app.clone();
        bot_util::async_util::map_limit(rows, JOB_CONCURRENCY, move |row, _| {
            let this = this.clone();
            let app = app2.clone();
            async move {
                this.run_job(&app, row).await;
                Ok::<(), ()>(())
            }
        })
        .await;
        n
    }

    /// `fillSlots(client)`: claims as many due ordinary jobs as there are free slots and starts them without waiting.
    /// While the last claim filled every free slot (a backlog), each finished job refills at once. Never fails; the
    /// jobs claimed.
    pub async fn fill_slots(&self, app: &App) -> usize {
        let free = {
            let st = self.loops();
            if !st.started || st.running >= JOB_CONCURRENCY {
                return 0;
            }
            JOB_CONCURRENCY - st.running
        };
        let ids = app.guild_directory().available_guild_ids();
        if ids.is_empty() {
            return 0;
        }
        let req = self.claim_request(app, Owned::Guilds(ids), false, free as i64);
        let rows = match self.inner.store.claim(&req).await {
            Ok(r) => r,
            Err(e) => {
                self.loops().backlog = false; // the database is failing: leave the retry to the next poll
                bot_util::log_error!("Scheduler tick failed", bot_util::logger::LogArg::error(&e));
                return 0;
            }
        };
        let n = rows.len();
        self.loops().backlog = n == free;
        for row in rows {
            self.loops().running += 1;
            let flight = Inflight::new(&self.inner);
            let this = self.clone();
            let app = app.clone();
            tokio::spawn(async move {
                this.run_job(&app, row).await;
                let backlog = {
                    let mut st = this.loops();
                    st.running -= 1;
                    st.backlog
                };
                drop(flight);
                if backlog {
                    this.request_fill(&app);
                }
            });
        }
        n
    }

    /// `requestFill(client)`: one claim at a time; a request during a claim is served right after it.
    pub fn request_fill(&self, app: &App) {
        {
            let mut st = self.loops();
            if !st.started {
                return;
            }
            if st.filling {
                st.refill_asked = true;
                return;
            }
            st.filling = true;
        }
        let flight = Inflight::new(&self.inner);
        let this = self.clone();
        let app = app.clone();
        tokio::spawn(async move {
            this.fill_slots(&app).await;
            let again = {
                let mut st = this.loops();
                st.filling = false;
                std::mem::take(&mut st.refill_asked)
            };
            drop(flight);
            if again {
                this.request_fill(&app);
            }
        });
    }

    /// `fillInteractiveSlots(client)`: like [`Scheduler::fill_slots`] for the interactive types (claims by shard, at
    /// most one job per guild); every finished job refills at once. Never fails; the jobs claimed.
    pub async fn fill_interactive_slots(&self, app: &App) -> usize {
        let free = {
            let st = self.loops();
            if !st.started || st.interactive_running >= INTERACTIVE_CONCURRENCY {
                return 0;
            }
            INTERACTIVE_CONCURRENCY - st.interactive_running
        };
        let req = self.claim_request(app, Self::shard_scope(app), true, free as i64);
        let rows = match self.inner.store.claim(&req).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("Scheduler tick failed", bot_util::logger::LogArg::error(&e));
                return 0;
            }
        };
        let n = rows.len();
        if n > 0 {
            self.loops().last_interactive_claim = Some(self.now_ms());
        }
        for row in rows {
            self.loops().interactive_running += 1;
            let flight = Inflight::new(&self.inner);
            let this = self.clone();
            let app = app.clone();
            tokio::spawn(async move {
                this.run_job(&app, row).await;
                this.loops().interactive_running -= 1;
                drop(flight);
                let again = this.clone();
                tokio::spawn(async move { again.request_interactive_fill(&app).await });
            });
        }
        n
    }

    /// `requestInteractiveFill(client)`: one interactive claim at a time; resolves when that claim has been made (not
    /// when its jobs are done).
    pub fn request_interactive_fill(&self, app: &App) -> BoxFuture<()> {
        let this = self.clone();
        let app = app.clone();
        Box::pin(async move { this.request_interactive_fill_inner(&app).await })
    }

    async fn request_interactive_fill_inner(&self, app: &App) {
        let wait = {
            let mut st = self.loops();
            if !st.started {
                return;
            }
            if st.interactive_filling {
                st.interactive_refill_asked = true;
                let mut notified = Box::pin(self.inner.interactive_fill_done.notified());
                notified.as_mut().enable();
                Some(notified)
            } else {
                st.interactive_filling = true;
                None
            }
        };
        if let Some(notified) = wait {
            notified.await;
            return;
        }
        let _flight = Inflight::new(&self.inner);
        loop {
            self.fill_interactive_slots(app).await;
            let again = {
                let mut st = self.loops();
                let again = std::mem::take(&mut st.interactive_refill_asked) && st.started;
                if !again {
                    st.interactive_filling = false;
                }
                again
            };
            self.inner.interactive_fill_done.notify_waiters();
            if !again {
                return;
            }
        }
    }

    /// `gc()`: deletes finished jobs older than 7 days. Never fails; the rows deleted.
    pub async fn gc(&self) -> u64 {
        match self.inner.store.gc().await {
            Ok(n) => n,
            Err(e) => {
                bot_util::log_error!("Scheduler GC failed", bot_util::logger::LogArg::error(&e));
                0
            }
        }
    }

    /// Whether the loops are running.
    pub fn is_started(&self) -> bool {
        self.loops().started
    }

    /// `start(client)`: the ordinary loop every ~5 s, the interactive loop every ~1 s for 2 min after a claim and
    /// every ~5 s otherwise, and (on the shard holding shard 0, or unsharded) the hourly GC. Timer chains with +-20 %
    /// jitter. Idempotent.
    pub fn start(&self, app: &App) {
        let mut st = self.loops();
        if st.started {
            return;
        }
        st.started = true;
        st.last_interactive_claim = None;
        // A claim aborted by an earlier stop() may have left these set; nothing is in flight now.
        st.filling = false;
        st.refill_asked = false;
        st.interactive_filling = false;
        st.interactive_refill_asked = false;

        let this = self.clone();
        let a = app.clone();
        st.timers.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(jitter(POLL_INTERVAL_MS))).await;
                if !this.is_started() {
                    return;
                }
                this.request_fill(&a);
            }
        }));

        let this = self.clone();
        let a = app.clone();
        st.timers.push(tokio::spawn(async move {
            loop {
                let since = this.loops().last_interactive_claim.map(|t| this.now_ms() - t);
                tokio::time::sleep(Duration::from_millis(jitter(interactive_poll_delay(since)))).await;
                if !this.is_started() {
                    return;
                }
                this.request_interactive_fill(&a).await;
            }
        }));

        let dir = app.guild_directory();
        if dir.shard_ids().is_none_or(|ids| ids.contains(&0)) {
            let this = self.clone();
            st.timers.push(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(GC_INTERVAL_MS)).await;
                    if !this.is_started() {
                        return;
                    }
                    this.gc().await;
                }
            }));
        }
    }

    /// Starts the loops' state without timers (tests drive `fill_slots` themselves, like the JS test that starts the
    /// scheduler on shard 1 and never lets the 5 s poll fire).
    pub fn mark_started(&self) {
        let mut st = self.loops();
        st.started = true;
        st.filling = false;
        st.refill_asked = false;
        st.interactive_filling = false;
        st.interactive_refill_asked = false;
    }

    /// `stop({ timeoutMs })`: clears the timers, then waits up to `timeout_ms` for in-flight jobs and claims.
    pub async fn stop(&self, timeout_ms: u64) {
        let timers = {
            let mut st = self.loops();
            st.started = false;
            std::mem::take(&mut st.timers)
        };
        for t in timers {
            t.abort();
        }
        self.inner.interactive_fill_done.notify_waiters();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        while self.inner.inflight.load(Ordering::SeqCst) > 0 {
            let mut notified = Box::pin(self.inner.idle.notified());
            notified.as_mut().enable();
            if self.inner.inflight.load(Ordering::SeqCst) == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break;
            }
        }
    }

    /// Jobs and claims in flight (tests).
    pub fn inflight(&self) -> usize {
        self.inner.inflight.load(Ordering::SeqCst)
    }

    /// Ordinary jobs running in the slot loop now.
    pub fn running(&self) -> usize {
        self.loops().running
    }
}
