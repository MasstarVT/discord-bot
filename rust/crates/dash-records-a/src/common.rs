//! What the stores of this crate share: the lock statements of the JS stores (`lockGuildSettingsSql`, `lockSql`),
//! the transaction error, task helpers and small JSON helpers.

use bot_db::DbError;
use dash_collections::{RecordError, StoreCtx, StoreError, TaskOutcome, task_outcome};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::PgConnection;

/// `lockGuildSettingsSql(guildId)` of levelRoles.store.js: locks the GuildSettings row, serialising the
/// count-then-create cap checks of every dashboard store.
pub const LOCK_GUILD_SETTINGS_SQL: &str = r#"SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#;

/// `lockSql(key)` of embeds.store.js and feeds.store.js: a transaction-scoped advisory lock on `hashtext(key)`.
pub const ADVISORY_LOCK_SQL: &str = "SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext($1))) AS l";

/// `{ timeout: 10_000, maxWait: 10_000 }` (`TX_OPTS` of the stores).
pub const TX_OPTS: bot_db::TxOptions = bot_db::TxOptions::TEN_SECONDS;

/// Runs [`LOCK_GUILD_SETTINGS_SQL`] (`tx.$queryRaw(lockGuildSettingsSql(guildId))`).
pub async fn lock_guild_settings(tx: &mut PgConnection, guild_id: &str) -> Result<(), TxError> {
    sqlx::query(LOCK_GUILD_SETTINGS_SQL).bind(guild_id).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    Ok(())
}

/// Runs [`ADVISORY_LOCK_SQL`] (`tx.$queryRaw(lockSql(key))`).
pub async fn advisory_lock(tx: &mut PgConnection, key: &str) -> Result<(), TxError> {
    sqlx::query(ADVISORY_LOCK_SQL).bind(key).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    Ok(())
}

/// What a transaction body fails with: the database, or a RecordError thrown inside it.
#[derive(Debug)]
pub enum TxError {
    Db(DbError),
    Record(RecordError),
    Other(anyhow::Error),
}

impl TxError {
    /// Prisma's P2002 (a unique violation).
    pub fn is_unique(&self) -> bool {
        match self {
            Self::Db(e) => e.is_unique_violation(),
            Self::Other(e) => is_unique(e),
            Self::Record(_) => false,
        }
    }
}

impl From<DbError> for TxError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl From<sqlx::Error> for TxError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(DbError::from(e))
    }
}

impl From<RecordError> for TxError {
    fn from(e: RecordError) -> Self {
        Self::Record(e)
    }
}

impl From<anyhow::Error> for TxError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

impl From<TxError> for StoreError {
    fn from(e: TxError) -> Self {
        match e {
            TxError::Db(e) => StoreError::Internal(e.into()),
            TxError::Record(e) => StoreError::Record(e),
            TxError::Other(e) => internal(e),
        }
    }
}

/// `err?.code === 'P2002'` for any error (a DbError or sqlx error anywhere in the chain).
pub fn is_unique(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
            || c.downcast_ref::<sqlx::Error>().is_some_and(|s| matches!(s, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505")))
    })
}

/// Any other error: logged and answered 500/503 by the server. A RecordError inside the anyhow error comes back as
/// itself.
pub fn internal<E: Into<anyhow::Error>>(e: E) -> StoreError {
    let e: anyhow::Error = e.into();
    match e.downcast::<RecordError>() {
        Ok(r) => StoreError::Record(r),
        Err(e) => match e.downcast::<StoreError>() {
            Ok(s) => s,
            Err(e) => StoreError::Internal(e),
        },
    }
}

/// `serde_json::to_value` of a bot-db row (Prisma's JSON: dates as ISO strings).
pub fn row_json<T: Serialize>(row: &T) -> Value {
    serde_json::to_value(row).unwrap_or(Value::Null)
}

/// `typeof ctx.discord?.[method] === 'function'`: the client, when there is one and it offers `method`.
pub fn discord_with(
    discord: Option<&std::sync::Arc<dyn dash_collections::DashboardDiscord>>,
    method: dash_collections::DiscordMethod,
) -> Option<&std::sync::Arc<dyn dash_collections::DashboardDiscord>> {
    discord.filter(|d| d.supports(method))
}

/// "The bot cannot be reached from the dashboard right now." (requireTasks / runner).
pub const NO_BOT: &str = "The bot cannot be reached from the dashboard right now.";

/// `requireTasks(ctx)`: `typeof ctx.runTask !== 'function'` answers unavailable.
pub fn require_tasks(ctx: &StoreCtx) -> Result<(), StoreError> {
    if ctx.can_run_tasks() { Ok(()) } else { Err(RecordError::unavailable(NO_BOT).into()) }
}

/// `taskOutcome(await ctx.runTask(kind, payload))` (after [`require_tasks`]).
pub async fn run_task(ctx: &StoreCtx, kind: &str, payload: Value) -> Result<TaskOutcome, StoreError> {
    require_tasks(ctx)?;
    let result = ctx.run_task(kind, payload).await.map_err(StoreError::Internal)?;
    Ok(task_outcome(result.as_ref())?)
}

/// `Number.isInteger(v)` as an i64.
pub fn int(v: Option<&Value>) -> Option<i64> {
    let f = v?.as_f64()?;
    (f.is_finite() && f.fract() == 0.0).then_some(f as i64)
}

/// A string entry (`data.x` when it is a string).
pub fn str_at<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

/// `v !== undefined` for a map entry.
pub fn defined(m: &Map<String, Value>, k: &str) -> bool {
    m.contains_key(k)
}

/// `(v ?? null)`: absent and null both read as null.
pub fn nn(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(v) => v.clone(),
    }
}

/// A truthy JS value.
pub fn truthy(v: Option<&Value>) -> bool {
    dash_schema::js::truthy(v)
}

/// `date.getTime()` of a stored ISO date.
pub fn iso_ms(v: Option<&Value>) -> Option<i64> {
    let t = bot_util::js::date_parse(v?.as_str()?);
    t.is_finite().then_some(t as i64)
}

/// Epoch ms as a timestamp(3) parameter.
pub fn now_ms_naive(ms: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().naive_utc()
}
