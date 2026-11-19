//! Helpers every store of this crate shares: JSON rows, error mapping, the advisory lock of the JS stores (`lockSql`),
//! the server resources (`ctx.discord.getGuildResources`) and the task outcome wrappers (`runAfterSave`).

use std::sync::Arc;

use bot_db::DbError;
use dash_collections::errors::{TaskOutcome, task_outcome};
use dash_collections::{DashboardDiscord, DiscordMethod, DiscordResources, RecordError, RecordErrorCode, StoreCtx, StoreError};
use serde::Serialize;
use serde_json::{Map, Value};

/// A store answer.
pub type R<T> = Result<T, StoreError>;

/// `Prisma.sql\`SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext(${key}))) AS l\``: the JS stores' `lockSql`.
pub const LOCK_SQL: &str = "SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext($1))) AS l";

/// `tx.$queryRaw(lockSql(key))`.
pub async fn lock(conn: &mut sqlx::PgConnection, key: &str) -> Result<(), DbError> {
    sqlx::query(LOCK_SQL).bind(key).execute(conn).await?;
    Ok(())
}

/// A model row as Prisma returns it (Prisma field names, dates as ISO strings).
pub fn row_json<T: Serialize>(row: &T) -> Value {
    serde_json::to_value(row).unwrap_or(Value::Null)
}

/// A row's JSON object (an empty map for a value that is not one).
pub fn row_map<T: Serialize>(row: &T) -> Map<String, Value> {
    match row_json(row) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// An unexpected failure (logged by the server, answered 500 on a write and 503 on a read).
pub fn internal<E: Into<anyhow::Error>>(e: E) -> StoreError {
    StoreError::Internal(e.into())
}

/// `.map_err(internal)` for any error type.
pub trait OrInternal<T> {
    fn oi(self) -> R<T>;
}

impl<T, E: Into<anyhow::Error>> OrInternal<T> for Result<T, E> {
    fn oi(self) -> R<T> {
        self.map_err(internal)
    }
}

/// `new RecordError(code, message, { path })` as a store error.
pub fn rec_at(code: RecordErrorCode, message: impl Into<String>, path: &str) -> StoreError {
    RecordError::new(code, message).with_path(path).into()
}

/// Prisma's P2002 (a unique constraint) anywhere in an `anyhow::Error`.
pub fn is_unique(err: &anyhow::Error) -> bool {
    // Anywhere in the chain: a service may wrap the database error in its own context.
    err.chain().any(|c| {
        c.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
            || c.downcast_ref::<sqlx::Error>().is_some_and(|e| e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505"))
    })
}

/// `String(ctx?.userId ?? '0')`.
pub fn actor_id(ctx: &StoreCtx) -> String {
    if ctx.user_id.is_empty() { "0".to_string() } else { ctx.user_id.clone() }
}

/// JS truthiness of a JSON value (missing is falsy).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(s).slice(0, n)` (UTF-16 code units, as the JS counts them).
pub fn js_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `n.toLocaleString('en-US')` of an integer.
pub fn locale_int(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// The custom emoji ids of `entries[i].emoji` (`/:(\d{17,20})>$/`).
pub fn custom_emoji_id(emoji: &str) -> Option<&str> {
    let body = emoji.strip_suffix('>')?;
    let idx = body.rfind(':')?;
    let id = &body[idx + 1..];
    ((17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// `typeof ctx.discord?.[method] === 'function'`: the client, when there is one and it offers `method`.
pub fn discord_with(ctx: &StoreCtx, method: DiscordMethod) -> Option<&Arc<dyn DashboardDiscord>> {
    ctx.discord.as_ref().filter(|d| d.supports(method))
}

/// `ctx.discord.getGuildResources(guildId)` with the store's texts: `unavailable` when there is no client, the
/// 'discord' RecordError when Discord fails. `emoji_ids`: custom emojis the form uses; when the cached list misses one,
/// the list is read again, fresh (a custom emoji may be a few seconds old).
pub async fn load_resources(
    ctx: &StoreCtx,
    guild_id: &str,
    unavailable: &str,
    discord_failed: &str,
    emoji_ids: &[String],
) -> R<Arc<DiscordResources>> {
    let Some(discord) = discord_with(ctx, DiscordMethod::GetGuildResources) else { return Err(RecordError::unavailable(unavailable).into()) };
    let fail = |_| StoreError::from(RecordError::discord(discord_failed));
    let mut resources = discord.get_guild_resources(guild_id, false).await.map_err(fail)?;
    if emoji_ids.iter().any(|id| !resources.emojis.iter().any(|e| &e.id == id)) {
        resources = discord.get_guild_resources(guild_id, true).await.map_err(fail)?;
    }
    Ok(resources)
}

/// `ctx.runTask(kind, payload)` then `taskOutcome(...)`: no runner is the store's 'unavailable' text; a queueing
/// failure is internal.
pub async fn run_task(ctx: &StoreCtx, kind: &str, payload: Value) -> R<TaskOutcome> {
    let result = ctx.run_task(kind, payload).await.oi()?;
    Ok(task_outcome(result.as_ref())?)
}

/// `runAfterSave(ctx, kind, payload, { lead, hint })`: a refusal after the change was saved becomes "saved, but <why>.
/// <what to do>" (the change stays).
pub async fn run_after_save(ctx: &StoreCtx, kind: &str, payload: Value, lead: &str, hint: &str) -> R<TaskOutcome> {
    match run_task(ctx, kind, payload).await {
        Err(StoreError::Record(e)) if matches!(e.code, RecordErrorCode::Failed | RecordErrorCode::Unavailable) => {
            Err(RecordError::new(e.code, format!("{lead} {} {hint}", e.message)).saved().into())
        }
        other => other,
    }
}

/// `(msg.endsWith('.') ? msg : msg + '.')`.
pub fn sentence(msg: &str) -> String {
    if msg.ends_with('.') { msg.to_string() } else { format!("{msg}.") }
}

/// The epoch ms of the app clock (`Date.now()`).
pub fn now_ms(app: &bot_core::App) -> i64 {
    app.clock().now_ms()
}

/// `new Date()` on the app clock.
pub fn now(app: &bot_core::App) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(now_ms(app)).unwrap_or_else(chrono::Utc::now)
}

/// The collection definition's cross-field rule run again on update, against the item as it is now:
/// `collection.check(data, { mode: 'update', current })`.
pub fn recheck(key: &str, data: &Map<String, Value>, current: &Value, now_ms: i64) -> R<()> {
    let Some(c) = dash_collections::Collections::builtin().get(key) else { return Ok(()) };
    let Some(check) = c.check else { return Ok(()) };
    let cur = current.as_object();
    let ctx = dash_collections::CheckCtx { mode: dash_collections::Mode::Update, current: cur, now_ms: now_ms as f64 };
    match check(data, &ctx) {
        Some(p) => Err(RecordError::invalid(p.message).with_path(p.path).into()),
        None => Ok(()),
    }
}

/// Logs what the JS logs with `ctx.logger?.info?.(...)` (the dashboard's logger).
pub fn log_info(text: String) {
    bot_util::log_info!(text);
}
