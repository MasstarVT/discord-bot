//! The PostgreSQL job store: `scheduler.js`'s raw SQL statement for statement, and its Prisma calls as the SQL Prisma
//! sends (`@updatedAt` and `@default(cuid())` filled in here, `version: { increment: 1 }` as `"version" + 1`).
//!
//! Rows are read with `status::text` (the enum is parsed in Rust), so decoding never depends on the enum type's OID.

use async_trait::async_trait;
use bot_db::{DbError, JobStatus, PgPool, ScheduledJob, TxOptions};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};

use super::store::{CancelWhere, ClaimRequest, JobStore, JobUpdate, NewJob, OnConflict, Owned, Scheduled, SettleNextFn, SettleOutcome};
use super::{ConflictAction, resolve_conflict};

const NOW: &str = "(now() AT TIME ZONE 'UTC')";

/// The columns every job read returns (`j.*`, with the status as text).
pub const JOB_COLUMNS: &str = r#"j."id", j."guildId", j."type", j."dedupeKey", j."runAt", j."payload", j."status"::text AS "status", j."attempts", j."maxAttempts", j."lastError", j."lockedBy", j."lockedUntil", j."version", j."createdAt", j."updatedAt""#;

/// `ownedSql`: `$4`/`$5` are the owned parameters.
fn owned_sql(owned: &Owned) -> &'static str {
    match owned {
        Owned::Guilds(_) => r#""guildId" = ANY($4::text[])"#,
        Owned::All => "TRUE",
        Owned::Shards { .. } => r#"(CASE WHEN "guildId" ~ '^[0-9]{1,19}$' THEN ("guildId"::bigint >> 22) % $4::bigint END) = ANY($5::bigint[])"#,
    }
}

/// `onePerGuildSql` (`$2` is the interactive type list).
pub const ONE_PER_GUILD_SQL: &str = r#"
      AND NOT EXISTS (
        SELECT 1 FROM "ScheduledJob" r
        WHERE r."guildId" = j."guildId" AND r.type = ANY($2::text[])
          AND r.status = 'RUNNING' AND r."lockedUntil" >= (now() AT TIME ZONE 'UTC') AND r."updatedAt" > (now() AT TIME ZONE 'UTC') - interval '90 seconds')
      AND NOT EXISTS (
        SELECT 1 FROM "ScheduledJob" e
        WHERE e."guildId" = j."guildId" AND e.type = ANY($2::text[]) AND e.id <> j.id
          AND e."runAt" <= (now() AT TIME ZONE 'UTC') AND (e.status = 'PENDING' OR (e.status = 'RUNNING' AND e."lockedUntil" < (now() AT TIME ZONE 'UTC')))
          AND (e."runAt", e.id) < (j."runAt", j.id))"#;

/// `claimSql(owned, owner, { interactive, limit })`. Parameters: `$1` owner, `$2` the interactive types, `$3` the
/// limit, `$4`/`$5` the owned filter.
pub fn claim_sql(owned: &Owned, interactive: bool) -> String {
    let types = if interactive { "type = ANY($2::text[])" } else { "NOT (type = ANY($2::text[]))" };
    let one = if interactive { ONE_PER_GUILD_SQL } else { "" };
    format!(
        r#"
  WITH due AS (
    SELECT j.id FROM "ScheduledJob" j
    WHERE {owned}
      AND {types}
      AND "runAt" <= {NOW}
      AND (status = 'PENDING' OR (status = 'RUNNING' AND "lockedUntil" < {NOW})){one}
    ORDER BY "runAt" ASC
    LIMIT $3
    FOR UPDATE SKIP LOCKED
  )
  UPDATE "ScheduledJob" j
  SET status = 'RUNNING', attempts = j.attempts + 1, "lockedBy" = $1,
      "lockedUntil" = {NOW} + interval '5 minutes', "updatedAt" = {NOW}
  FROM due WHERE j.id = due.id
  RETURNING {JOB_COLUMNS}"#,
        owned = owned_sql(owned),
    )
}

/// `lockDedupeSql(dedupeKey)`.
pub const LOCK_DEDUPE_SQL: &str = r#"
  SELECT id, "runAt", status::text AS status, payload FROM "ScheduledJob" WHERE "dedupeKey" = $1 FOR UPDATE"#;

/// `lockJobSql(id)`.
pub const LOCK_JOB_SQL: &str = r#"
  SELECT id, version, payload FROM "ScheduledJob" WHERE id = $1 FOR UPDATE"#;

/// `gcSql()`.
pub const GC_SQL: &str = r#"
  DELETE FROM "ScheduledJob"
  WHERE status IN ('DONE','FAILED','CANCELLED') AND "updatedAt" < (now() AT TIME ZONE 'UTC') - interval '7 days'"#;

/// `RAW_SQL`: each raw statement with sample parameters, for the statement tests (name, SQL, kind of sample binds).
pub fn raw_sql() -> Vec<(&'static str, String)> {
    vec![
        ("scheduler.claim", claim_sql(&Owned::Guilds(vec!["0".into()]), false)),
        ("scheduler.claimInteractive", claim_sql(&Owned::Shards { count: 2, ids: vec![0] }, true)),
        ("scheduler.claimInteractiveUnsharded", claim_sql(&Owned::All, true)),
        ("scheduler.lockDedupe", LOCK_DEDUPE_SQL.to_string()),
        ("scheduler.lockJob", LOCK_JOB_SQL.to_string()),
        ("scheduler.gc", GC_SQL.to_string()),
    ]
}

fn ts(dt: &DateTime<Utc>) -> NaiveDateTime {
    bot_db::serde_js::truncate_ms(*dt).naive_utc()
}

fn parse_status(s: &str) -> Result<JobStatus, sqlx::Error> {
    JobStatus::ALL.iter().copied().find(|v| v.as_str() == s).ok_or_else(|| sqlx::Error::Decode(format!("unknown JobStatus {s:?}").into()))
}

/// Decodes a row selected with [`JOB_COLUMNS`].
pub fn job_from_row(row: &PgRow) -> Result<ScheduledJob, sqlx::Error> {
    Ok(ScheduledJob {
        id: row.try_get("id")?,
        guild_id: row.try_get("guildId")?,
        r#type: row.try_get("type")?,
        dedupe_key: row.try_get("dedupeKey")?,
        run_at: row.try_get::<NaiveDateTime, _>("runAt")?.and_utc(),
        payload: row.try_get("payload")?,
        status: parse_status(row.try_get::<&str, _>("status")?)?,
        attempts: row.try_get("attempts")?,
        max_attempts: row.try_get("maxAttempts")?,
        last_error: row.try_get("lastError")?,
        locked_by: row.try_get("lockedBy")?,
        locked_until: row.try_get::<Option<NaiveDateTime>, _>("lockedUntil")?.map(|d| d.and_utc()),
        version: row.try_get("version")?,
        created_at: row.try_get::<NaiveDateTime, _>("createdAt")?.and_utc(),
        updated_at: row.try_get::<NaiveDateTime, _>("updatedAt")?.and_utc(),
    })
}

fn select_jobs(where_sql: &str) -> String {
    format!(r#"SELECT {JOB_COLUMNS} FROM "ScheduledJob" j WHERE {where_sql}"#)
}

/// The PostgreSQL store.
#[derive(Clone)]
pub struct PgJobStore {
    pool: PgPool,
}

impl PgJobStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

async fn insert_on(conn: &mut PgConnection, job: &NewJob) -> Result<Scheduled, DbError> {
    let now = ts(&bot_db::now_utc());
    let row = sqlx::query(
        r#"INSERT INTO "ScheduledJob" ("id", "guildId", "type", "dedupeKey", "runAt", "payload", "maxAttempts", "createdAt", "updatedAt")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) RETURNING "id", "runAt""#,
    )
    .bind(bot_db::new_cuid())
    .bind(&job.guild_id)
    .bind(&job.job_type)
    .bind(&job.dedupe_key)
    .bind(ts(&job.run_at))
    .bind(&job.payload)
    .bind(job.max_attempts)
    .bind(now)
    .fetch_one(conn)
    .await?;
    Ok(Scheduled { id: row.try_get("id")?, run_at: row.try_get::<NaiveDateTime, _>("runAt")?.and_utc() })
}

#[async_trait]
impl JobStore for PgJobStore {
    async fn claim(&self, req: &ClaimRequest) -> Result<Vec<ScheduledJob>, DbError> {
        let sql = claim_sql(&req.owned, req.interactive);
        let mut q = sqlx::query(AssertSqlSafe(sql)).bind(&req.owner).bind(&req.interactive_types).bind(req.limit);
        match &req.owned {
            Owned::Guilds(ids) => q = q.bind(ids),
            Owned::Shards { count, ids } => {
                q = q.bind(*count as i64).bind(ids.iter().map(|i| *i as i64).collect::<Vec<i64>>());
            }
            Owned::All => {}
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.iter().map(job_from_row).collect::<Result<_, _>>().map_err(DbError::from)
    }

    async fn insert(&self, job: &NewJob) -> Result<Scheduled, DbError> {
        let mut conn = self.pool.acquire().await?;
        insert_on(&mut conn, job).await
    }

    async fn upsert_dedupe(&self, job: &NewJob, on_conflict: &OnConflict) -> Result<Scheduled, DbError> {
        let job = job.clone();
        let on_conflict = on_conflict.clone();
        bot_db::transaction(&self.pool, TxOptions::default(), move |tx| {
            Box::pin(async move {
                let key = job.dedupe_key.clone().unwrap_or_default();
                let existing = sqlx::query(LOCK_DEDUPE_SQL).bind(&key).fetch_optional(&mut *tx).await?;
                let existing = match existing {
                    Some(r) => Some((
                        r.try_get::<String, _>("id")?,
                        r.try_get::<NaiveDateTime, _>("runAt")?.and_utc(),
                        parse_status(r.try_get::<&str, _>("status")?)?,
                        r.try_get::<Option<Value>, _>("payload")?,
                    )),
                    None => None,
                };
                let action = resolve_conflict(existing.as_ref().map(|e| (e.2, e.1)), job.run_at, &on_conflict);
                match (action, existing) {
                    (ConflictAction::Keep, Some((id, run_at, _, _))) => Ok(Scheduled { id, run_at }),
                    (ConflictAction::Merge, Some((id, _, _, payload))) => {
                        let OnConflict::Merge(merge) = &on_conflict else { unreachable!("merge action without a merge fn") };
                        let current = payload.filter(|p| !p.is_null()).unwrap_or_else(|| Value::Object(Default::default()));
                        let merged = merge(&current, &job.payload);
                        let merged = if merged.is_null() { Value::Object(Default::default()) } else { merged };
                        let row = sqlx::query(r#"UPDATE "ScheduledJob" SET "payload" = $1, "updatedAt" = $2 WHERE "id" = $3 RETURNING "id", "runAt""#)
                            .bind(&merged)
                            .bind(ts(&bot_db::now_utc()))
                            .bind(&id)
                            .fetch_one(&mut *tx)
                            .await?;
                        Ok(Scheduled { id: row.try_get("id")?, run_at: row.try_get::<NaiveDateTime, _>("runAt")?.and_utc() })
                    }
                    (ConflictAction::Replace, Some((id, _, _, _))) => {
                        let row = sqlx::query(
                            r#"UPDATE "ScheduledJob" SET "guildId" = $1, "type" = $2, "runAt" = $3, "payload" = $4, "maxAttempts" = $5,
                               "status" = 'PENDING', "attempts" = 0, "lastError" = NULL, "lockedBy" = NULL, "lockedUntil" = NULL,
                               "version" = "version" + 1, "updatedAt" = $6
                               WHERE "id" = $7 RETURNING "id", "runAt""#,
                        )
                        .bind(&job.guild_id)
                        .bind(&job.job_type)
                        .bind(ts(&job.run_at))
                        .bind(&job.payload)
                        .bind(job.max_attempts)
                        .bind(ts(&bot_db::now_utc()))
                        .bind(&id)
                        .fetch_one(&mut *tx)
                        .await?;
                        Ok(Scheduled { id: row.try_get("id")?, run_at: row.try_get::<NaiveDateTime, _>("runAt")?.and_utc() })
                    }
                    _ => insert_on(tx, &job).await,
                }
            })
        })
        .await
    }

    async fn settle(&self, id: &str, version: i32, update: &JobUpdate) -> Result<u64, DbError> {
        let mut sets: Vec<String> = Vec::new();
        let mut n = 2; // $1 id, $2 version
        let mut next = || {
            n += 1;
            n
        };
        let status_p = update.status.map(|_| next());
        let attempts_p = update.attempts.map(|_| next());
        let run_at_p = update.run_at.map(|_| next());
        let last_error_p = update.last_error.as_ref().map(|_| next());
        let payload_p = update.payload.as_ref().map(|_| next());
        let updated_p = next();
        if let Some(p) = status_p {
            sets.push(format!(r#""status" = ${p}::"JobStatus""#));
        }
        if let Some(p) = attempts_p {
            sets.push(format!(r#""attempts" = ${p}"#));
        }
        if let Some(p) = run_at_p {
            sets.push(format!(r#""runAt" = ${p}"#));
        }
        if let Some(p) = last_error_p {
            sets.push(format!(r#""lastError" = ${p}"#));
        }
        if let Some(p) = payload_p {
            sets.push(format!(r#""payload" = ${p}"#));
        }
        if update.clear_lock {
            sets.push(r#""lockedBy" = NULL, "lockedUntil" = NULL"#.into());
        }
        if update.bump_version {
            sets.push(r#""version" = "version" + 1"#.into());
        }
        sets.push(format!(r#""updatedAt" = ${updated_p}"#));
        let sql = format!(r#"UPDATE "ScheduledJob" SET {} WHERE "id" = $1 AND "version" = $2"#, sets.join(", "));
        let mut q = sqlx::query(AssertSqlSafe(sql)).bind(id).bind(version);
        if let Some(s) = update.status {
            q = q.bind(s.as_str());
        }
        if let Some(a) = update.attempts {
            q = q.bind(a);
        }
        if let Some(r) = &update.run_at {
            q = q.bind(ts(r));
        }
        if let Some(e) = &update.last_error {
            q = q.bind(e.clone());
        }
        if let Some(p) = &update.payload {
            q = q.bind(p);
        }
        q = q.bind(ts(&bot_db::now_utc()));
        Ok(q.execute(&self.pool).await?.rows_affected())
    }

    async fn delete_version(&self, id: &str, version: i32) -> Result<u64, DbError> {
        Ok(sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "id" = $1 AND "version" = $2"#).bind(id).bind(version).execute(&self.pool).await?.rows_affected())
    }

    async fn cancel(&self, filter: &CancelWhere) -> Result<u64, DbError> {
        let mut conds = Vec::new();
        let mut n = 0;
        if filter.dedupe_key.is_some() {
            n += 1;
            conds.push(format!(r#""dedupeKey" = ${n}"#));
        }
        if filter.guild_id.is_some() {
            n += 1;
            conds.push(format!(r#""guildId" = ${n}"#));
        }
        if filter.types.is_some() {
            n += 1;
            conds.push(format!(r#""type" = ANY(${n}::text[])"#));
        }
        if filter.ids.is_some() {
            n += 1;
            conds.push(format!(r#""id" = ANY(${n}::text[])"#));
        }
        if conds.is_empty() {
            return Err(DbError::EmptyWhere("DELETE"));
        }
        let sql = format!(r#"DELETE FROM "ScheduledJob" WHERE {}"#, conds.join(" AND "));
        let mut q = sqlx::query(AssertSqlSafe(sql));
        if let Some(k) = &filter.dedupe_key {
            q = q.bind(k);
        }
        if let Some(g) = &filter.guild_id {
            q = q.bind(g);
        }
        if let Some(t) = &filter.types {
            q = q.bind(t);
        }
        if let Some(i) = &filter.ids {
            q = q.bind(i);
        }
        Ok(q.execute(&self.pool).await?.rows_affected())
    }

    async fn get(&self, dedupe_key: &str) -> Result<Option<ScheduledJob>, DbError> {
        let row = sqlx::query(AssertSqlSafe(select_jobs(r#"j."dedupeKey" = $1"#))).bind(dedupe_key).fetch_optional(&self.pool).await?;
        row.as_ref().map(job_from_row).transpose().map_err(DbError::from)
    }

    async fn get_many(&self, dedupe_keys: &[String]) -> Result<Vec<ScheduledJob>, DbError> {
        let rows = sqlx::query(AssertSqlSafe(select_jobs(r#"j."dedupeKey" = ANY($1::text[])"#))).bind(dedupe_keys).fetch_all(&self.pool).await?;
        rows.iter().map(job_from_row).collect::<Result<_, _>>().map_err(DbError::from)
    }

    async fn settle_merged(&self, id: &str, version: i32, next: SettleNextFn, run_at: DateTime<Utc>) -> Result<SettleOutcome, DbError> {
        let id = id.to_string();
        bot_db::transaction(&self.pool, TxOptions::new(2_000, 10_000), move |tx| {
            Box::pin(async move {
                let live = sqlx::query(LOCK_JOB_SQL).bind(&id).fetch_optional(&mut *tx).await?;
                let Some(live) = live else { return Ok(SettleOutcome::Gone) };
                if live.try_get::<i32, _>("version")? != version {
                    return Ok(SettleOutcome::Gone);
                }
                let current = live.try_get::<Option<Value>, _>("payload")?.filter(|p| !p.is_null()).unwrap_or_else(|| Value::Object(Default::default()));
                match next(&current) {
                    None => {
                        sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "id" = $1"#).bind(&id).execute(&mut *tx).await?;
                        Ok(SettleOutcome::Done)
                    }
                    Some(payload) => {
                        sqlx::query(
                            r#"UPDATE "ScheduledJob" SET "status" = 'PENDING', "runAt" = $1, "attempts" = 0, "lastError" = NULL,
                               "lockedBy" = NULL, "lockedUntil" = NULL, "payload" = $2, "version" = "version" + 1, "updatedAt" = $3
                               WHERE "id" = $4"#,
                        )
                        .bind(ts(&run_at))
                        .bind(&payload)
                        .bind(ts(&bot_db::now_utc()))
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                        Ok(SettleOutcome::Again)
                    }
                }
            })
        })
        .await
    }

    async fn gc(&self) -> Result<u64, DbError> {
        Ok(sqlx::query(GC_SQL).execute(&self.pool).await?.rows_affected())
    }
}
