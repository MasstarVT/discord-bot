//! The storage side of the scheduler: what `scheduler.js` asks of Prisma and raw SQL, as a trait with a PostgreSQL
//! implementation ([`super::PgJobStore`]) and an in-memory one ([`super::MemoryJobStore`]).

use std::sync::Arc;

use async_trait::async_trait;
use bot_db::{DbError, JobStatus, ScheduledJob};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Which guilds' jobs a claim may take (`ownedSql`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owned {
    /// This shard's available guilds (`"guildId" = ANY($)`).
    Guilds(Vec<String>),
    /// Every guild of these shards by Discord's formula `(guild_id >> 22) % count` (malformed ids never match).
    Shards { count: u64, ids: Vec<u64> },
    /// Every guild (one process holds them all).
    All,
}

/// What a claim asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRequest {
    pub owned: Owned,
    /// The lease owner written to `lockedBy`.
    pub owner: String,
    /// `true`: only the interactive job types (one per guild); `false`: every other type.
    pub interactive: bool,
    /// At most this many rows.
    pub limit: i64,
    /// `INTERACTIVE_JOB_TYPES`.
    pub interactive_types: Vec<String>,
}

/// The conflict policy of a scheduled job with a dedupe key.
#[derive(Clone)]
pub enum OnConflict {
    /// Reset the live row to this schedule (the default; resets even a RUNNING row).
    Replace,
    /// Leave a live row untouched.
    Keep,
    /// Replace only when the new run is earlier.
    Earliest,
    /// Change only the payload of a live row to `merge(existing, incoming)`, under the row lock.
    Merge(MergeFn),
}

impl std::fmt::Debug for OnConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OnConflict::Replace => "replace",
            OnConflict::Keep => "keep",
            OnConflict::Earliest => "earliest",
            OnConflict::Merge(_) => "merge",
        })
    }
}

/// `merge(existingPayload, incomingPayload)`.
pub type MergeFn = Arc<dyn Fn(&Value, &Value) -> Value + Send + Sync>;

/// `next(livePayload)` of `settleMergedJob`: `None` deletes the row, `Some(payload)` runs it again.
pub type SettleNextFn = Box<dyn FnOnce(&Value) -> Option<Value> + Send>;

/// A job to insert or upsert.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub guild_id: String,
    pub job_type: String,
    pub run_at: DateTime<Utc>,
    pub payload: Value,
    pub dedupe_key: Option<String>,
    pub max_attempts: i32,
}

/// `{ id, runAt }`, what `scheduleJob` resolves with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    pub id: String,
    pub run_at: DateTime<Utc>,
}

/// The fields a settle writes (`prisma.scheduledJob.updateMany({ where: { id, version }, data })`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobUpdate {
    pub status: Option<JobStatus>,
    pub attempts: Option<i32>,
    pub run_at: Option<DateTime<Utc>>,
    /// `Some(None)` writes NULL.
    pub last_error: Option<Option<String>>,
    pub payload: Option<Value>,
    /// `lockedBy: null, lockedUntil: null`.
    pub clear_lock: bool,
    /// `version: { increment: 1 }`.
    pub bump_version: bool,
}

/// The `cancelJobs` filter after validation: every present field must match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancelWhere {
    pub dedupe_key: Option<String>,
    pub guild_id: Option<String>,
    pub types: Option<Vec<String>>,
    pub ids: Option<Vec<String>>,
}

/// `settleMergedJob` outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// `next` returned `None`: the row was deleted.
    Done,
    /// The row went back to PENDING with the new payload.
    Again,
    /// The job was replaced or cancelled meanwhile: nothing was done.
    Gone,
}

/// The scheduler's storage.
#[async_trait]
pub trait JobStore: Send + Sync + 'static {
    /// The claim statement (`claimSql`): marks up to `limit` due rows RUNNING for `owner` and returns them.
    async fn claim(&self, req: &ClaimRequest) -> Result<Vec<ScheduledJob>, DbError>;
    /// `prisma.scheduledJob.create` (no dedupe key).
    async fn insert(&self, job: &NewJob) -> Result<Scheduled, DbError>;
    /// The dedupe transaction of `scheduleJob`: lock the key's row, resolve the conflict, then keep, merge, replace
    /// or insert. A unique violation (two inserters racing on a new key) is reported as such so the caller retries.
    async fn upsert_dedupe(&self, job: &NewJob, on_conflict: &OnConflict) -> Result<Scheduled, DbError>;
    /// `settle`: writes `update` only while the row still has `version`. The rows changed.
    async fn settle(&self, id: &str, version: i32, update: &JobUpdate) -> Result<u64, DbError>;
    /// `deleteMany({ where: { id, version } })`.
    async fn delete_version(&self, id: &str, version: i32) -> Result<u64, DbError>;
    /// `deleteMany({ where })` of `cancelJobs`.
    async fn cancel(&self, filter: &CancelWhere) -> Result<u64, DbError>;
    /// `findUnique({ where: { dedupeKey } })`.
    async fn get(&self, dedupe_key: &str) -> Result<Option<ScheduledJob>, DbError>;
    /// `findMany({ where: { dedupeKey: { in: keys } } })` (one call per chunk).
    async fn get_many(&self, dedupe_keys: &[String]) -> Result<Vec<ScheduledJob>, DbError>;
    /// The `settleMergedJob` transaction.
    async fn settle_merged(&self, id: &str, version: i32, next: SettleNextFn, run_at: DateTime<Utc>) -> Result<SettleOutcome, DbError>;
    /// The hourly GC: deletes finished rows not updated for 7 days.
    async fn gc(&self) -> Result<u64, DbError>;
}

/// Is `status` live (PENDING or RUNNING)?
pub fn is_live(status: JobStatus) -> bool {
    matches!(status, JobStatus::Pending | JobStatus::Running)
}
