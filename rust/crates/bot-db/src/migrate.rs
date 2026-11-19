//! A migration runner compatible with `prisma migrate deploy` (Prisma 5.22).
//!
//! * The migrations are `prisma/migrations/*/migration.sql`, embedded at compile time and applied in name order.
//! * State lives in Prisma's `_prisma_migrations` table (same DDL, same columns). A row is applied when
//!   `finished_at IS NOT NULL AND rolled_back_at IS NULL`; rolled-back rows are ignored.
//! * `checksum` is the lowercase sha256 hex of the file, as Prisma writes it. A stored checksum also matches the file
//!   with its line endings converted either way (a CRLF checkout on Windows, LF in Docker), and the old Prisma
//!   format without zero padding, like Prisma's own `script_matches_checksum`.
//! * Each migration is recorded the way Prisma does it: a row inserted before the script runs (`started_at`,
//!   `applied_steps_count = 0`), the script run as one multi-statement simple query (one implicit transaction), then
//!   `applied_steps_count = 1` and `finished_at = now()`. A failure stores the error in `logs` and leaves
//!   `finished_at` NULL, which Prisma (and this runner) then refuse to continue past (P3009).
//! * The whole run holds Prisma's session advisory lock `pg_advisory_lock(72707369)` (unless
//!   `PRISMA_SCHEMA_DISABLE_ADVISORY_LOCK` is set), so the Rust bot and `prisma migrate deploy` never interleave.
//! * Like Prisma's deploy, an applied migration whose checksum no longer matches its file is only a warning
//!   ("The migration `<name>` was modified after it was applied."): it is listed in [`Plan::modified`] /
//!   [`MigrateReport::modified`], logged, and the run goes on.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Row};
use std::time::Duration;

use crate::{DbError, quote_ident};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/migrations.rs"));
}

/// Prisma's advisory lock key for the schema engine.
pub const PRISMA_ADVISORY_LOCK_KEY: i64 = 72707369;

/// How long Prisma waits for its advisory lock.
pub const ADVISORY_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The `_prisma_migrations` DDL Prisma 5.22 uses on PostgreSQL (with `IF NOT EXISTS`).
pub const CREATE_MIGRATIONS_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS _prisma_migrations (
    id                      VARCHAR(36) PRIMARY KEY NOT NULL,
    checksum                VARCHAR(64) NOT NULL,
    finished_at             TIMESTAMPTZ,
    migration_name          VARCHAR(255) NOT NULL,
    logs                    TEXT,
    rolled_back_at          TIMESTAMPTZ,
    started_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    applied_steps_count     INTEGER NOT NULL DEFAULT 0
)";

/// One migration directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// The directory name (`20261010120000_mee6_parity`).
    pub name: &'static str,
    /// The contents of `migration.sql` as built.
    pub sql: &'static str,
}

impl Migration {
    /// The checksum Prisma records for this file.
    pub fn checksum(&self) -> String {
        checksum(self.sql)
    }
}

/// Every embedded migration, in the order Prisma applies them.
pub fn migrations() -> Vec<Migration> {
    let mut v: Vec<Migration> = embedded::EMBEDDED.iter().map(|(name, sql)| Migration { name, sql }).collect();
    v.sort_by(|a, b| a.name.cmp(b.name));
    v
}

/// The migrations directory the crate was built from.
pub fn source_dir() -> &'static str {
    embedded::SOURCE_DIR
}

fn sha256(script: &str) -> [u8; 32] {
    let digest = Sha256::digest(script.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_slice());
    out
}

/// sha256 of the script as zero-padded lowercase hex (what Prisma stores).
pub fn checksum(script: &str) -> String {
    hex::encode(sha256(script))
}

/// The pre-2.x Prisma rendering without zero padding (`{:x}` per byte).
fn checksum_unpadded(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:x}")).collect()
}

/// Whether a stored checksum belongs to this script (as is, CRLF -> LF, or LF -> CRLF; padded or not).
pub fn checksum_matches(script: &str, stored: &str) -> bool {
    let lf = script.replace("\r\n", "\n");
    let crlf = lf.replace('\n', "\r\n");
    [script, lf.as_str(), crlf.as_str()].iter().any(|s| {
        let bytes = sha256(s);
        let padded = hex::encode(bytes);
        stored.eq_ignore_ascii_case(&padded) || (stored.len() != 64 && stored == checksum_unpadded(&bytes))
    })
}

/// One row of `_prisma_migrations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    pub id: String,
    pub checksum: String,
    pub finished_at: Option<DateTime<Utc>>,
    pub migration_name: String,
    pub logs: Option<String>,
    pub rolled_back_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub applied_steps_count: i32,
}

impl AppliedMigration {
    /// Applied and not rolled back.
    pub fn is_applied(&self) -> bool {
        self.finished_at.is_some() && self.rolled_back_at.is_none()
    }

    /// Started, never finished, never rolled back: Prisma's "failed migration".
    pub fn is_failed(&self) -> bool {
        self.finished_at.is_none() && self.rolled_back_at.is_none()
    }
}

/// What a deploy would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Migrations to apply, in order.
    pub pending: Vec<Migration>,
    /// Applied migrations whose directory does not exist in this build (Prisma only warns about these).
    pub missing_locally: Vec<String>,
    /// Applied migrations whose file no longer matches the recorded checksum (Prisma only warns about these).
    pub modified: Vec<String>,
}

/// Prisma's warning for an applied migration whose file changed (schema-engine `diagnose_migration_history`).
pub fn modified_warning(name: &str) -> String {
    format!("The migration `{name}` was modified after it was applied.")
}

/// Decides what to apply. Pure: `local` in any order, `applied` as read from `_prisma_migrations`.
pub fn plan(local: &[Migration], applied: &[AppliedMigration]) -> Result<Plan, DbError> {
    if let Some(failed) = applied.iter().find(|m| m.is_failed()) {
        return Err(DbError::Migration(format!(
            "migrate found failed migrations in the target database, new migrations will not be applied. \
             The `{}` migration started at {} failed (P3009). Resolve it with `prisma migrate resolve` first.",
            failed.migration_name, failed.started_at
        )));
    }
    let mut local: Vec<Migration> = local.to_vec();
    local.sort_by(|a, b| a.name.cmp(b.name));

    let mut pending = Vec::new();
    let mut modified = Vec::new();
    for m in &local {
        let rows: Vec<&AppliedMigration> = applied.iter().filter(|a| a.migration_name == m.name && a.is_applied()).collect();
        if rows.is_empty() {
            pending.push(*m);
            continue;
        }
        if !rows.iter().any(|r| checksum_matches(m.sql, &r.checksum)) {
            modified.push(m.name.to_string());
        }
    }
    let mut missing_locally: Vec<String> = applied
        .iter()
        .filter(|a| a.is_applied() && !local.iter().any(|m| m.name == a.migration_name))
        .map(|a| a.migration_name.clone())
        .collect();
    missing_locally.dedup();
    Ok(Plan { pending, missing_locally, modified })
}

/// Result of [`migrate_deploy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateReport {
    /// Names of the migrations applied by this run.
    pub applied: Vec<String>,
    /// Applied migrations unknown to this build.
    pub missing_locally: Vec<String>,
    /// Applied migrations whose file was modified after it was applied (a warning, as in Prisma).
    pub modified: Vec<String>,
}

async fn table_exists(conn: &mut PgConnection, schema: &str, table: &str) -> Result<bool, DbError> {
    let (exists,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND n.nspname = $1 AND c.relname = $2)",
    )
    .bind(schema)
    .bind(table)
    .fetch_one(&mut *conn)
    .await?;
    Ok(exists)
}

async fn user_table_count(conn: &mut PgConnection, schema: &str) -> Result<i64, DbError> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*)::bigint FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND n.nspname = $1 AND c.relname <> '_prisma_migrations'",
    )
    .bind(schema)
    .fetch_one(&mut *conn)
    .await?;
    Ok(n)
}

/// Reads `_prisma_migrations` (ordered by `started_at`, like Prisma). Empty when the table does not exist.
pub async fn read_applied(conn: &mut PgConnection, schema: &str) -> Result<Vec<AppliedMigration>, DbError> {
    if !table_exists(conn, schema, "_prisma_migrations").await? {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, checksum, finished_at, migration_name, logs, rolled_back_at, started_at, applied_steps_count \
         FROM {}._prisma_migrations ORDER BY started_at ASC",
        quote_ident(schema)
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(&mut *conn).await?;
    rows.into_iter()
        .map(|r| {
            Ok(AppliedMigration {
                id: r.try_get("id")?,
                checksum: r.try_get("checksum")?,
                finished_at: r.try_get("finished_at")?,
                migration_name: r.try_get("migration_name")?,
                logs: r.try_get("logs")?,
                rolled_back_at: r.try_get("rolled_back_at")?,
                started_at: r.try_get("started_at")?,
                applied_steps_count: r.try_get("applied_steps_count")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(DbError::from)
}

/// What [`migrate_deploy`] would apply, without applying anything.
pub async fn status(pool: &PgPool, schema: &str) -> Result<Plan, DbError> {
    let mut conn = pool.acquire().await?;
    let applied = read_applied(&mut conn, schema).await?;
    plan(&migrations(), &applied)
}

fn advisory_lock_disabled() -> bool {
    std::env::var_os("PRISMA_SCHEMA_DISABLE_ADVISORY_LOCK").is_some()
}

/// `prisma migrate deploy`: applies every embedded migration that `_prisma_migrations` does not record as applied.
/// `schema` is the `schema=` of `DATABASE_URL` ([`crate::DbConfig::schema`]).
pub async fn migrate_deploy(pool: &PgPool, schema: &str) -> Result<MigrateReport, DbError> {
    let mut conn = pool.acquire().await?;
    let locked = !advisory_lock_disabled();
    if locked {
        tokio::time::timeout(
            ADVISORY_LOCK_TIMEOUT,
            sqlx::query("SELECT pg_advisory_lock($1)").bind(PRISMA_ADVISORY_LOCK_KEY).execute(&mut *conn),
        )
        .await
        .map_err(|_| {
            DbError::Timeout(format!(
                "Timed out trying to acquire a postgres advisory lock (SELECT pg_advisory_lock({PRISMA_ADVISORY_LOCK_KEY})). Elapsed: {} ms.",
                ADVISORY_LOCK_TIMEOUT.as_millis()
            ))
        })??;
    }
    let result = deploy_locked(&mut conn, schema).await;
    if locked {
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(PRISMA_ADVISORY_LOCK_KEY).execute(&mut *conn).await;
    }
    result
}

async fn deploy_locked(conn: &mut PgConnection, schema: &str) -> Result<MigrateReport, DbError> {
    // Like Prisma: check first, create only when missing (CREATE SCHEMA needs CREATE on the database even with
    // IF NOT EXISTS, which a least-privilege bot user may not have).
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname = $1)").bind(schema).fetch_one(&mut *conn).await?;
    if !exists {
        tracing::info!(schema, "the schema does not exist on the target database; creating it");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema)))).execute(&mut *conn).await?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!("SET search_path TO {}", quote_ident(schema)))).execute(&mut *conn).await?;

    if !table_exists(conn, schema, "_prisma_migrations").await? {
        // Prisma's P3005: a non-empty schema without migration history must be baselined first.
        if user_table_count(conn, schema).await? > 0 {
            return Err(DbError::Migration(format!(
                "the database schema {schema:?} is not empty and has no _prisma_migrations table (P3005); baseline it first"
            )));
        }
        sqlx::query(CREATE_MIGRATIONS_TABLE_SQL).execute(&mut *conn).await?;
    }

    let applied = read_applied(conn, schema).await?;
    let plan = plan(&migrations(), &applied)?;
    for name in &plan.missing_locally {
        tracing::warn!(migration = %name, "applied migration is not present in this build");
    }
    for name in &plan.modified {
        tracing::warn!(migration = %name, "{}", modified_warning(name));
    }

    let mut done = Vec::new();
    for m in &plan.pending {
        let id = crate::new_uuid();
        sqlx::query(
            "INSERT INTO _prisma_migrations (id, checksum, migration_name, logs, rolled_back_at, started_at, applied_steps_count) \
             VALUES ($1, $2, $3, NULL, NULL, now(), 0)",
        )
        .bind(&id)
        .bind(m.checksum())
        .bind(m.name)
        .execute(&mut *conn)
        .await?;

        match sqlx::raw_sql(m.sql).execute(&mut *conn).await {
            Ok(_) => {
                sqlx::query("UPDATE _prisma_migrations SET applied_steps_count = applied_steps_count + 1 WHERE id = $1")
                    .bind(&id)
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("UPDATE _prisma_migrations SET finished_at = now() WHERE id = $1").bind(&id).execute(&mut *conn).await?;
                tracing::info!(migration = %m.name, "applied migration");
                done.push(m.name.to_string());
            }
            Err(e) => {
                let logs = format!("Migration name: {}\n\nDatabase error:\n{e}", m.name);
                let _ = sqlx::query("UPDATE _prisma_migrations SET logs = $1 WHERE id = $2").bind(&logs).bind(&id).execute(&mut *conn).await;
                return Err(DbError::Migration(format!("migration `{}` failed to apply: {e}", m.name)));
            }
        }
    }
    Ok(MigrateReport { applied: done, missing_locally: plan.missing_locally, modified: plan.modified })
}
