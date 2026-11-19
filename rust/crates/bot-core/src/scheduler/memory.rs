//! The in-memory job store (offline tests): the claim statement's rules (ownership, due time, expired leases, the
//! interactive one-per-guild rule, `ORDER BY "runAt"`, `LIMIT`), the dedupe transaction, version-guarded settles,
//! cancel, lookups and GC, on the shared [`Clock`](crate::clock::Clock). It can be made unavailable to test the
//! "database down" paths.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bot_db::{DbError, JobStatus, ScheduledJob};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use super::store::{CancelWhere, ClaimRequest, JobStore, JobUpdate, NewJob, OnConflict, Owned, Scheduled, SettleNextFn, SettleOutcome};
use super::{ConflictAction, resolve_conflict};
use crate::clock::SharedClock;

/// The in-memory store.
pub struct MemoryJobStore {
    clock: SharedClock,
    rows: Mutex<Vec<ScheduledJob>>,
    down: AtomicBool,
    claims: Mutex<Vec<ClaimRequest>>,
}

fn unavailable() -> DbError {
    DbError::Timeout("the in-memory database is unavailable".into())
}

/// `(guild_id >> 22) % count` for a well-formed snowflake (`^[0-9]{1,19}$` that fits a bigint), else `None`.
pub fn shard_of(guild_id: &str, count: u64) -> Option<u64> {
    if guild_id.is_empty() || guild_id.len() > 19 || !guild_id.bytes().all(|b| b.is_ascii_digit()) || count == 0 {
        return None;
    }
    let n: i64 = guild_id.parse().ok()?;
    Some(((n >> 22) as u64) % count)
}

impl MemoryJobStore {
    pub fn new(clock: SharedClock) -> Self {
        Self { clock, rows: Mutex::new(Vec::new()), down: AtomicBool::new(false), claims: Mutex::new(Vec::new()) }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, Vec<ScheduledJob>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check(&self) -> Result<(), DbError> {
        if self.down.load(Ordering::SeqCst) { Err(unavailable()) } else { Ok(()) }
    }

    /// Makes every operation fail (`false`) or work again (`true`).
    pub fn set_available(&self, available: bool) {
        self.down.store(!available, Ordering::SeqCst);
    }

    /// Every row (tests).
    pub fn all(&self) -> Vec<ScheduledJob> {
        self.rows().clone()
    }

    /// The row with this id.
    pub fn by_id(&self, id: &str) -> Option<ScheduledJob> {
        self.rows().iter().find(|r| r.id == id).cloned()
    }

    /// Stores a row as is (a row the JS bot or a crashed process left behind).
    pub fn put(&self, row: ScheduledJob) {
        let mut rows = self.rows();
        rows.retain(|r| r.id != row.id);
        rows.push(row);
    }

    /// Changes a stored row in place (tests: mark it RUNNING with an expired lease, ...).
    pub fn modify(&self, id: &str, f: impl FnOnce(&mut ScheduledJob)) -> bool {
        let mut rows = self.rows();
        match rows.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                f(r);
                true
            }
            None => false,
        }
    }

    /// The claim requests received so far.
    pub fn claims(&self) -> Vec<ClaimRequest> {
        self.claims.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn new_row(&self, job: &NewJob) -> ScheduledJob {
        let now = bot_db::serde_js::truncate_ms(self.now());
        ScheduledJob {
            id: bot_db::new_cuid(),
            guild_id: job.guild_id.clone(),
            r#type: job.job_type.clone(),
            dedupe_key: job.dedupe_key.clone(),
            run_at: bot_db::serde_js::truncate_ms(job.run_at),
            payload: job.payload.clone(),
            status: JobStatus::Pending,
            attempts: 0,
            max_attempts: job.max_attempts,
            last_error: None,
            locked_by: None,
            locked_until: None,
            version: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

fn claimable(r: &ScheduledJob, now: DateTime<Utc>) -> bool {
    r.run_at <= now && (r.status == JobStatus::Pending || (r.status == JobStatus::Running && r.locked_until.is_some_and(|t| t < now)))
}

#[async_trait]
impl JobStore for MemoryJobStore {
    async fn claim(&self, req: &ClaimRequest) -> Result<Vec<ScheduledJob>, DbError> {
        self.check()?;
        self.claims.lock().unwrap_or_else(|e| e.into_inner()).push(req.clone());
        let now = self.now();
        let mut rows = self.rows();
        let is_interactive = |t: &str| req.interactive_types.iter().any(|x| x == t);
        let owned = |g: &str| match &req.owned {
            Owned::All => true,
            Owned::Guilds(ids) => ids.iter().any(|i| i == g),
            Owned::Shards { count, ids } => shard_of(g, *count).is_some_and(|s| ids.contains(&s)),
        };
        let snapshot = rows.clone();
        let mut due: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| owned(&r.guild_id) && is_interactive(&r.r#type) == req.interactive && claimable(r, now))
            .filter(|(_, j)| {
                if !req.interactive {
                    return true;
                }
                let running = snapshot.iter().any(|r| {
                    r.guild_id == j.guild_id
                        && is_interactive(&r.r#type)
                        && r.status == JobStatus::Running
                        && r.locked_until.is_some_and(|t| t >= now)
                        && r.updated_at > now - Duration::seconds(90)
                });
                let earlier = snapshot.iter().any(|e| {
                    e.guild_id == j.guild_id
                        && is_interactive(&e.r#type)
                        && e.id != j.id
                        && claimable(e, now)
                        && (e.run_at, &e.id) < (j.run_at, &j.id)
                });
                !running && !earlier
            })
            .map(|(i, _)| i)
            .collect();
        due.sort_by(|a, b| rows[*a].run_at.cmp(&rows[*b].run_at));
        due.truncate(req.limit.max(0) as usize);
        let mut out = Vec::new();
        for i in due {
            let r = &mut rows[i];
            r.status = JobStatus::Running;
            r.attempts += 1;
            r.locked_by = Some(req.owner.clone());
            r.locked_until = Some(now + Duration::minutes(5));
            r.updated_at = now;
            out.push(r.clone());
        }
        Ok(out)
    }

    async fn insert(&self, job: &NewJob) -> Result<Scheduled, DbError> {
        self.check()?;
        let row = self.new_row(job);
        let out = Scheduled { id: row.id.clone(), run_at: row.run_at };
        self.rows().push(row);
        Ok(out)
    }

    async fn upsert_dedupe(&self, job: &NewJob, on_conflict: &OnConflict) -> Result<Scheduled, DbError> {
        self.check()?;
        let now = self.now();
        let new_row = self.new_row(job);
        let mut rows = self.rows();
        let existing = rows.iter_mut().find(|r| r.dedupe_key.is_some() && r.dedupe_key == job.dedupe_key);
        let action = resolve_conflict(existing.as_ref().map(|e| (e.status, e.run_at)), job.run_at, on_conflict);
        match (action, existing) {
            (ConflictAction::Keep, Some(e)) => Ok(Scheduled { id: e.id.clone(), run_at: e.run_at }),
            (ConflictAction::Merge, Some(e)) => {
                let OnConflict::Merge(merge) = on_conflict else { unreachable!("merge action without a merge fn") };
                let current = if e.payload.is_null() { Value::Object(Default::default()) } else { e.payload.clone() };
                let merged = merge(&current, &job.payload);
                e.payload = if merged.is_null() { Value::Object(Default::default()) } else { merged };
                e.updated_at = now;
                Ok(Scheduled { id: e.id.clone(), run_at: e.run_at })
            }
            (ConflictAction::Replace, Some(e)) => {
                e.guild_id = job.guild_id.clone();
                e.r#type = job.job_type.clone();
                e.run_at = bot_db::serde_js::truncate_ms(job.run_at);
                e.payload = job.payload.clone();
                e.max_attempts = job.max_attempts;
                e.status = JobStatus::Pending;
                e.attempts = 0;
                e.last_error = None;
                e.locked_by = None;
                e.locked_until = None;
                e.version += 1;
                e.updated_at = now;
                Ok(Scheduled { id: e.id.clone(), run_at: e.run_at })
            }
            _ => {
                let out = Scheduled { id: new_row.id.clone(), run_at: new_row.run_at };
                rows.push(new_row);
                Ok(out)
            }
        }
    }

    async fn settle(&self, id: &str, version: i32, update: &JobUpdate) -> Result<u64, DbError> {
        self.check()?;
        let now = self.now();
        let mut rows = self.rows();
        let Some(r) = rows.iter_mut().find(|r| r.id == id && r.version == version) else { return Ok(0) };
        if let Some(s) = update.status {
            r.status = s;
        }
        if let Some(a) = update.attempts {
            r.attempts = a;
        }
        if let Some(t) = update.run_at {
            r.run_at = bot_db::serde_js::truncate_ms(t);
        }
        if let Some(e) = &update.last_error {
            r.last_error = e.clone();
        }
        if let Some(p) = &update.payload {
            r.payload = p.clone();
        }
        if update.clear_lock {
            r.locked_by = None;
            r.locked_until = None;
        }
        if update.bump_version {
            r.version += 1;
        }
        r.updated_at = now;
        Ok(1)
    }

    async fn delete_version(&self, id: &str, version: i32) -> Result<u64, DbError> {
        self.check()?;
        let mut rows = self.rows();
        let before = rows.len();
        rows.retain(|r| !(r.id == id && r.version == version));
        Ok((before - rows.len()) as u64)
    }

    async fn cancel(&self, filter: &CancelWhere) -> Result<u64, DbError> {
        self.check()?;
        let mut rows = self.rows();
        let before = rows.len();
        rows.retain(|r| {
            let hit = filter.dedupe_key.as_ref().is_none_or(|k| r.dedupe_key.as_ref() == Some(k))
                && filter.guild_id.as_ref().is_none_or(|g| &r.guild_id == g)
                && filter.types.as_ref().is_none_or(|t| t.contains(&r.r#type))
                && filter.ids.as_ref().is_none_or(|i| i.contains(&r.id));
            !hit
        });
        Ok((before - rows.len()) as u64)
    }

    async fn get(&self, dedupe_key: &str) -> Result<Option<ScheduledJob>, DbError> {
        self.check()?;
        Ok(self.rows().iter().find(|r| r.dedupe_key.as_deref() == Some(dedupe_key)).cloned())
    }

    async fn get_many(&self, dedupe_keys: &[String]) -> Result<Vec<ScheduledJob>, DbError> {
        self.check()?;
        Ok(self.rows().iter().filter(|r| r.dedupe_key.as_ref().is_some_and(|k| dedupe_keys.contains(k))).cloned().collect())
    }

    async fn settle_merged(&self, id: &str, version: i32, next: SettleNextFn, run_at: DateTime<Utc>) -> Result<SettleOutcome, DbError> {
        self.check()?;
        let now = self.now();
        let mut rows = self.rows();
        let Some(pos) = rows.iter().position(|r| r.id == id) else { return Ok(SettleOutcome::Gone) };
        if rows[pos].version != version {
            return Ok(SettleOutcome::Gone);
        }
        let current = if rows[pos].payload.is_null() { Value::Object(Default::default()) } else { rows[pos].payload.clone() };
        match next(&current) {
            None => {
                rows.remove(pos);
                Ok(SettleOutcome::Done)
            }
            Some(payload) => {
                let r = &mut rows[pos];
                r.status = JobStatus::Pending;
                r.run_at = bot_db::serde_js::truncate_ms(run_at);
                r.attempts = 0;
                r.last_error = None;
                r.locked_by = None;
                r.locked_until = None;
                r.payload = payload;
                r.version += 1;
                r.updated_at = now;
                Ok(SettleOutcome::Again)
            }
        }
    }

    async fn gc(&self) -> Result<u64, DbError> {
        self.check()?;
        let cutoff = self.now() - Duration::days(7);
        let mut rows = self.rows();
        let before = rows.len();
        rows.retain(|r| !(matches!(r.status, JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled) && r.updated_at < cutoff));
        Ok((before - rows.len()) as u64)
    }
}
