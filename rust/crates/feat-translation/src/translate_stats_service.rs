//! Port of `src/services/translateStatsService.js`: translation activity per channel (the `TranslateStat` table),
//! shown on the dashboard's Auto-Translate page: how many messages were translated, the languages their authors
//! wrote in (as the provider detected them) and the languages they were translated into. Auto-translate replies and
//! /translate in a server count; DMs never do. Counts only, never text.

use std::sync::Mutex;

use bot_core::App;
use serde_json::{Value, json};

/// How a message was translated (`STAT_VIA`): an auto-translate reply or the /translate command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatVia {
    Auto,
    Command,
}

impl StatVia {
    /// `'auto'` or `'command'`.
    pub fn as_str(self) -> &'static str {
        match self {
            StatVia::Auto => "auto",
            StatVia::Command => "command",
        }
    }

    /// The JS value, when it is one of the two.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(StatVia::Auto),
            "command" => Some(StatVia::Command),
            _ => None,
        }
    }
}

/// `STAT_DIRECTION.FROM`: the language the author wrote in.
pub const DIRECTION_FROM: &str = "from";
/// `STAT_DIRECTION.TO`: each language a message was translated into.
pub const DIRECTION_TO: &str = "to";
/// The `from` language when the provider detected none.
pub const UNKNOWN_LANG: &str = "und";

const INT4_MAX: i64 = 2_147_483_647;
const WARN_EVERY_MS: i64 = 10 * 60_000;

/// `/^[a-z]{2,3}(?:-[a-z0-9]{2,8})?$/`.
fn is_lang(s: &str) -> bool {
    let (base, sub) = match s.split_once('-') {
        Some((b, r)) => (b, Some(r)),
        None => (s, None),
    };
    (2..=3).contains(&base.len())
        && base.bytes().all(|b| b.is_ascii_lowercase())
        && sub.is_none_or(|r| (2..=8).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// `/^\d{15,25}$/`.
fn is_snowflake(s: &str) -> bool {
    (15..=25).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `statKeys(detected, targets)`: the counters one translated message adds to, `(direction, lang)` pairs without
/// duplicates and in a fixed order (two concurrent upserts then lock their shared rows in the same order).
pub fn stat_keys<S: AsRef<str>>(detected: Option<&str>, targets: &[S]) -> Vec<(&'static str, String)> {
    let from = detected.filter(|d| is_lang(d)).unwrap_or(UNKNOWN_LANG).to_string();
    let mut to: Vec<String> = Vec::new();
    for t in targets {
        let t = t.as_ref();
        if is_lang(t) && !to.iter().any(|x| x == t) {
            to.push(t.to_string());
        }
    }
    // Array.prototype.sort() compares UTF-16 code units; these strings are ASCII.
    to.sort();
    let mut keys = vec![(DIRECTION_FROM, from)];
    keys.extend(to.into_iter().map(|l| (DIRECTION_TO, l)));
    keys
}

/// [`stat_keys`] over JSON values (the JS accepts any array; non-strings are dropped).
pub fn stat_keys_values(detected: &Value, targets: &Value) -> Vec<(&'static str, String)> {
    let ts: Vec<String> = targets.as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
    stat_keys(detected.as_str(), &ts)
}

/// The upsert (`recordSql`): one `VALUES` row per key, `$n` binds, `SQL_NOW` for both dates.
pub fn record_sql(keys: usize) -> String {
    let mut rows = Vec::with_capacity(keys);
    for i in 0..keys {
        let b = i * 5;
        rows.push(format!(
            "(${}, ${}, ${}, ${}, ${}, 1, {now}, {now})",
            b + 1,
            b + 2,
            b + 3,
            b + 4,
            b + 5,
            now = bot_db::SQL_NOW
        ));
    }
    format!(
        "\n    INSERT INTO \"TranslateStat\" (\"guildId\", \"channelId\", \"via\", \"direction\", \"lang\", \"count\", \"firstAt\", \"lastAt\")\n    VALUES {}\n    ON CONFLICT (\"guildId\", \"channelId\", \"via\", \"direction\", \"lang\") DO UPDATE\n    SET \"count\" = LEAST(\"TranslateStat\".\"count\"::bigint + 1, ${})::int, \"lastAt\" = EXCLUDED.\"lastAt\"",
        rows.join(","),
        keys * 5 + 1
    )
}

/// The bind values of [`record_sql`]: five per key, then INT4_MAX.
pub fn record_values(guild_id: &str, channel_id: &str, via: StatVia, keys: &[(&'static str, String)]) -> Vec<Value> {
    let mut v = Vec::with_capacity(keys.len() * 5 + 1);
    for (direction, lang) in keys {
        v.extend([json!(guild_id), json!(channel_id), json!(via.as_str()), json!(direction), json!(lang)]);
    }
    v.push(json!(INT4_MAX));
    v
}

/// `RAW_SQL`: the statement with the JS example (`statKeys('es', ['ja', 'en'])`).
pub fn raw_sql() -> Vec<crate::translation_service::RawSql> {
    let keys = stat_keys(Some("es"), &["ja", "en"]);
    vec![crate::translation_service::RawSql {
        name: "translateStats.record",
        sql: record_sql(keys.len()),
        values: record_values("0", "0", StatVia::Auto, &keys),
    }]
}

#[derive(Default)]
struct WarnState(Mutex<i64>);

/// Runs the upsert for one translated message.
pub async fn record_raw(app: &App, guild_id: &str, channel_id: &str, via: StatVia, keys: &[(&'static str, String)]) -> Result<u64, bot_db::DbError> {
    let sql = record_sql(keys.len());
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for (direction, lang) in keys {
        q = q.bind(guild_id.to_string()).bind(channel_id.to_string()).bind(via.as_str()).bind(*direction).bind(lang.clone());
    }
    q = q.bind(INT4_MAX);
    Ok(q.execute(app.db()).await?.rows_affected())
}

/// `recordTranslation({ guildId, channelId, via, detected, targets })`: counts one translated message. Never fails,
/// so callers may spawn it.
pub async fn record_translation<S: AsRef<str>>(app: &App, guild_id: &str, channel_id: &str, via: StatVia, detected: Option<&str>, targets: &[S]) {
    if !is_snowflake(guild_id) || !is_snowflake(channel_id) {
        return;
    }
    let result: anyhow::Result<()> = async {
        app.settings().ensure_guild_settings(guild_id).await?;
        record_raw(app, guild_id, channel_id, via, &stat_keys(detected, targets)).await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let warn = app.ext::<WarnState>();
        let now = app.clock().now_ms();
        let due = {
            let mut last = warn.0.lock().unwrap_or_else(|p| p.into_inner());
            if now - *last >= WARN_EVERY_MS {
                *last = now;
                true
            } else {
                false
            }
        };
        if due {
            bot_util::log_warn!(format!("translateStats: could not count a translation in {guild_id}: {e}"));
        }
    }
}
