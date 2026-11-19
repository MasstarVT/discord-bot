//! scheduler.js: conflict resolution, the handler registry, validation, the slot loop, the interactive loop's claims,
//! getJobsByKeys, and the claim / execute / reschedule / retry / fail / release state machine. The state machine runs
//! twice: on the in-memory store (offline) and, with TEST_DATABASE_URL, on PostgreSQL with the ported SQL. With
//! TEST_REDIS_URL too, jobs scheduled by the JS bot run under Rust and the reverse.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bot_config::JobType;
use bot_core::clock::{SharedClock, SystemClock};
use bot_core::discord::StaticGuilds;
use bot_core::scheduler::{
    CancelJobs, CancelWhere, ClaimRequest, ConflictAction, INTERACTIVE_ACTIVE_WINDOW_MS, INTERACTIVE_IDLE_POLL_INTERVAL_MS, INTERACTIVE_MAX_LATE_MS,
    INTERACTIVE_POLL_INTERVAL_MS, JOB_CONCURRENCY, Job, JobCtx, JobOutcome, JobStore, JobUpdate, MemoryJobStore, NewJob, OnConflict, Owned,
    ScheduleJob, Scheduled, SettleNextFn, SettleOutcome, interactive_poll_delay, job_handler, resolve_conflict, retry_delay_ms,
};
use bot_core::settings::MemorySettingsStore;
use bot_core::{App, PermanentJobError};
use bot_db::{DbError, JobStatus, ScheduledJob};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn at(s: i64) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::seconds(s)
}

#[test]
fn resolve_conflict_table() {
    let early = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc);
    let late = DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z").unwrap().with_timezone(&Utc);
    for mode in [OnConflict::Replace, OnConflict::Keep, OnConflict::Earliest] {
        assert_eq!(resolve_conflict(None, early, &mode), ConflictAction::Insert);
    }
    for status in [JobStatus::Pending, JobStatus::Running] {
        assert_eq!(resolve_conflict(Some((status, late)), early, &OnConflict::Replace), ConflictAction::Replace);
        assert_eq!(resolve_conflict(Some((status, late)), early, &OnConflict::Keep), ConflictAction::Keep);
        assert_eq!(resolve_conflict(Some((status, late)), early, &OnConflict::Earliest), ConflictAction::Replace);
        assert_eq!(resolve_conflict(Some((status, late)), late, &OnConflict::Earliest), ConflictAction::Keep);
        assert_eq!(resolve_conflict(Some((status, early)), late, &OnConflict::Earliest), ConflictAction::Keep);
        let merge = OnConflict::Merge(Arc::new(|a: &Value, _: &Value| a.clone()));
        assert_eq!(resolve_conflict(Some((status, early)), late, &merge), ConflictAction::Merge);
    }
    for status in [JobStatus::Failed, JobStatus::Done, JobStatus::Cancelled] {
        for mode in [OnConflict::Replace, OnConflict::Keep, OnConflict::Earliest] {
            assert_eq!(resolve_conflict(Some((status, early)), late, &mode), ConflictAction::Replace);
        }
    }
    assert_eq!(bot_core::scheduler::LIVE_STATUSES, [JobStatus::Pending, JobStatus::Running]);
}

#[test]
fn back_off_and_poll_delays() {
    assert_eq!(retry_delay_ms(1), 30_000);
    assert_eq!(retry_delay_ms(2), 60_000);
    assert_eq!(retry_delay_ms(8), 3_600_000);
    assert_eq!(retry_delay_ms(0), 30_000);
    const { assert!(INTERACTIVE_POLL_INTERVAL_MS < INTERACTIVE_IDLE_POLL_INTERVAL_MS) };
    const { assert!(INTERACTIVE_IDLE_POLL_INTERVAL_MS * 12 <= 60_000) };
    assert_eq!(interactive_poll_delay(None), INTERACTIVE_IDLE_POLL_INTERVAL_MS, "nothing claimed yet");
    assert_eq!(interactive_poll_delay(Some(0)), INTERACTIVE_POLL_INTERVAL_MS);
    assert_eq!(interactive_poll_delay(Some(INTERACTIVE_ACTIVE_WINDOW_MS - 1)), INTERACTIVE_POLL_INTERVAL_MS);
    assert_eq!(interactive_poll_delay(Some(INTERACTIVE_ACTIVE_WINDOW_MS)), INTERACTIVE_IDLE_POLL_INTERVAL_MS);
    for _ in 0..100 {
        let j = bot_core::scheduler::jitter(5000);
        assert!((4000..=6000).contains(&j));
    }
}

fn memory_app() -> (App, Arc<MemoryJobStore>) {
    let clock: SharedClock = Arc::new(SystemClock);
    let jobs = Arc::new(MemoryJobStore::new(clock.clone()));
    let app = App::builder().clock(clock).job_store(jobs.clone()).settings_store(Arc::new(MemorySettingsStore::new())).build();
    (app, jobs)
}

#[tokio::test]
async fn handler_registry_validation_and_offline_tick() {
    let (app, jobs) = memory_app();
    let s = app.scheduler();
    let h = job_handler(|_, _| async { Ok(JobOutcome::Done) });
    s.register_job_handler(JobType::PollEnd, h.clone()).unwrap();
    s.register_job_handler(JobType::PollEnd, h).unwrap();
    let err = s.register_job_handler(JobType::PollEnd, job_handler(|_, _| async { Ok(JobOutcome::Done) })).unwrap_err();
    assert!(err.contains("already registered"));
    assert!(anyhow::Error::from(PermanentJobError::new("x")).to_string() == "x");

    // Unavailable guilds are never claimed: no claim is issued at all.
    app.set_guild_directory(Arc::new(StaticGuilds { unavailable: vec!["1".into()], shard_count: 1, ..Default::default() }));
    assert_eq!(s.tick(&app, false).await, 0);
    assert!(jobs.claims().is_empty());

    let base = || ScheduleJob::new("1", JobType::PollEnd, Utc::now());
    assert!(s.schedule_job(ScheduleJob { guild_id: String::new(), ..base() }).await.unwrap_err().to_string().contains("needs a guildId"));
    assert!(s.schedule_job(base().merge(|a, _| a.clone())).await.unwrap_err().to_string().contains("onConflict 'merge'"));
    assert!(s.schedule_job(base().max_attempts(0)).await.unwrap_err().to_string().contains("maxAttempts"));
    assert_eq!(s.cancel_jobs(CancelJobs::default()).await, 0, "no filter cancels nothing");
    assert_eq!(s.cancel_jobs(CancelJobs { types: Some(vec![]), ..Default::default() }).await, 0);
    assert_eq!(s.cancel_jobs(CancelJobs { ids: Some(vec![]), ..Default::default() }).await, 0);
    s.stop(50).await;
}

#[tokio::test]
async fn slot_loop_a_slow_job_never_holds_up_the_jobs_due_behind_it() {
    let (app, jobs) = memory_app();
    app.set_guild_directory(Arc::new(StaticGuilds { available: vec!["7".into()], shard_ids: Some(vec![1]), shard_count: 2, ..Default::default() }));
    let total = JOB_CONCURRENCY + 3;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let done = Arc::new(Mutex::new(Vec::<String>::new()));
    let active = Arc::new(Mutex::new((0usize, 0usize)));
    {
        let (gate, done, active) = (gate.clone(), done.clone(), active.clone());
        app.scheduler()
            .register_job_handler(
                JobType::ReminderFire,
                job_handler(move |job: Job, _| {
                    let (gate, done, active) = (gate.clone(), done.clone(), active.clone());
                    async move {
                        {
                            let mut a = active.lock().unwrap();
                            a.0 += 1;
                            a.1 = a.1.max(a.0);
                        }
                        if job.payload["slow"] == json!(true) {
                            let _p = gate.acquire().await.unwrap();
                        } else {
                            tokio::task::yield_now().await;
                        }
                        done.lock().unwrap().push(job.payload["n"].to_string());
                        active.lock().unwrap().0 -= 1;
                        Ok(JobOutcome::Done)
                    }
                }),
            )
            .unwrap();
    }
    for n in 0..total {
        let mut row = new_row("7", "reminder.fire", json!({ "slow": n == 0, "n": n }));
        row.run_at = Utc::now() - chrono::Duration::seconds((total - n) as i64);
        jobs.put(row);
    }
    app.scheduler().mark_started();
    assert_eq!(app.scheduler().fill_slots(&app).await, JOB_CONCURRENCY, "claims exactly the free slots");
    let ok = wait_until(|| done.lock().unwrap().len() == total - 1).await;
    assert!(ok, "every other job ran while the slow one still holds a slot");
    assert!(!done.lock().unwrap().contains(&"0".to_string()));
    assert!(active.lock().unwrap().1 <= JOB_CONCURRENCY, "never more than the concurrency at once");
    let limits: Vec<i64> = jobs.claims().iter().map(|c| c.limit).collect();
    assert!(limits.iter().all(|l| (1..=JOB_CONCURRENCY as i64).contains(l)), "claims never exceed the free slots ({limits:?})");
    assert!(jobs.claims().iter().all(|c| !c.interactive && c.owned == Owned::Guilds(vec!["7".into()])));
    let claims = jobs.claims().len();
    gate.add_permits(1);
    assert!(wait_until(|| done.lock().unwrap().len() == total).await);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(jobs.claims().len(), claims, "no backlog: a finished job leaves new work to the poll");
    app.scheduler().stop(1000).await;
    assert_eq!(app.scheduler().inflight(), 0);
}

async fn wait_until(f: impl Fn() -> bool) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f()
}

fn new_row(guild: &str, job_type: &str, payload: Value) -> ScheduledJob {
    let now = bot_db::now_utc();
    ScheduledJob {
        id: bot_db::new_cuid(),
        guild_id: guild.into(),
        r#type: job_type.into(),
        dedupe_key: None,
        run_at: now,
        payload,
        status: JobStatus::Pending,
        attempts: 0,
        max_attempts: 3,
        last_error: None,
        locked_by: None,
        locked_until: None,
        version: 0,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn interactive_loop_claims_by_shard_without_a_guild_list() {
    let (app, jobs) = memory_app();
    app.set_guild_directory(Arc::new(StaticGuilds {
        available: vec!["111".into()],
        unavailable: vec!["222".into()],
        shard_ids: Some(vec![1, 3]),
        shard_count: 4,
        ..Default::default()
    }));
    assert_eq!(app.scheduler().tick(&app, true).await, 0);
    assert_eq!(jobs.claims().last().unwrap().owned, Owned::Shards { count: 4, ids: vec![1, 3] });
    assert!(jobs.claims().last().unwrap().interactive);
    // One process for every guild: no ownership filter, even with no guild cached.
    app.set_guild_directory(Arc::new(StaticGuilds::default()));
    app.scheduler().tick(&app, true).await;
    assert_eq!(jobs.claims().last().unwrap().owned, Owned::All);
    // The ordinary jobs still go by this shard's available guilds.
    app.set_guild_directory(Arc::new(StaticGuilds {
        available: vec!["111".into()],
        unavailable: vec!["222".into()],
        shard_ids: Some(vec![1]),
        shard_count: 4,
        ..Default::default()
    }));
    app.scheduler().tick(&app, false).await;
    assert_eq!(jobs.claims().last().unwrap().owned, Owned::Guilds(vec!["111".into()]));
    assert_eq!(bot_core::scheduler::shard_of("111", 4), Some(0));
    assert_eq!(bot_core::scheduler::shard_of("abc", 4), None, "a malformed guild id never matches");
}

/// Counts `get_many` calls (getJobsByKeys: one query per 500 keys).
struct CountingStore {
    inner: Arc<MemoryJobStore>,
    calls: Mutex<Vec<usize>>,
    fail: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl JobStore for CountingStore {
    async fn claim(&self, req: &ClaimRequest) -> Result<Vec<ScheduledJob>, DbError> {
        self.inner.claim(req).await
    }
    async fn insert(&self, job: &NewJob) -> Result<Scheduled, DbError> {
        self.inner.insert(job).await
    }
    async fn upsert_dedupe(&self, job: &NewJob, on_conflict: &OnConflict) -> Result<Scheduled, DbError> {
        self.inner.upsert_dedupe(job, on_conflict).await
    }
    async fn settle(&self, id: &str, version: i32, update: &JobUpdate) -> Result<u64, DbError> {
        self.inner.settle(id, version, update).await
    }
    async fn delete_version(&self, id: &str, version: i32) -> Result<u64, DbError> {
        self.inner.delete_version(id, version).await
    }
    async fn cancel(&self, filter: &CancelWhere) -> Result<u64, DbError> {
        self.inner.cancel(filter).await
    }
    async fn get(&self, key: &str) -> Result<Option<ScheduledJob>, DbError> {
        self.inner.get(key).await
    }
    async fn get_many(&self, keys: &[String]) -> Result<Vec<ScheduledJob>, DbError> {
        self.calls.lock().unwrap().push(keys.len());
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DbError::Timeout("db down".into()));
        }
        self.inner.get_many(keys).await
    }
    async fn settle_merged(&self, id: &str, version: i32, next: SettleNextFn, run_at: DateTime<Utc>) -> Result<SettleOutcome, DbError> {
        self.inner.settle_merged(id, version, next, run_at).await
    }
    async fn gc(&self) -> Result<u64, DbError> {
        self.inner.gc().await
    }
}

#[tokio::test]
async fn get_jobs_by_keys_one_query_per_500_keys_a_map_and_none_on_error() {
    let clock: SharedClock = Arc::new(SystemClock);
    let inner = Arc::new(MemoryJobStore::new(clock.clone()));
    for i in (0..501).filter(|i| i % 10 == 0) {
        let mut r = new_row("1", "mod.unban", json!({}));
        r.dedupe_key = Some(format!("k{i}"));
        inner.put(r);
    }
    let store = Arc::new(CountingStore { inner, calls: Mutex::new(Vec::new()), fail: Default::default() });
    let app = App::builder().clock(clock).job_store(store.clone()).build();
    let mut keys: Vec<String> = (0..501).map(|i| format!("k{i}")).collect();
    keys.push("k1".into());
    let jobs = app.scheduler().get_jobs_by_keys(&keys).await.unwrap();
    assert_eq!(*store.calls.lock().unwrap(), vec![500, 1]);
    assert_eq!(jobs.get("k10").unwrap().status, JobStatus::Pending);
    assert!(!jobs.contains_key("k11"));
    assert_eq!(jobs.len(), 51);
    store.calls.lock().unwrap().clear();
    assert!(app.scheduler().get_jobs_by_keys::<String>(&[]).await.unwrap().is_empty());
    assert!(store.calls.lock().unwrap().is_empty(), "no keys, no query");
    store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(app.scheduler().get_jobs_by_keys(&["a"]).await.is_none());
}

// ── The state machine, on both stores ───────────────────────────────────────

struct Env {
    app: App,
    mem: Option<Arc<MemoryJobStore>>,
    pool: Option<bot_db::PgPool>,
    seen: Arc<Mutex<Vec<Value>>>,
    interactive_seen: Arc<Mutex<Vec<String>>>,
}

impl Env {
    fn new(app: App, mem: Option<Arc<MemoryJobStore>>, pool: Option<bot_db::PgPool>) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let interactive_seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        app.scheduler()
            .register_job_handler(
                JobType::ModUnban,
                job_handler(move |job: Job, ctx: JobCtx| {
                    let s = s.clone();
                    async move {
                        s.lock().unwrap().push(json!({ "id": job.id, "attempt": ctx.attempt, "lateByMs": ctx.late_by_ms, "version": job.version }));
                        match job.payload["mode"].as_str() {
                            Some("resched") => Ok(JobOutcome::Reschedule { next_run_at: at(3600), payload: Some(json!({ "mode": "ok", "second": true })) }),
                            Some("throw") => Err(anyhow::anyhow!("{}", "boom ".repeat(1000))),
                            Some("permanent") => Err(PermanentJobError::new("never retry").into()),
                            _ => Ok(JobOutcome::Done),
                        }
                    }
                }),
            )
            .unwrap();
        let i = interactive_seen.clone();
        app.scheduler()
            .register_job_handler(
                JobType::DashboardTask,
                job_handler(move |job: Job, _| {
                    let i = i.clone();
                    async move {
                        i.lock().unwrap().push(job.id);
                        Ok(JobOutcome::Done)
                    }
                }),
            )
            .unwrap();
        Self { app, mem, pool, seen, interactive_seen }
    }

    fn guilds(&self, ids: &[&str], cache_miss: bool) {
        self.app.set_guild_directory(Arc::new(StaticGuilds { available: ids.iter().map(|s| s.to_string()).collect(), cache_miss, shard_count: 1, ..Default::default() }));
    }

    async fn get(&self, key: &str) -> Option<ScheduledJob> {
        self.app.scheduler().get_job(key).await
    }

    /// Sets a row's status, attempts and lease (as a crashed worker would have left it).
    async fn set_lease(&self, id: &str, status: JobStatus, attempts: i32, locked_by: &str, locked_until: DateTime<Utc>) {
        if let Some(m) = &self.mem {
            m.modify(id, |r| {
                r.status = status;
                r.attempts = attempts;
                r.locked_by = Some(locked_by.into());
                r.locked_until = Some(locked_until);
            });
        }
        if let Some(p) = &self.pool {
            sqlx::query(r#"UPDATE "ScheduledJob" SET status = $1::"JobStatus", attempts = $2, "lockedBy" = $3, "lockedUntil" = $4 WHERE id = $5"#)
                .bind(status.as_str())
                .bind(attempts)
                .bind(locked_by)
                .bind(locked_until.naive_utc())
                .bind(id)
                .execute(p)
                .await
                .unwrap();
        }
    }

    /// Marks a row RUNNING for `locked_by` until `locked_until`, last updated at `updated_at`.
    async fn set_running(&self, id: &str, locked_by: &str, locked_until: DateTime<Utc>, updated_at: DateTime<Utc>) {
        if let Some(m) = &self.mem {
            m.modify(id, |r| {
                r.status = JobStatus::Running;
                r.attempts = 1;
                r.locked_by = Some(locked_by.into());
                r.locked_until = Some(locked_until);
                r.updated_at = updated_at;
            });
        }
        if let Some(p) = &self.pool {
            sqlx::query(r#"UPDATE "ScheduledJob" SET status = 'RUNNING', attempts = 1, "lockedBy" = $1, "lockedUntil" = $2, "updatedAt" = $3 WHERE id = $4"#)
                .bind(locked_by)
                .bind(locked_until.naive_utc())
                .bind(updated_at.naive_utc())
                .bind(id)
                .execute(p)
                .await
                .unwrap();
        }
    }

    async fn delete(&self, id: &str) {
        self.app.scheduler().cancel_jobs(CancelJobs { ids: Some(vec![id.to_string()]), ..Default::default() }).await;
    }

    async fn status(&self, key: &str) -> Option<JobStatus> {
        self.get(key).await.map(|r| r.status)
    }

    async fn set_failed(&self, id: &str) {
        if let Some(m) = &self.mem {
            m.modify(id, |r| {
                r.status = JobStatus::Failed;
                r.attempts = 5;
                r.last_error = Some("dead".into());
            });
        }
        if let Some(p) = &self.pool {
            sqlx::query(r#"UPDATE "ScheduledJob" SET status = 'FAILED', attempts = 5, "lastError" = 'dead' WHERE id = $1"#).bind(id).execute(p).await.unwrap();
        }
    }

    async fn count(&self, key: &str) -> usize {
        if let Some(m) = &self.mem {
            return m.all().iter().filter(|r| r.dedupe_key.as_deref() == Some(key)).count();
        }
        let (n,): (i64,) = sqlx::query_as(r#"SELECT count(*) FROM "ScheduledJob" WHERE "dedupeKey" = $1"#).bind(key).fetch_one(self.pool.as_ref().unwrap()).await.unwrap();
        n as usize
    }

    async fn make_finished(&self, gid: &str, key: &str, status: JobStatus, updated: DateTime<Utc>) {
        let s = self.app.scheduler().schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(-8 * 86400)).dedupe_key(key)).await.unwrap();
        if let Some(m) = &self.mem {
            m.modify(&s.id, |r| {
                r.status = status;
                r.updated_at = updated;
            });
        }
        if let Some(p) = &self.pool {
            sqlx::query(r#"UPDATE "ScheduledJob" SET status = $1::"JobStatus", "updatedAt" = $2 WHERE id = $3"#)
                .bind(status.as_str())
                .bind(updated.naive_utc())
                .bind(&s.id)
                .execute(p)
                .await
                .unwrap();
        }
    }
}

async fn schedule(env: &Env, gid: &str, mode: &str, key: Option<&str>, max_attempts: i32) -> Scheduled {
    let mut opts = ScheduleJob::new(gid, JobType::ModUnban, at(-5)).payload(json!({ "mode": mode })).max_attempts(max_attempts);
    if let Some(k) = key {
        opts = opts.dedupe_key(k);
    }
    env.app.scheduler().schedule_job(opts).await.unwrap()
}

async fn dedupe_modes_flow(env: &Env, gid: &str) {
    let s = env.app.scheduler();
    let key = format!("mod.unban:{gid}:u1");
    let a = s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(3600)).dedupe_key(&key).payload(json!({ "mode": "ok" }))).await.unwrap();
    let later = s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(7200)).dedupe_key(&key).on_conflict(OnConflict::Earliest)).await.unwrap();
    assert_eq!(later.id, a.id);
    assert_eq!(later.run_at.timestamp_millis(), a.run_at.timestamp_millis());
    let sooner = bot_db::serde_js::truncate_ms(at(1800));
    let earlier = s
        .schedule_job(ScheduleJob::new(gid, JobType::ModUnban, sooner).dedupe_key(&key).on_conflict(OnConflict::Earliest).payload(json!({ "mode": "resched" })))
        .await
        .unwrap();
    assert_eq!(earlier.id, a.id);
    let row = env.get(&key).await.unwrap();
    assert_eq!(row.version, 1);
    assert_eq!(row.run_at, sooner);
    assert_eq!(row.payload, json!({ "mode": "resched" }));
    s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(10)).dedupe_key(&key).on_conflict(OnConflict::Keep).payload(json!({ "mode": "x" }))).await.unwrap();
    let row = env.get(&key).await.unwrap();
    assert_eq!(row.version, 1);
    assert_eq!(row.payload, json!({ "mode": "resched" }));
    // A FAILED row is re-seeded even by 'keep'.
    env.set_failed(&a.id).await;
    s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(60)).dedupe_key(&key).on_conflict(OnConflict::Keep).payload(json!({ "mode": "ok" }))).await.unwrap();
    let row = env.get(&key).await.unwrap();
    assert_eq!(row.status, JobStatus::Pending);
    assert_eq!(row.attempts, 0);
    assert_eq!(row.last_error, None);
    assert_eq!(row.version, 2);
    // merge: only the payload of a live row changes.
    s.schedule_job(
        ScheduleJob::new(gid, JobType::ModUnban, at(9999)).dedupe_key(&key).payload(json!({ "add": 1 })).merge(|cur, inc| {
            let mut m = cur.as_object().cloned().unwrap_or_default();
            m.insert("merged".into(), inc["add"].clone());
            Value::Object(m)
        }),
    )
    .await
    .unwrap();
    let merged = env.get(&key).await.unwrap();
    assert_eq!(merged.payload, json!({ "mode": "ok", "merged": 1 }));
    assert_eq!(merged.version, 2, "merge never touches the version");
    assert_eq!(merged.run_at, row.run_at, "or the schedule");
    // Without a dedupe key every call inserts.
    let n1 = s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(60))).await.unwrap();
    let n2 = s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(60))).await.unwrap();
    assert_ne!(n1.id, n2.id);
    // Two inserters racing on a new key leave exactly one row.
    let race = format!("mod.unban:{gid}:race");
    let mut tasks = Vec::new();
    for i in 1..=3 {
        let app = env.app.clone();
        let (g, k) = (gid.to_string(), race.clone());
        tasks.push(tokio::spawn(async move { app.scheduler().schedule_job(ScheduleJob::new(g, JobType::ModUnban, at(60 + i)).dedupe_key(k)).await.unwrap() }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(env.count(&race).await, 1);
    // cancelJobs by dedupe key, ids, types and guild.
    assert_eq!(s.cancel_jobs(CancelJobs { dedupe_key: Some(race.clone()), ..Default::default() }).await, 1);
    assert_eq!(s.cancel_jobs(CancelJobs { ids: Some(vec![n1.id.clone()]), ..Default::default() }).await, 1);
    assert_eq!(s.cancel_jobs(CancelJobs { guild_id: Some(gid.into()), types: Some(vec![]), ..Default::default() }).await, 0);
    assert_eq!(s.cancel_jobs(CancelJobs { guild_id: Some(gid.into()), job_type: Some("poll.end".into()), ..Default::default() }).await, 0);
    assert_eq!(s.cancel_jobs(CancelJobs { guild_id: Some(gid.into()), types: Some(vec!["poll.end".into(), "mod.unban".into()]), ..Default::default() }).await, 2);
    s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(60))).await.unwrap();
    assert_eq!(s.cancel_jobs(CancelJobs { guild_id: Some(gid.into()), ..Default::default() }).await, 1, "guildId alone cancels every job");
    assert!(env.get(&key).await.is_none());
}

async fn claim_execute_flow(env: &Env, gid: &str, gid2: &str) {
    let s = env.app.scheduler();
    env.guilds(&[gid], false);
    // success: the handler runs once with a 1-based attempt, then the row is deleted
    env.seen.lock().unwrap().clear();
    let ok = schedule(env, gid, "ok", Some(&format!("{gid}:ok")), 5).await;
    assert_eq!(s.tick(&env.app, false).await, 1);
    {
        let seen = env.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["id"], json!(ok.id));
        assert_eq!(seen[0]["attempt"], json!(1));
        assert!(seen[0]["lateByMs"].as_i64().unwrap() >= 4000);
    }
    assert!(env.get(&format!("{gid}:ok")).await.is_none());
    assert_eq!(s.tick(&env.app, false).await, 0, "nothing left to claim");

    // not yet due, and a guild this process does not have available, are not claimed
    s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(600)).dedupe_key(format!("{gid}:future"))).await.unwrap();
    schedule(env, gid2, "ok", Some(&format!("{gid}:away")), 5).await;
    env.seen.lock().unwrap().clear();
    assert_eq!(s.tick(&env.app, false).await, 0);
    assert!(env.seen.lock().unwrap().is_empty());
    let away = env.get(&format!("{gid}:away")).await.unwrap();
    assert_eq!((away.status, away.attempts), (JobStatus::Pending, 0));

    // reschedule: resets attempts, bumps the version, writes the new payload
    schedule(env, gid, "resched", Some(&format!("{gid}:resched")), 5).await;
    assert_eq!(s.tick(&env.app, false).await, 1);
    let row = env.get(&format!("{gid}:resched")).await.unwrap();
    assert_eq!((row.status, row.attempts, row.version), (JobStatus::Pending, 0, 1));
    assert_eq!(row.payload, json!({ "mode": "ok", "second": true }));
    assert!(row.run_at > at(3000));
    assert_eq!(row.locked_by, None);

    // failure with attempts left: back-off ~30 s, error recorded and truncated
    schedule(env, gid, "throw", Some(&format!("{gid}:retry")), 3).await;
    assert_eq!(s.tick(&env.app, false).await, 1);
    let row = env.get(&format!("{gid}:retry")).await.unwrap();
    assert_eq!((row.status, row.attempts), (JobStatus::Pending, 1));
    assert!(row.run_at > at(20) && row.run_at < at(40), "first back-off is 30 s");
    let err = row.last_error.unwrap();
    assert!(err.starts_with("boom") && err.chars().count() <= 2000);
    assert_eq!(row.locked_by, None);

    // the final attempt fails permanently; a PermanentJobError is not retried
    schedule(env, gid, "throw", Some(&format!("{gid}:last")), 1).await;
    s.tick(&env.app, false).await;
    let row = env.get(&format!("{gid}:last")).await.unwrap();
    assert_eq!((row.status, row.attempts), (JobStatus::Failed, 1));
    schedule(env, gid, "permanent", Some(&format!("{gid}:perm")), 5).await;
    s.tick(&env.app, false).await;
    let row = env.get(&format!("{gid}:perm")).await.unwrap();
    assert_eq!((row.status, row.attempts), (JobStatus::Failed, 1));
    assert_eq!(row.last_error.as_deref(), Some("never retry"));

    // poison guard: a crashed worker used every attempt; the next claim fails the row unexecuted
    let poison = schedule(env, gid, "ok", Some(&format!("{gid}:poison")), 2).await;
    env.set_lease(&poison.id, JobStatus::Running, 2, "dead", at(-60)).await;
    env.seen.lock().unwrap().clear();
    s.tick(&env.app, false).await;
    let row = env.get(&format!("{gid}:poison")).await.unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert!(row.last_error.unwrap().contains("claimed 3 times without completing"));
    assert!(env.seen.lock().unwrap().is_empty(), "the handler never ran");

    // an expired lease is re-claimed; a live one is not
    let stale = schedule(env, gid, "ok", Some(&format!("{gid}:stale")), 5).await;
    env.set_lease(&stale.id, JobStatus::Running, 1, "dead", at(-60)).await;
    let locked = schedule(env, gid, "ok", Some(&format!("{gid}:locked")), 5).await;
    env.set_lease(&locked.id, JobStatus::Running, 1, "alive", at(120)).await;
    env.seen.lock().unwrap().clear();
    assert_eq!(s.tick(&env.app, false).await, 1);
    {
        let seen = env.seen.lock().unwrap();
        assert_eq!(seen[0]["id"], json!(stale.id));
        assert_eq!(seen[0]["attempt"], json!(2));
    }
    assert!(env.get(&format!("{gid}:stale")).await.is_none());
    assert_eq!(env.get(&format!("{gid}:locked")).await.unwrap().status, JobStatus::Running);

    // no handler registered: released without spending an attempt, pushed back ~10 min
    let orphan = s.schedule_job(ScheduleJob::new(gid, JobType::CaptchaTimeout, at(-5)).dedupe_key(format!("{gid}:orphan"))).await.unwrap();
    s.tick(&env.app, false).await;
    let row = env.get(&format!("{gid}:orphan")).await.unwrap();
    assert_eq!(row.id, orphan.id);
    assert_eq!((row.status, row.attempts), (JobStatus::Pending, 0));
    assert!(row.run_at > at(500));

    // the guild is missing from the cache at execution time: released, attempt not spent, runAt unchanged
    let miss = schedule(env, gid, "ok", Some(&format!("{gid}:miss")), 5).await;
    let before = env.get(&format!("{gid}:miss")).await.unwrap().run_at;
    env.seen.lock().unwrap().clear();
    env.guilds(&[gid], true);
    s.tick(&env.app, false).await;
    let row = env.get(&format!("{gid}:miss")).await.unwrap();
    assert_eq!(row.id, miss.id);
    assert_eq!((row.status, row.attempts, row.run_at), (JobStatus::Pending, 0, before));
    assert!(env.seen.lock().unwrap().is_empty());
    env.guilds(&[gid], false);
    s.cancel_jobs(CancelJobs { dedupe_key: Some(format!("{gid}:miss")), ..Default::default() }).await;

    // a job replaced while its handler runs is not deleted by the stale completion (version guard)
    let racing_key = format!("{gid}:racing");
    let (g, k) = (gid.to_string(), racing_key.clone());
    s.register_job_handler(
        JobType::ModExpire,
        job_handler(move |_, ctx: JobCtx| {
            let (g, k) = (g.clone(), k.clone());
            async move {
                ctx.app.scheduler().schedule_job(ScheduleJob::new(g, JobType::ModExpire, at(3600)).dedupe_key(k).payload(json!({ "replaced": true }))).await?;
                Ok(JobOutcome::Done)
            }
        }),
    )
    .unwrap();
    s.schedule_job(ScheduleJob::new(gid, JobType::ModExpire, at(-5)).dedupe_key(&racing_key)).await.unwrap();
    s.tick(&env.app, false).await;
    let row = env.get(&racing_key).await.expect("the replacement survives the old run finishing");
    assert_eq!(row.version, 1);
    assert_eq!(row.payload, json!({ "replaced": true }));

    // settleMergedJob: under the lock, deletes or puts the row back with the live payload
    let mj = s
        .schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(600)).dedupe_key(format!("{gid}:merged")).payload(json!({ "panels": { "a": 1 } })))
        .await
        .unwrap();
    let job = Job { id: mj.id.clone(), guild_id: gid.into(), job_type: "mod.unban".into(), payload: json!({}), run_at: mj.run_at, attempts: 1, max_attempts: 5, dedupe_key: None, version: 0 };
    let outcome = s.settle_merged_job(&job, |live| Some(json!({ "seen": live.clone() })), Some(at(30))).await.unwrap();
    assert_eq!(outcome, SettleOutcome::Again);
    let row = env.get(&format!("{gid}:merged")).await.unwrap();
    assert_eq!(row.payload, json!({ "seen": { "panels": { "a": 1 } } }));
    assert_eq!(row.version, 1);
    assert_eq!(s.settle_merged_job(&job, |_| None, None).await.unwrap(), SettleOutcome::Gone, "the version moved on");
    let job = Job { version: 1, ..job };
    assert_eq!(s.settle_merged_job(&job, |_| None, None).await.unwrap(), SettleOutcome::Done);
    assert!(env.get(&format!("{gid}:merged")).await.is_none());

    s.stop(100).await;
}

async fn interactive_flow(env: &Env, gid: &str) {
    let s = env.app.scheduler();
    env.guilds(&[gid], false);
    let task = |key: &str, secs: i64| ScheduleJob::new(gid, JobType::DashboardTask, at(secs)).dedupe_key(format!("dashboard.task:{gid}:{key}")).max_attempts(1);
    env.interactive_seen.lock().unwrap().clear();
    let due = s.schedule_job(task("due", -2)).await.unwrap();
    assert_eq!(s.tick(&env.app, false).await, 0, "the ordinary loop leaves interactive jobs alone");
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(*env.interactive_seen.lock().unwrap(), vec![due.id.clone()]);
    assert!(env.get(&format!("dashboard.task:{gid}:due")).await.is_none());

    s.schedule_job(ScheduleJob::new(gid, JobType::ModUnban, at(-5)).dedupe_key(format!("{gid}:ordinary")).payload(json!({ "mode": "ok" }))).await.unwrap();
    assert_eq!(s.tick(&env.app, true).await, 0, "the interactive loop leaves ordinary jobs alone");
    assert_eq!(s.tick(&env.app, false).await, 1);

    // One per guild: two due tasks of one guild are claimed one at a time.
    s.schedule_job(task("one", -3)).await.unwrap();
    s.schedule_job(task("two", -2)).await.unwrap();
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(s.tick(&env.app, true).await, 1);

    // Too late: failed, never run.
    env.interactive_seen.lock().unwrap().clear();
    s.schedule_job(task("late", -(INTERACTIVE_MAX_LATE_MS / 1000 + 30))).await.unwrap();
    assert_eq!(s.tick(&env.app, true).await, 1);
    let row = env.get(&format!("dashboard.task:{gid}:late")).await.unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert!(row.last_error.unwrap().contains("waited too long"));
    assert!(env.interactive_seen.lock().unwrap().is_empty());

    // The guild is not in this shard's cache: failed instead of released for later.
    s.schedule_job(task("gone", -1)).await.unwrap();
    env.guilds(&[gid], true);
    assert_eq!(s.tick(&env.app, true).await, 1);
    let row = env.get(&format!("dashboard.task:{gid}:gone")).await.unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert!(row.last_error.unwrap().contains("unavailable"));
    assert!(env.interactive_seen.lock().unwrap().is_empty());
}

async fn gc_flow(env: &Env, gid: &str) {
    let old = at(-8 * 86_400);
    env.make_finished(gid, &format!("{gid}:old-failed"), JobStatus::Failed, old).await;
    env.make_finished(gid, &format!("{gid}:old-pending"), JobStatus::Pending, old).await;
    env.make_finished(gid, &format!("{gid}:new-failed"), JobStatus::Failed, Utc::now()).await;
    assert!(env.app.scheduler().gc().await >= 1);
    assert!(env.get(&format!("{gid}:old-failed")).await.is_none());
    assert!(env.get(&format!("{gid}:old-pending")).await.is_some());
    assert!(env.get(&format!("{gid}:new-failed")).await.is_some());
}

/// sec-dashboard-tasks.test.js: one interactive job per guild per claim, none for a guild with one running, and a row
/// a crashed process left RUNNING does not hold its guild back.
async fn one_per_guild_flow(env: &Env, ga: &str, gb: &str) {
    let s = env.app.scheduler();
    env.app.set_guild_directory(Arc::new(StaticGuilds::available([ga, gb])));
    let task = |g: &str, key: &str, ago: i64| ScheduleJob::new(g, JobType::DashboardTask, at(-ago)).dedupe_key(format!("dashboard.task:{g}:{key}")).max_attempts(1);
    let key = |g: &str, k: &str| format!("dashboard.task:{g}:{k}");
    let a1 = s.schedule_job(task(ga, "a1", 8)).await.unwrap();
    s.schedule_job(task(ga, "a2", 7)).await.unwrap();
    s.schedule_job(task(ga, "a3", 6)).await.unwrap();
    let b1 = s.schedule_job(task(gb, "b1", 5)).await.unwrap();
    env.interactive_seen.lock().unwrap().clear();
    assert_eq!(s.tick(&env.app, true).await, 2, "one per guild");
    let mut started = env.interactive_seen.lock().unwrap().clone();
    started.sort();
    let mut expected = vec![a1.id.clone(), b1.id.clone()];
    expected.sort();
    assert_eq!(started, expected);
    assert_eq!(env.status(&key(ga, "a2")).await, Some(JobStatus::Pending));
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(s.tick(&env.app, true).await, 0);

    // A task of guild A is running (claimed a moment ago by a live loop): the next task of A waits, B does not.
    env.interactive_seen.lock().unwrap().clear();
    let c1 = s.schedule_job(task(ga, "c1", 4)).await.unwrap();
    let c2 = s.schedule_job(task(ga, "c2", 3)).await.unwrap();
    let d1 = s.schedule_job(task(gb, "d1", 2)).await.unwrap();
    env.set_running(&c1.id, "live", at(300), Utc::now()).await;
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(*env.interactive_seen.lock().unwrap(), vec![d1.id.clone()]);
    assert_eq!(env.status(&key(ga, "c2")).await, Some(JobStatus::Pending), "guild A already has a task running");
    env.delete(&c1.id).await;
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(*env.interactive_seen.lock().unwrap(), vec![d1.id.clone(), c2.id.clone()]);

    // A row a crashed process left RUNNING (claimed minutes ago, lock still valid) does not hold the guild back.
    env.interactive_seen.lock().unwrap().clear();
    let e1 = s.schedule_job(task(ga, "e1", 4)).await.unwrap();
    let e2 = s.schedule_job(task(ga, "e2", 3)).await.unwrap();
    env.set_running(&e1.id, "dead", at(240), Utc::now() - chrono::Duration::minutes(10)).await;
    assert_eq!(s.tick(&env.app, true).await, 1);
    assert_eq!(*env.interactive_seen.lock().unwrap(), vec![e2.id.clone()]);
    env.delete(&e1.id).await;
}

/// sec-dashboard-tasks.test.js: merge changes only the payload of a live row (a RUNNING one is not reset), and
/// settleMergedJob ends or requeues under the lock.
async fn merge_running_flow(env: &Env, gid: &str) {
    let s = env.app.scheduler();
    let key = format!("ticket.managersync:{gid}");
    let merge = |cur: &Value, inc: &Value| {
        let mut roles: Vec<Value> = cur["roles"].as_array().cloned().unwrap_or_default();
        for r in inc["roles"].as_array().cloned().unwrap_or_default() {
            if !roles.contains(&r) {
                roles.push(r);
            }
        }
        json!({ "roles": roles })
    };
    let sync = |role: &str| {
        ScheduleJob::new(gid, JobType::TicketManagersync, Utc::now()).dedupe_key(&key).max_attempts(3).payload(json!({ "roles": [role] })).merge(merge)
    };
    let first = s.schedule_job(sync("r1")).await.unwrap();
    let again = s.schedule_job(sync("r2")).await.unwrap();
    assert_eq!(again.id, first.id, "one row per guild");
    assert_eq!(env.get(&key).await.unwrap().payload, json!({ "roles": ["r1", "r2"] }));
    env.set_running(&first.id, "live", at(300), Utc::now()).await;
    let running = env.get(&key).await.unwrap();
    s.schedule_job(sync("r3")).await.unwrap();
    let after = env.get(&key).await.unwrap();
    assert_eq!(after.status, JobStatus::Running);
    assert_eq!(after.version, running.version);
    assert_eq!(after.attempts, 1);
    assert_eq!(after.locked_by.as_deref(), Some("live"));
    assert_eq!(after.payload, json!({ "roles": ["r1", "r2", "r3"] }));
    let job = Job {
        id: first.id.clone(),
        guild_id: gid.into(),
        job_type: "ticket.managersync".into(),
        payload: json!({}),
        run_at: Utc::now(),
        attempts: 1,
        max_attempts: 3,
        dedupe_key: Some(key.clone()),
        version: after.version,
    };
    assert_eq!(s.settle_merged_job(&job, |live| Some(live.clone()), None).await.unwrap(), SettleOutcome::Again);
    let row = env.get(&key).await.unwrap();
    assert_eq!((row.status, row.version, row.attempts), (JobStatus::Pending, after.version + 1, 0));
    assert_eq!(row.locked_by, None);
    assert_eq!(s.settle_merged_job(&job, |_| None, None).await.unwrap(), SettleOutcome::Gone);
    assert!(env.get(&key).await.is_some());
    let job = Job { version: row.version, ..job };
    assert_eq!(s.settle_merged_job(&job, |_| None, None).await.unwrap(), SettleOutcome::Done);
    assert!(env.get(&key).await.is_none());
}

#[tokio::test]
async fn memory_one_interactive_job_per_guild() {
    one_per_guild_flow(&memory_env(), "9640000000000001", "9640000000000002").await;
}

#[tokio::test]
async fn memory_merge_changes_only_the_payload_of_a_running_row() {
    merge_running_flow(&memory_env(), "9640000000000003").await;
}

#[tokio::test]
async fn db_one_interactive_job_per_guild() {
    let Some(url) = common::database_url() else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9640000000000001", "9640000000000002"];
    let env = db_env(&url, &g).await;
    one_per_guild_flow(&env, g[0], g[1]).await;
    db_cleanup(&env, &g).await;
}

#[tokio::test]
async fn db_merge_changes_only_the_payload_of_a_running_row() {
    let Some(url) = common::database_url() else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9640000000000003"];
    let env = db_env(&url, &g).await;
    merge_running_flow(&env, g[0]).await;
    db_cleanup(&env, &g).await;
}

#[tokio::test]
async fn a_job_handler_that_panics_before_returning_its_future_fails_the_job() {
    let env = memory_env();
    env.guilds(&["9550000000000701"], false);
    let fail: Option<u8> = std::hint::black_box(None);
    env.app
        .scheduler()
        .register_job_handler(
            JobType::PollEnd,
            Arc::new(move |_job, _ctx| -> bot_core::BoxFuture<bot_core::BotResult<JobOutcome>> {
                let _x = fail.expect("unwrap before the async block");
                Box::pin(async { Ok(JobOutcome::Done) })
            }),
        )
        .unwrap();
    let key = "poll.end:panics";
    env.app.scheduler().schedule_job(ScheduleJob::new("9550000000000701", JobType::PollEnd, at(-1)).dedupe_key(key).max_attempts(1)).await.unwrap();
    assert_eq!(env.app.scheduler().tick(&env.app, false).await, 1);
    let row = env.get(key).await.unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert!(row.last_error.unwrap().contains("unwrap before the async block"));
}

fn memory_env() -> Env {
    let (app, jobs) = memory_app();
    Env::new(app, Some(jobs), None)
}

#[tokio::test]
async fn memory_dedupe_modes_races_and_cancel() {
    dedupe_modes_flow(&memory_env(), "9550000000000201").await;
}

#[tokio::test]
async fn memory_claim_execute_reschedule_retry_fail_and_release() {
    claim_execute_flow(&memory_env(), "9550000000000301", "9550000000000302").await;
}

#[tokio::test]
async fn memory_interactive_jobs_have_their_own_loop_and_are_failed_rather_than_run_late() {
    interactive_flow(&memory_env(), "9550000000000311").await;
}

#[tokio::test]
async fn memory_gc_removes_only_old_finished_jobs() {
    gc_flow(&memory_env(), "9550000000000401").await;
}

// ── PostgreSQL ────────────────────────────────────────────────────────────────

/// The DB scheduler tests share one database; the unsharded interactive claim takes any guild's due job.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn db_env(url: &str, guilds: &[&str]) -> Env {
    let pool = common::pool(url).await;
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = ANY($1)"#).bind(guilds.iter().map(|s| s.to_string()).collect::<Vec<_>>()).execute(&pool).await.unwrap();
    // Interactive jobs left by an earlier failed run would be claimed by the unsharded interactive loop.
    sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE type = 'dashboard.task'"#).execute(&pool).await.unwrap();
    let app = App::builder().postgres(pool.clone()).build();
    Env::new(app, None, Some(pool))
}

async fn db_cleanup(env: &Env, guilds: &[&str]) {
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = ANY($1)"#)
        .bind(guilds.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .execute(env.pool.as_ref().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn db_dedupe_modes_races_and_cancel() {
    let Some(url) = common::database_url() else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9550000000000201"];
    let env = db_env(&url, &g).await;
    dedupe_modes_flow(&env, g[0]).await;
    db_cleanup(&env, &g).await;
}

#[tokio::test]
async fn db_claim_execute_reschedule_retry_fail_and_release() {
    let Some(url) = common::database_url() else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9550000000000301", "9550000000000302"];
    let env = db_env(&url, &g).await;
    claim_execute_flow(&env, g[0], g[1]).await;
    db_cleanup(&env, &g).await;
}

#[tokio::test]
async fn db_interactive_jobs_have_their_own_loop_and_are_failed_rather_than_run_late() {
    let Some(url) = common::database_url() else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9550000000000311"];
    let env = db_env(&url, &g).await;
    interactive_flow(&env, g[0]).await;
    db_cleanup(&env, &g).await;
}

#[tokio::test]
async fn db_gc_removes_only_old_finished_jobs() {
    let Some(url) = common::database_url() else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9550000000000401"];
    let env = db_env(&url, &g).await;
    gc_flow(&env, g[0]).await;
    db_cleanup(&env, &g).await;
}

#[tokio::test]
async fn db_every_raw_scheduler_statement_executes() {
    let Some(url) = common::database_url() else { return };
    let pool = common::pool(&url).await;
    let mut tx = pool.begin().await.unwrap();
    let types: Vec<String> = vec!["dashboard.task".into()];
    for (name, sql) in bot_core::scheduler::raw_sql() {
        assert!(!sql.replace("now() AT TIME ZONE 'UTC'", "").contains("now()"), "{name}: bare now()");
        let q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()));
        let r = match name {
            "scheduler.claim" => q.bind("0:0:0").bind(&types).bind(25i64).bind(vec!["0".to_string()]).execute(&mut *tx).await,
            "scheduler.claimInteractive" => q.bind("0:0:0").bind(&types).bind(25i64).bind(2i64).bind(vec![0i64]).execute(&mut *tx).await,
            "scheduler.claimInteractiveUnsharded" => q.bind("0:0:0").bind(&types).bind(25i64).execute(&mut *tx).await,
            "scheduler.gc" => q.execute(&mut *tx).await,
            _ => q.bind("sample").execute(&mut *tx).await,
        };
        r.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    tx.rollback().await.unwrap();
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn jobs_scheduled_by_the_js_bot_run_under_rust_and_the_reverse() {
    let (Some(db), Some(_redis)) = (common::database_url(), common::redis_url()) else { return };
    let _l = DB_LOCK.lock().await;
    let g = ["9550000000000601"];
    let env = db_env(&db, &g).await;
    let gid = g[0];
    env.guilds(&[gid], false);

    // JS schedules a reminder-style job with a payload, a dedupe key and maxAttempts.
    let key = format!("reminder:{gid}-js");
    let payload = json!({ "reminderId": "r1", "text": "ünïcödé ✨ \"q\" <@1>", "nested": { "list": [1, 2.5, null] } });
    let opts = json!({ "type": "reminder.fire", "runAtMs": Utc::now().timestamp_millis() - 2000, "payload": payload, "dedupeKey": key, "maxAttempts": 3 });
    let res = common::run_js(&["schedule", gid, &opts.to_string()]);
    assert!(res.get("error").is_none(), "{res}");
    let got = Arc::new(Mutex::new(None::<Job>));
    let g2 = got.clone();
    env.app
        .scheduler()
        .register_job_handler(
            JobType::ReminderFire,
            job_handler(move |job: Job, _| {
                let g2 = g2.clone();
                async move {
                    *g2.lock().unwrap() = Some(job);
                    Ok(JobOutcome::Reschedule { next_run_at: at(3600), payload: Some(json!({ "reminderId": "r1", "fired": 1 })) })
                }
            }),
        )
        .unwrap();
    assert_eq!(env.app.scheduler().tick(&env.app, false).await, 1);
    let job = got.lock().unwrap().clone().expect("the JS job ran under Rust");
    assert_eq!(job.payload, payload);
    assert_eq!(job.max_attempts, 3);
    assert_eq!(job.dedupe_key.as_deref(), Some(key.as_str()));
    assert_eq!(job.id, res["id"].as_str().unwrap());

    // The JS reads what Rust wrote back (the reschedule).
    let js = common::run_js(&["get-job", &key]);
    assert_eq!(js["status"], json!("PENDING"));
    assert_eq!(js["version"], json!(1));
    assert_eq!(js["attempts"], json!(0));
    assert_eq!(js["payload"], json!({ "reminderId": "r1", "fired": 1 }));
    assert_eq!(js["lockedBy"], Value::Null);

    // Rust schedules; the JS reads the row exactly.
    let rkey = format!("giveaway.end:{gid}-rust");
    let run_at = bot_db::serde_js::truncate_ms(at(120));
    env.app
        .scheduler()
        .schedule_job(ScheduleJob::new(gid, JobType::GiveawayEnd, run_at).dedupe_key(&rkey).payload(json!({ "giveawayId": "g1", "n": 3 })).max_attempts(4))
        .await
        .unwrap();
    let js = common::run_js(&["get-job", &rkey]);
    assert_eq!(js["type"], json!("giveaway.end"));
    assert_eq!(js["guildId"], json!(gid));
    assert_eq!(js["payload"], json!({ "giveawayId": "g1", "n": 3 }));
    assert_eq!(js["maxAttempts"], json!(4));
    assert_eq!(js["status"], json!("PENDING"));
    assert_eq!(js["runAt"], json!(bot_db::format_js_date(&run_at)));
    assert!(bot_db::is_cuid(js["id"].as_str().unwrap()), "a Prisma cuid");
    db_cleanup(&env, &g).await;
}

#[test]
fn owned_variants_cover_the_js_owned_sql() {
    let guilds = bot_core::scheduler::claim_sql(&Owned::Guilds(vec![]), false);
    assert!(guilds.contains(r#""guildId" = ANY($4::text[])"#) && guilds.contains("NOT (type = ANY"));
    let shards = bot_core::scheduler::claim_sql(&Owned::Shards { count: 2, ids: vec![0] }, true);
    assert!(shards.contains(">> 22") && shards.contains("NOT EXISTS"));
    let all = bot_core::scheduler::claim_sql(&Owned::All, true);
    assert!(!all.contains(">> 22") && !all.contains(r#""guildId" = ANY"#));
    let _: HashMap<(), ()> = HashMap::new();
}
