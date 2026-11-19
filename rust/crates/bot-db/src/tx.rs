//! Interactive transactions (Prisma's `$transaction(async (tx) => ..., { timeout, maxWait })`) and the locking
//! statements the JS code runs inside them, ported statement for statement:
//!
//! * `SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE` ([`lock_guild_settings`])
//! * `SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))` ([`advisory_xact_lock`])
//! * `... FOR UPDATE SKIP LOCKED` ([`crate::query::LockMode::ForUpdateSkipLocked`], [`FOR_UPDATE_SKIP_LOCKED`])

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use sqlx::{PgConnection, PgPool};

use crate::DbError;

/// The row-lock suffix of the scheduler's claim query.
pub const FOR_UPDATE_SKIP_LOCKED: &str = "FOR UPDATE SKIP LOCKED";

/// `lockGuildSettingsSql` / `lockSettingsRow` in the JS.
pub const LOCK_GUILD_SETTINGS_SQL: &str = r#"SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#;

/// `lockSql(key)` in the JS stores: a transaction-scoped advisory lock on `hashtext(key)`.
pub const ADVISORY_XACT_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";

/// Non-blocking variant of [`ADVISORY_XACT_LOCK_SQL`].
pub const TRY_ADVISORY_XACT_LOCK_SQL: &str = "SELECT pg_try_advisory_xact_lock(hashtext($1)) AS ok";

/// `{ timeout, maxWait }` of an interactive transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxOptions {
    /// How long to wait for a connection (Prisma `maxWait`, default 2 s).
    pub max_wait: Duration,
    /// How long the whole transaction may run before it is rolled back (Prisma `timeout`, default 5 s).
    pub timeout: Duration,
}

impl Default for TxOptions {
    fn default() -> Self {
        Self { max_wait: Duration::from_secs(2), timeout: Duration::from_secs(5) }
    }
}

impl TxOptions {
    /// `{ timeout: 10_000, maxWait: 10_000 }`, the options the JS services use.
    pub const TEN_SECONDS: TxOptions = TxOptions { max_wait: Duration::from_secs(10), timeout: Duration::from_secs(10) };

    pub fn new(max_wait_ms: u64, timeout_ms: u64) -> Self {
        Self { max_wait: Duration::from_millis(max_wait_ms), timeout: Duration::from_millis(timeout_ms) }
    }
}

/// A boxed future borrowing the transaction's connection.
pub type TxFuture<'c, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'c>>;

/// Runs `f` inside a transaction: committed when it returns `Ok`, rolled back on `Err`, on panic-free early drop, and
/// when `opts.timeout` elapses (Prisma's "Transaction already closed" case, reported as [`DbError::Timeout`]).
///
/// ```ignore
/// let n = transaction(&pool, TxOptions::TEN_SECONDS, |tx| Box::pin(async move {
///     lock_guild_settings(tx, guild_id).await?;
///     Ok::<_, DbError>(1)
/// })).await?;
/// ```
pub async fn transaction<T, E, F>(pool: &PgPool, opts: TxOptions, f: F) -> Result<T, E>
where
    E: From<DbError>,
    F: for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, T, E>,
{
    let mut tx = tokio::time::timeout(opts.max_wait, pool.begin())
        .await
        .map_err(|_| E::from(DbError::Timeout(format!("unable to start a transaction within {} ms", opts.max_wait.as_millis()))))?
        .map_err(|e| E::from(DbError::from(e)))?;
    let outcome = tokio::time::timeout(opts.timeout, f(&mut tx)).await;
    match outcome {
        Ok(Ok(value)) => {
            tx.commit().await.map_err(|e| E::from(DbError::from(e)))?;
            Ok(value)
        }
        Ok(Err(e)) => {
            let _ = tx.rollback().await;
            Err(e)
        }
        Err(_) => {
            let _ = tx.rollback().await;
            Err(E::from(DbError::Timeout(format!("transaction exceeded its timeout of {} ms", opts.timeout.as_millis()))))
        }
    }
}

/// Locks the guild's `GuildSettings` row until the transaction ends. `false` when the row does not exist.
pub async fn lock_guild_settings(conn: &mut PgConnection, guild_id: &str) -> Result<bool, DbError> {
    let row: Option<(i32,)> = sqlx::query_as(LOCK_GUILD_SETTINGS_SQL).bind(guild_id).fetch_optional(conn).await?;
    Ok(row.is_some())
}

/// Takes the transaction-scoped advisory lock on `hashtext(key)` (blocks until it is free).
pub async fn advisory_xact_lock(conn: &mut PgConnection, key: &str) -> Result<(), DbError> {
    sqlx::query(ADVISORY_XACT_LOCK_SQL).bind(key).execute(conn).await?;
    Ok(())
}

/// Tries the transaction-scoped advisory lock on `hashtext(key)`; `false` when another transaction holds it.
pub async fn try_advisory_xact_lock(conn: &mut PgConnection, key: &str) -> Result<bool, DbError> {
    let (ok,): (bool,) = sqlx::query_as(TRY_ADVISORY_XACT_LOCK_SQL).bind(key).fetch_one(conn).await?;
    Ok(ok)
}

/// `pg_advisory_xact_lock(id)` on a numeric key.
pub async fn advisory_xact_lock_id(conn: &mut PgConnection, id: i64) -> Result<(), DbError> {
    sqlx::query("SELECT 1 AS ok FROM pg_advisory_xact_lock($1)").bind(id).execute(conn).await?;
    Ok(())
}
