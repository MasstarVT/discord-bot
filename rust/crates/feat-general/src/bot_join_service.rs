//! Port of `src/services/botJoinService.js`: who added the bot to each guild (`BotJoin`), for the bot owner's
//! dashboard. The bot records it on `guildReady` ([`crate::plugins::bot_joins`]); the dashboard reads it with
//! [`list_bot_joins`].
//!
//! The Discord reads are raw REST calls parsed as JSON (what discord.js reads), so one odd audit-log entry or
//! integration does not fail the whole lookup the way a strict serenity model would.

use std::collections::HashMap;

use bot_core::App;
use bot_core::hooks::events::GuildReadyReason;
use bot_core::interaction_helpers::{PermTarget, bot_permissions, with_cached_guild};
use bot_core::permissions::js_has;
use bot_db::{BOT_JOIN_INFO, BotJoin, InsertBuilder, SelectBuilder};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{GuildId, Http, Permissions};
use serenity::http::Route;

/// Audit-log entries lag the gateway event a little (`AUDIT_LAG_MS`).
pub const AUDIT_LAG_MS: u64 = 1500;

/// `AuditLogEvent.BotAdd`.
pub const AUDIT_BOT_ADD: u8 = 28;

/// Who added the bot (`{ inviterId, inviterName, source }`); all `None` when unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inviter {
    pub inviter_id: Option<String>,
    pub inviter_name: Option<String>,
    /// `'audit'`, `'integration'`, or `None` when unknown.
    pub source: Option<String>,
}

impl Inviter {
    /// `UNKNOWN`.
    pub fn unknown() -> Self {
        Self::default()
    }
}

/// discord.js `user.tag` of a raw user object: the username, or `username#discriminator` for a legacy account;
/// `None` without a username.
pub fn raw_user_tag(user: &Value) -> Option<String> {
    let name = user.get("username").and_then(Value::as_str)?;
    match user.get("discriminator").and_then(Value::as_str) {
        None | Some("0") => Some(name.to_string()),
        Some(d) => Some(format!("{name}#{d}")),
    }
}

fn id_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The newest Bot Add entry of this bot in a raw `GET /guilds/{g}/audit-logs` answer (newest first), with an
/// executor: `logs.entries.find((e) => e.targetId === botId && e.executorId)`.
pub fn inviter_from_audit(logs: &Value, bot_id: &str) -> Option<Inviter> {
    let entries = logs.get("entries")?.as_array()?;
    let entry = entries.iter().find(|e| id_of(e.get("target_id")).as_deref() == Some(bot_id) && id_of(e.get("user_id")).is_some())?;
    let executor_id = id_of(entry.get("user_id"))?;
    let name = logs
        .get("users")
        .and_then(Value::as_array)
        .and_then(|users| users.iter().find(|u| id_of(u.get("id")).as_deref() == Some(executor_id.as_str())))
        .and_then(raw_user_tag);
    Some(Inviter { inviter_id: Some(executor_id), inviter_name: name, source: Some("audit".into()) })
}

/// The server's integration for the bot in a raw `GET /guilds/{g}/integrations` answer:
/// `integrations.find((i) => i.user && (i.application?.id === appId || i.account?.id === botId))`.
pub fn inviter_from_integrations(list: &Value, bot_id: &str, app_id: &str) -> Option<Inviter> {
    let mine = list.as_array()?.iter().find(|i| {
        let has_user = i.get("user").is_some_and(|u| u.is_object());
        let app = id_of(i.get("application").and_then(|a| a.get("id")));
        let account = id_of(i.get("account").and_then(|a| a.get("id")));
        has_user && (app.as_deref() == Some(app_id) || account.as_deref() == Some(bot_id))
    })?;
    let user = mine.get("user")?;
    Some(Inviter { inviter_id: id_of(user.get("id")), inviter_name: raw_user_tag(user), source: Some("integration".into()) })
}

/// A raw GET through serenity's HTTP client (rate limiter included), parsed as JSON.
/// Kept for callers of this crate; it is `bot_core::discord::rest_get_json`.
pub async fn rest_get_json(http: &Http, route: Route<'_>, params: Option<Vec<(&'static str, String)>>) -> anyhow::Result<Value> {
    bot_core::discord::rest_get_json(http, route, params).await
}

/// `findInviter(guild, botId, appId)` from the bot's guild permissions and two REST calls: the newest Bot Add
/// audit-log entry (needs View Audit Log; Discord keeps entries about 45 days), else the server's integration for the
/// bot (needs Manage Server; kept while the bot is there). Never fails; a failed fetch counts as no answer.
pub async fn find_inviter_with(http: &Http, guild_id: GuildId, perms: Option<Permissions>, bot_id: &str, app_id: &str) -> Inviter {
    let has = |p: Permissions| perms.is_some_and(|bits| js_has(bits, p));
    if has(Permissions::VIEW_AUDIT_LOG) {
        let params = vec![("action_type", AUDIT_BOT_ADD.to_string()), ("limit", "10".to_string())];
        if let Ok(logs) = rest_get_json(http, Route::GuildAuditLogs { guild_id }, Some(params)).await
            && let Some(found) = inviter_from_audit(&logs, bot_id)
        {
            return found;
        }
    }
    if has(Permissions::MANAGE_GUILD)
        && let Ok(list) = rest_get_json(http, Route::GuildIntegrations { guild_id }, None).await
        && let Some(found) = inviter_from_integrations(&list, bot_id, app_id)
    {
        return found;
    }
    Inviter::unknown()
}

/// `findInviter(guild, botId, appId)` with the bot's permissions from the serenity cache (`guild.members.me`). Unknown
/// without a Discord connection.
pub async fn find_inviter(app: &App, guild_id: GuildId, bot_id: &str, app_id: &str) -> Inviter {
    let Some(http) = app.http() else { return Inviter::unknown() };
    let perms = bot_permissions(app, PermTarget::Guild(guild_id));
    find_inviter_with(&http, guild_id, perms, bot_id, app_id).await
}

/// One stored row as the dashboard reads it (`{ inviterId, inviterName, source, joinedAt }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotJoinEntry {
    pub inviter_id: Option<String>,
    pub inviter_name: Option<String>,
    pub source: Option<String>,
    #[serde(with = "bot_db::serde_js::date")]
    pub joined_at: DateTime<Utc>,
}

/// `recordBotJoin({ client, guild, reason })`: the `guildReady` hook body. On a join it waits for the audit log and
/// records who added the bot, replacing any older row (a re-add); on startup it looks again only where the inviter is
/// not known yet, so a restart does not query every server again. Never fails.
pub async fn record_bot_join(app: &App, guild_id: GuildId, reason: GuildReadyReason) {
    record_bot_join_with_lag(app, guild_id, reason, AUDIT_LAG_MS).await;
}

/// [`record_bot_join`] with the audit-log lag of a join given (tests pass 0).
pub async fn record_bot_join_with_lag(app: &App, guild_id: GuildId, reason: GuildReadyReason, lag_ms: u64) {
    if let Err(e) = try_record(app, guild_id, reason, lag_ms).await {
        bot_util::log_warn!(format!("Could not record who added the bot to guild {guild_id}: {e}"));
    }
}

async fn try_record(app: &App, guild_id: GuildId, reason: GuildReadyReason, lag_ms: u64) -> anyhow::Result<()> {
    let Some(discord) = app.discord() else { return Ok(()) };
    let bot_id = discord.bot_user_id().to_string();
    let app_id = discord.http.application_id().map(|a| a.to_string()).unwrap_or_else(|| bot_id.clone());
    let g = guild_id.to_string();
    if reason == GuildReadyReason::Join {
        tokio::time::sleep(std::time::Duration::from_millis(lag_ms)).await;
    } else {
        let row: Option<BotJoin> = SelectBuilder::new(&BOT_JOIN_INFO).filter("guildId", json!(g))?.build()?.fetch_optional(app.db()).await?;
        if row.is_some_and(|r| r.inviter_id.is_some_and(|id| !id.is_empty())) {
            return Ok(());
        }
    }
    let found = find_inviter(app, guild_id, &bot_id, &app_id).await;
    let joined_at = with_cached_guild(app, guild_id, |guild| guild.joined_at.timestamp_millis()).unwrap_or_else(|| app.clock().now_ms());
    save_bot_join(app, &g, &found, joined_at).await
}

/// The upsert of `recordBotJoin` (`create { guildId, ...data }`, `update data`), after `ensureGuildSettings`.
pub async fn save_bot_join(app: &App, guild_id: &str, found: &Inviter, joined_at_ms: i64) -> anyhow::Result<()> {
    let joined = bot_util::js::to_iso_string(joined_at_ms as f64).unwrap_or_default();
    app.settings().ensure_guild_settings(guild_id).await?;
    let data = [
        ("inviterId", json!(found.inviter_id)),
        ("inviterName", json!(found.inviter_name)),
        ("source", json!(found.source)),
        ("joinedAt", json!(joined)),
    ];
    InsertBuilder::new(&BOT_JOIN_INFO)
        .value("guildId", json!(guild_id))?
        .values(data.clone())?
        .on_conflict_update(&["guildId"], data)?
        .build()?
        .execute(app.db())
        .await?;
    Ok(())
}

/// `listBotJoins(guildIds)`: the stored rows for these guilds, by guild id. Fails on a database error (the dashboard
/// leaves the field out).
pub async fn list_bot_joins(db: &bot_db::PgPool, guild_ids: &[String]) -> anyhow::Result<HashMap<String, BotJoinEntry>> {
    if guild_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<BotJoin> = bot_db::sqlx::query_as(r#"SELECT * FROM "BotJoin" WHERE "guildId" = ANY($1)"#).bind(guild_ids).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.guild_id, BotJoinEntry { inviter_id: r.inviter_id, inviter_name: r.inviter_name, source: r.source, joined_at: r.joined_at }))
        .collect())
}
