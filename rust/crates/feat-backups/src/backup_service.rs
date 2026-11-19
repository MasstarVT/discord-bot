//! Port of `src/services/backupService.js`: snapshots of a guild's roles, categories, channels and permission overwrites
//! (stored in `ServerBackup`), the automatic backup job (`backup.auto`) and the restore job (`backup.restore`). The
//! snapshot format and the restore engine are [`crate::guild_snapshot`]; the plugin is [`crate::plugin`] and the
//! command [`crate::commands::backup`].
//!
//! Restore rules (they are the point of this module):
//!   - It NEVER deletes anything. Roles and channels are created, and in mode `full` edited, moved and given permission
//!     overwrites one at a time. Things that are not in the backup stay.
//!   - Only the SERVER OWNER can start one (checked in the command and again in the button) or delete a backup.
//!   - It never runs inside the interaction. Confirm schedules the one-shot job `backup.restore` (dedupe
//!     `backup.restore:{guildId}`: one restore at a time per guild). Each run does a bounded batch and saves its progress
//!     in the job payload, so it resumes after a crash or restart.
//!   - The bot can only give permissions it has itself and place roles below its own highest role: the rest is masked
//!     and reported in the summary posted in the channel where the restore was started.
//!   - A finished restore stores old id -> new id in the backup (`data.idMap`, never exported), so restoring the same
//!     backup again does not make a second copy of what the first one recreated.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter};
use sqlx::Row;

use bot_config::limits::{BACKUP_BYTES, BACKUPS_AUTO_KEEP, BACKUPS_MANUAL};
use bot_core::context::App;
use bot_core::embed_builder::{error_embed, guild_embed, info_embed, success_embed, warn_embed};
use bot_core::interaction_helpers::{Payload, is_interaction_gone, safe_reply};
use bot_core::prelude::*;
use bot_core::sync::KeyedMutex;
use bot_db::{GuildSettings, InsertBuilder, JobStatus, PgPool, SERVER_BACKUP_INFO, ScheduledJob, ServerBackup};
use bot_util::text::{clamp_text, clamp_text_with, format_number};
use bot_util::time::{discord_timestamp, discord_timestamp_ms};

use crate::guild_snapshot::{
    self as snap, BatchResult, GuildView, MeView, Preview, RESTORE_MODES, RestoreApi, RestoreState, Snapshot, SnapshotError, capture_guild, make_env,
    normalise_restore_state, parse_snapshot, preview_restore, run_restore_batch, snapshot_bytes,
};
use crate::live_guild::{LiveGuild, SummaryPost};

pub use crate::guild_snapshot::{bit_names, exportable, snapshot_stats};

/// `BACKUP`.
pub mod backup {
    pub const NAME_MAX: usize = 64;
    /// A restore confirmation expires this long after its message was sent.
    pub const CONFIRM_TTL_MS: i64 = 5 * 60_000;
    /// backup.auto wakes at least this often, so dashboard changes take effect.
    pub const AUTO_MAX_WAIT_MS: i64 = 60 * 60_000;
    pub const AUTO_MIN_WAIT_MS: i64 = 60_000;
    pub const RETRY_MS: i64 = 5 * 60_000;
    pub const STARTUP_DELAY_MS: i64 = 60_000;
    pub const STARTUP_JITTER_MS: i64 = 5 * 60_000;
    /// Between two restore batches.
    pub const STEP_MS: i64 = 2000;
    /// A restore job this late (the bot was away) is stopped, not resumed weeks later.
    pub const STALE_MS: i64 = 30 * 60_000;
    pub const ROLE_CAP: i64 = 250;
    pub const CHANNEL_CAP: i64 = 500;
    pub const CALLS_PER_BATCH: i64 = 10;
}
use backup as B;

pub fn auto_job_key(guild_id: &str) -> String {
    format!("backup.auto:{guild_id}")
}
pub fn restore_job_key(guild_id: &str) -> String {
    format!("backup.restore:{guild_id}")
}

fn clamp(n: i64, min: i64, max: i64) -> i64 {
    n.max(min).min(max)
}

/// `isLive(job)`: PENDING or RUNNING.
pub fn is_live(job: Option<&ScheduledJob>) -> bool {
    job.is_some_and(|j| matches!(j.status, JobStatus::Pending | JobStatus::Running))
}

fn now(app: &App) -> DateTime<Utc> {
    app.clock().now()
}

fn at_ms(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

// ── Rows ──────────────────────────────────────────────────────────────────────

/// A backup without its snapshot (`LIST_SELECT`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub id: String,
    pub guild_id: String,
    pub name: Option<String>,
    pub automatic: bool,
    pub created_by_id: Option<String>,
    pub size_bytes: i32,
    pub role_count: i32,
    pub channel_count: i32,
    #[serde(with = "bot_db::serde_js::date")]
    pub created_at: DateTime<Utc>,
}

impl BackupInfo {
    pub fn of(row: &ServerBackup) -> Self {
        Self {
            id: row.id.clone(),
            guild_id: row.guild_id.clone(),
            name: row.name.clone(),
            automatic: row.automatic,
            created_by_id: row.created_by_id.clone(),
            size_bytes: row.size_bytes,
            role_count: row.role_count,
            channel_count: row.channel_count,
            created_at: row.created_at,
        }
    }

    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            guild_id: row.try_get("guildId")?,
            name: row.try_get("name")?,
            automatic: row.try_get("automatic")?,
            created_by_id: row.try_get("createdById")?,
            size_bytes: row.try_get("sizeBytes")?,
            role_count: row.try_get("roleCount")?,
            channel_count: row.try_get("channelCount")?,
            created_at: row.try_get::<chrono::NaiveDateTime, _>("createdAt")?.and_utc(),
        })
    }

    /// The name, or the kind when it has none.
    pub fn display_name(&self) -> &str {
        match &self.name {
            Some(n) if !n.is_empty() => n,
            _ if self.automatic => "Automatic backup",
            _ => "Manual backup",
        }
    }
}

const LIST_COLUMNS: &str = r#""id", "guildId", "name", "automatic", "createdById", "sizeBytes", "roleCount", "channelCount", "createdAt""#;

/// A name typed by an admin: one line, control characters gone, at most NAME_MAX characters; `None` when empty.
pub fn clean_name(name: Option<&str>) -> Option<String> {
    let name = name?;
    let mut flat = String::with_capacity(name.len());
    let mut in_ctrl = false;
    for c in name.chars() {
        let ctrl = (c as u32) < 0x20 || c as u32 == 0x7f;
        if ctrl {
            if !in_ctrl {
                flat.push(' ');
            }
            in_ctrl = true;
        } else {
            flat.push(c);
            in_ctrl = false;
        }
    }
    let mut collapsed = String::with_capacity(flat.len());
    let mut in_space = false;
    for c in flat.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let trimmed = bot_util::js::trim(&collapsed);
    if trimmed.is_empty() { None } else { Some(clamp_text_with(trimmed, B::NAME_MAX, "")) }
}

/// `2026-10-21 14:05 UTC`
pub fn stamp_of(date: &DateTime<Utc>) -> String {
    format!("{} UTC", date.format("%Y-%m-%d %H:%M"))
}

/// How a backup is named in lists and autocomplete.
pub fn backup_label(row: &BackupInfo) -> String {
    let named = row.name.as_deref().is_some_and(|n| !n.is_empty());
    format!("{} · {}{}", row.display_name(), stamp_of(&row.created_at), if named && row.automatic { " · auto" } else { "" })
}

async fn fetch_infos(db: &PgPool, sql: String, binds: Vec<Value>) -> anyhow::Result<Vec<BackupInfo>> {
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in binds {
        q = match b {
            Value::String(s) => q.bind(s),
            Value::Bool(x) => q.bind(x),
            Value::Number(n) => q.bind(n.as_i64().unwrap_or(0)),
            _ => q.bind(Option::<String>::None),
        };
    }
    let rows = q.fetch_all(db).await.map_err(bot_db::DbError::from)?;
    Ok(rows.iter().map(BackupInfo::from_row).collect::<Result<Vec<_>, _>>().map_err(bot_db::DbError::from)?)
}

/// The guild's backups, newest first.
pub async fn list_backups(db: &PgPool, guild_id: &str) -> anyhow::Result<Vec<BackupInfo>> {
    let take = (BACKUPS_MANUAL + BACKUPS_AUTO_KEEP + 10) as i64;
    let sql = format!(r#"SELECT {LIST_COLUMNS} FROM "ServerBackup" WHERE "guildId" = $1 ORDER BY "createdAt" DESC, "id" DESC LIMIT $2"#);
    fetch_infos(db, sql, vec![json!(guild_id), json!(take)]).await
}

fn usable_id(id: &str) -> bool {
    !id.is_empty() && bot_util::js::utf16_len(id) <= 64
}

/// One backup of THIS guild (another guild's id is not found), without its snapshot.
pub async fn get_backup(db: &PgPool, guild_id: &str, id: &str) -> anyhow::Result<Option<BackupInfo>> {
    if !usable_id(id) {
        return Ok(None);
    }
    let sql = format!(r#"SELECT {LIST_COLUMNS} FROM "ServerBackup" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#);
    Ok(fetch_infos(db, sql, vec![json!(id), json!(guild_id)]).await?.into_iter().next())
}

/// One backup of THIS guild with its snapshot (`getBackup(guildId, id, { withData: true })`).
pub async fn get_backup_with_data(db: &PgPool, guild_id: &str, id: &str) -> anyhow::Result<Option<ServerBackup>> {
    if !usable_id(id) {
        return Ok(None);
    }
    let row = sqlx::query_as::<_, ServerBackup>(r#"SELECT * FROM "ServerBackup" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(db)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(row)
}

/// Deletes one backup of this guild; the number deleted.
pub async fn delete_backup(db: &PgPool, guild_id: &str, id: &str) -> anyhow::Result<u64> {
    let res = sqlx::query(r#"DELETE FROM "ServerBackup" WHERE "id" = $1 AND "guildId" = $2"#)
        .bind(id)
        .bind(guild_id)
        .execute(db)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(res.rows_affected())
}

/// `{ manual, automatic }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct BackupCounts {
    pub manual: i64,
    pub automatic: i64,
}

pub async fn count_backups(db: &PgPool, guild_id: &str) -> anyhow::Result<BackupCounts> {
    let row = sqlx::query(
        r#"SELECT COUNT(*) FILTER (WHERE NOT "automatic")::bigint AS manual, COUNT(*) FILTER (WHERE "automatic")::bigint AS automatic FROM "ServerBackup" WHERE "guildId" = $1"#,
    )
    .bind(guild_id)
    .fetch_one(db)
    .await
    .map_err(bot_db::DbError::from)?;
    Ok(BackupCounts { manual: row.try_get("manual").unwrap_or(0), automatic: row.try_get("automatic").unwrap_or(0) })
}

/// The newest automatic backup: `(id, createdAt)`.
pub async fn newest_automatic(db: &PgPool, guild_id: &str) -> anyhow::Result<Option<(String, DateTime<Utc>)>> {
    let row = sqlx::query(r#"SELECT "id", "createdAt" FROM "ServerBackup" WHERE "guildId" = $1 AND "automatic" = true ORDER BY "createdAt" DESC, "id" DESC LIMIT 1"#)
        .bind(guild_id)
        .fetch_optional(db)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(match row {
        Some(r) => Some((r.try_get("id").map_err(bot_db::DbError::from)?, r.try_get::<chrono::NaiveDateTime, _>("createdAt").map_err(bot_db::DbError::from)?.and_utc())),
        None => None,
    })
}

/// Autocomplete rows: name (or kind) and date, newest first, at most 25. Like Prisma's `contains`, `%` and `_` in
/// the query are LIKE wildcards (not escaped).
pub async fn search_backups(db: &PgPool, guild_id: &str, query: &str) -> anyhow::Result<Vec<BackupInfo>> {
    let q = bot_util::js::utf16_prefix(bot_util::js::trim(query), 100).to_string();
    if q.is_empty() {
        let sql = format!(r#"SELECT {LIST_COLUMNS} FROM "ServerBackup" WHERE "guildId" = $1 ORDER BY "createdAt" DESC, "id" DESC LIMIT 25"#);
        return fetch_infos(db, sql, vec![json!(guild_id)]).await;
    }
    let sql = format!(r#"SELECT {LIST_COLUMNS} FROM "ServerBackup" WHERE "guildId" = $1 AND "name" ILIKE $2 ORDER BY "createdAt" DESC, "id" DESC LIMIT 25"#);
    fetch_infos(db, sql, vec![json!(guild_id), json!(format!("%{q}%"))]).await
}

/// Deletes the oldest AUTOMATIC backups beyond `keep`. Manual backups are never touched, and neither is `spare` (the one
/// a restore is reading). Returns how many went.
pub async fn prune_automatic(db: &PgPool, guild_id: &str, keep: Option<i64>, spare: &[String]) -> anyhow::Result<u64> {
    let k = clamp(keep.unwrap_or(7), 1, BACKUPS_AUTO_KEEP as i64);
    let rows = sqlx::query(r#"SELECT "id" FROM "ServerBackup" WHERE "guildId" = $1 AND "automatic" = true ORDER BY "createdAt" DESC, "id" DESC OFFSET $2"#)
        .bind(guild_id)
        .bind(k)
        .fetch_all(db)
        .await
        .map_err(bot_db::DbError::from)?;
    let ids: Vec<String> = rows.iter().filter_map(|r| r.try_get::<String, _>("id").ok()).filter(|id| !spare.contains(id)).collect();
    if ids.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query(r#"DELETE FROM "ServerBackup" WHERE "guildId" = $1 AND "automatic" = true AND "id" = ANY($2)"#)
        .bind(guild_id)
        .bind(&ids)
        .execute(db)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(res.rows_affected())
}

/// The raw statements of this module, for the rolled-back DB test.
pub const RAW_SQL: &[(&str, &str)] = &[
    ("countBackups", r#"SELECT COUNT(*) FILTER (WHERE NOT "automatic")::bigint AS manual, COUNT(*) FILTER (WHERE "automatic")::bigint AS automatic FROM "ServerBackup" WHERE "guildId" = $1"#),
    ("pruneAutomatic.select", r#"SELECT "id" FROM "ServerBackup" WHERE "guildId" = $1 AND "automatic" = true ORDER BY "createdAt" DESC, "id" DESC OFFSET $2"#),
];

// ── Making a backup ───────────────────────────────────────────────────────────

#[derive(Default)]
struct CreateLock(KeyedMutex);

pub async fn get_restore_job(app: &App, guild_id: &str) -> Option<ScheduledJob> {
    app.scheduler().get_job(&restore_job_key(guild_id)).await
}

/// The result of [`create_backup`].
#[derive(Debug, Clone, PartialEq)]
pub enum CreateResult {
    Ok(BackupInfo),
    /// `reason`: `restoring`, `cap` or `size`; `error`: the text for the user.
    Refused { reason: &'static str, error: String },
}

/// Takes a snapshot of the guild (from the cache, no Discord call) and stores it. Manual backups are capped by
/// `LIMITS.BACKUPS_MANUAL`; one over `BACKUP_BYTES` serialised is refused; none is made while a restore is running.
pub async fn create_backup(app: &App, guild: &GuildView, created_by_id: Option<&str>, name: Option<&str>, automatic: bool) -> anyhow::Result<CreateResult> {
    let lock = app.ext::<CreateLock>();
    let (app2, guild2) = (app.clone(), guild.clone());
    let (by, name) = (created_by_id.map(String::from), name.map(String::from));
    lock.0.run(&guild.id, move || async move { create_backup_locked(&app2, &guild2, by.as_deref(), name.as_deref(), automatic).await }).await
}

async fn create_backup_locked(app: &App, guild: &GuildView, created_by_id: Option<&str>, name: Option<&str>, automatic: bool) -> anyhow::Result<CreateResult> {
    if is_live(get_restore_job(app, &guild.id).await.as_ref()) {
        return Ok(CreateResult::Refused { reason: "restoring", error: "A restore is running. Wait until it is finished, then make the backup.".into() });
    }
    if !automatic {
        let manual: i64 = sqlx::query_scalar(r#"SELECT COUNT(*)::bigint FROM "ServerBackup" WHERE "guildId" = $1 AND "automatic" = false"#)
            .bind(&guild.id)
            .fetch_one(app.db())
            .await
            .map_err(bot_db::DbError::from)?;
        if manual >= BACKUPS_MANUAL as i64 {
            return Ok(CreateResult::Refused {
                reason: "cap",
                error: format!("This server already has {BACKUPS_MANUAL} manual backups. Delete one with `/backup delete` first (server owner only)."),
            });
        }
    }
    let snapshot = capture_guild(guild, now(app));
    let data = snapshot.to_json();
    let size = snapshot_bytes(&data);
    if size > BACKUP_BYTES {
        return Ok(CreateResult::Refused {
            reason: "size",
            error: format!("The backup would be {} bytes, over the limit of {}.", format_number(size as f64), format_number(BACKUP_BYTES as f64)),
        });
    }
    app.settings().ensure_guild_settings(&guild.id).await?;
    let row: ServerBackup = InsertBuilder::new(&SERVER_BACKUP_INFO)
        .values([
            ("guildId", json!(guild.id)),
            ("name", json!(clean_name(name))),
            ("automatic", json!(automatic)),
            ("createdById", json!(if automatic { None } else { created_by_id })),
            ("data", data),
            ("sizeBytes", json!(size)),
            ("roleCount", json!(snapshot.roles.len())),
            ("channelCount", json!(snapshot.categories.len() + snapshot.channels.len())),
        ])?
        .build()?
        .fetch_one(app.db())
        .await?;
    Ok(CreateResult::Ok(BackupInfo::of(&row)))
}

// ── backup.auto ───────────────────────────────────────────────────────────────

/// Makes sure the recurring backup.auto job exists: a healthy job is only pulled earlier, a FAILED, DONE or missing one
/// is revived. Never fails. `run_at` defaults to an hour from now.
pub async fn ensure_auto_job(app: &App, guild_id: &str, run_at: Option<DateTime<Utc>>) -> bool {
    let run_at = run_at.unwrap_or_else(|| at_ms(app.clock().now_ms() + B::AUTO_MAX_WAIT_MS));
    let job = app.scheduler().get_job(&auto_job_key(guild_id)).await;
    let mode = if is_live(job.as_ref()) { OnConflict::Earliest } else { OnConflict::Replace };
    match app
        .scheduler()
        .schedule_job(ScheduleJob::new(guild_id, JobType::BackupAuto, run_at).payload(json!({})).dedupe_key(auto_job_key(guild_id)).on_conflict(mode))
        .await
    {
        Ok(_) => true,
        Err(e) => {
            bot_util::log_error!(format!("backups: could not schedule backup.auto for guild {guild_id}"), bot_core::error::log_arg(&e));
            false
        }
    }
}

/// guildReady (always): guilds that never touched the module cost nothing; the others get their job back.
pub async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Backups)) && settings.backup_interval_hours <= 0 {
        return Ok(());
    }
    let jitter = (rand_unit() * B::STARTUP_JITTER_MS as f64).floor() as i64;
    let run_at = at_ms(ctx.app.clock().now_ms() + B::STARTUP_DELAY_MS + jitter);
    ensure_auto_job(&ctx.app, &ctx.data.guild_id.to_string(), Some(run_at)).await;
    Ok(())
}

fn rand_unit() -> f64 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    (n % 1_000_000) as f64 / 1_000_000.0
}

/// The backup.auto job (the guild comes from the cache).
pub async fn handle_auto_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let view = LiveGuild::new(&ctx.app, &job.guild_id).cached().map(|_| LiveGuild::new(&ctx.app, &job.guild_id).view());
    Ok(handle_auto_job_with(&ctx.app, &job.guild_id, view.as_ref()).await)
}

/// The backup.auto job with the guild given (`None`: not available). Reads the settings on every run, so a change
/// made on the dashboard is picked up within an hour. Never fails, so the recurring job cannot go FAILED.
pub async fn handle_auto_job_with(app: &App, guild_id: &str, guild: Option<&GuildView>) -> JobOutcome {
    let now_ms = app.clock().now_ms();
    let again = |ms: i64| JobOutcome::Reschedule { next_run_at: at_ms(now_ms + clamp(ms, B::AUTO_MIN_WAIT_MS, B::AUTO_MAX_WAIT_MS)), payload: None };
    let res: anyhow::Result<JobOutcome> = async {
        let Some(settings) = app.settings().get_settings_safe(guild_id).await else { return Ok(again(B::RETRY_MS)) };
        let hours = settings.backup_interval_hours as i64;
        if !is_module_enabled(Some(&settings), Some(ModuleKey::Backups)) || hours <= 0 {
            return Ok(again(B::AUTO_MAX_WAIT_MS)); // idle, but still watching
        }
        let Some(guild) = guild else { return Ok(again(B::RETRY_MS)) };
        let restore = get_restore_job(app, guild_id).await;
        if is_live(restore.as_ref()) {
            return Ok(again(10 * 60_000)); // a half-restored server is not worth keeping
        }
        let interval = hours * 3_600_000;
        let newest = newest_automatic(app.db(), guild_id).await?;
        let due_at = newest.as_ref().map(|(_, at)| at.timestamp_millis() + interval).unwrap_or(now_ms);
        if due_at > now_ms {
            return Ok(again(due_at - now_ms));
        }
        match create_backup(app, guild, None, None, true).await? {
            CreateResult::Refused { error, .. } => {
                bot_util::log_warn!(format!("backups: automatic backup of guild {guild_id} was not made: {error}"));
                Ok(again(B::AUTO_MAX_WAIT_MS))
            }
            CreateResult::Ok(made) => {
                let spare: Vec<String> =
                    restore.as_ref().and_then(|j| j.payload.get("backupId")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from).into_iter().collect();
                let pruned = prune_automatic(app.db(), guild_id, Some(settings.backup_keep as i64), &spare).await?;
                let extra = if pruned > 0 { format!(", {pruned} old one(s) removed") } else { String::new() };
                bot_util::log_info!(format!("backups: automatic backup {} of guild {guild_id} made{extra}", made.id));
                Ok(again(interval))
            }
        }
    }
    .await;
    match res {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("backup.auto failed for guild {guild_id}"), bot_core::error::log_arg(&e));
            again(B::RETRY_MS)
        }
    }
}

// ── Starting a restore ────────────────────────────────────────────────────────

/// Entries of a backup's stored id map whose new role or channel still exists (older restores of the same backup).
pub fn seed_id_map(guild: &GuildView, snapshot: &Snapshot) -> IndexMap<String, String> {
    snapshot.id_map.iter().filter(|(_, new)| guild.has_role_or_channel(new)).map(|(o, n)| (o.clone(), n.clone())).collect()
}

/// What an UNFINISHED earlier restore of the same backup got done (`carriedFrom`).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Carried {
    pub id_map: IndexMap<String, String>,
    pub made: Vec<String>,
    pub started_at: Option<i64>,
}

/// What an UNFINISHED earlier restore of the same backup got done: its job row stays FAILED with the last saved payload.
/// A new restore continues from it (its id map and what it made). A finished restore's row is gone.
pub fn carried_from(guild: &GuildView, job: Option<&ScheduledJob>, backup_id: &str, now_ms: i64) -> Carried {
    let Some(job) = job else { return Carried::default() };
    if job.status != JobStatus::Failed || job.payload.get("backupId").and_then(Value::as_str) != Some(backup_id) {
        return Carried::default();
    }
    let Some(prev) = normalise_restore_state(&job.payload, now_ms) else { return Carried::default() };
    Carried {
        id_map: prev.id_map.into_iter().filter(|(_, n)| guild.has_role_or_channel(n)).collect(),
        made: prev.made.into_iter().filter(|m| guild.has_role_or_channel(m)).collect(),
        started_at: Some(prev.started_at),
    }
}

/// The restore preview (counts, warnings) of a backup against the guild as it is now.
pub fn restore_preview(guild: &GuildView, snapshot: &Snapshot, mode: &str, prev_job: Option<&ScheduledJob>, backup_id: Option<&str>, now_ms: i64) -> Preview {
    let mut id_map = seed_id_map(guild, snapshot);
    id_map.extend(carried_from(guild, prev_job, backup_id.unwrap_or(""), now_ms).id_map);
    preview_restore(guild, snapshot, mode, &id_map)
}

/// Why [`start_restore`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRefused {
    Running,
    Mode,
}

/// Schedules the restore job. Refuses while one is live (`replace` would reset its progress and make everything twice).
pub async fn start_restore(
    app: &App,
    guild: &GuildView,
    backup_id: &str,
    snapshot: &Snapshot,
    mode: &str,
    started_by_id: Option<&str>,
    channel_id: Option<&str>,
) -> anyhow::Result<Result<(), StartRefused>> {
    if !RESTORE_MODES.contains(&mode) {
        return Ok(Err(StartRefused::Mode));
    }
    let prev = get_restore_job(app, &guild.id).await;
    if is_live(prev.as_ref()) {
        return Ok(Err(StartRefused::Running));
    }
    let now_ms = app.clock().now_ms();
    let carried = carried_from(guild, prev.as_ref(), backup_id, now_ms);
    // The start time is NOT carried: a wider adoption window would take in same-named roles or channels made since.
    let mut id_map = seed_id_map(guild, snapshot);
    id_map.extend(carried.id_map);
    let mut payload = snap::new_restore_state(backup_id, mode, started_by_id, channel_id, &id_map, now_ms);
    payload.made = carried.made;
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(&guild.id, JobType::BackupRestore, at_ms(now_ms + B::STEP_MS))
                .payload(payload.to_json())
                .dedupe_key(restore_job_key(&guild.id))
                .max_attempts(5)
                .on_conflict(OnConflict::Keep),
        )
        .await?;
    Ok(Ok(()))
}

// ── The restore job ───────────────────────────────────────────────────────────

/// The permissions a restore needs that the bot lacks: Manage Roles AND Manage Channels, both.
pub fn missing_restore_permissions(me: Option<&MeView>) -> Vec<&'static str> {
    let mut out = vec![];
    if !me.is_some_and(|m| m.has(snap::perm::MANAGE_ROLES)) {
        out.push("Manage Roles");
    }
    if !me.is_some_and(|m| m.has(snap::perm::MANAGE_CHANNELS)) {
        out.push("Manage Channels");
    }
    out
}

fn bold_and(missing: &[&str]) -> String {
    missing.iter().map(|p| format!("**{p}**")).collect::<Vec<_>>().join(" and ")
}

pub fn missing_permissions_embed(missing: &[&str]) -> CreateEmbed {
    error_embed(
        "Missing Permissions",
        Some(&format!(
            "I need {} to restore a backup: a channel I could not give its permissions would be open to everyone. Give my role {}, then run `/backup restore` again.",
            bold_and(missing),
            if missing.len() > 1 { "those permissions" } else { "that permission" }
        )),
        None,
    )
}

/// Everything the restore job needs from Discord: the engine's calls plus the summary post.
#[async_trait]
pub trait RestoreHost: RestoreApi {
    /// `guild.members.fetchMe()` (bounded), when the view has no `me`.
    async fn fetch_me(&self) -> Option<MeView>;
    /// `checkChannelUsable(channel).ok`.
    fn channel_usable(&self, channel_id: &str) -> bool;
    async fn send_channel(&self, channel_id: &str, post: &SummaryPost) -> Result<(), snap::ApiError>;
    async fn send_dm(&self, user_id: &str, embed: &CreateEmbed) -> Result<(), snap::ApiError>;
}

#[async_trait]
impl RestoreHost for LiveGuild {
    async fn fetch_me(&self) -> Option<MeView> {
        LiveGuild::fetch_me(self).await
    }
    fn channel_usable(&self, channel_id: &str) -> bool {
        LiveGuild::channel_usable(self, channel_id)
    }
    async fn send_channel(&self, channel_id: &str, post: &SummaryPost) -> Result<(), snap::ApiError> {
        LiveGuild::send_channel(self, channel_id, post).await
    }
    async fn send_dm(&self, user_id: &str, embed: &CreateEmbed) -> Result<(), snap::ApiError> {
        LiveGuild::send_dm(self, user_id, embed).await
    }
}

/// Guilds with a batch in flight in this process: a retry that finds one waits.
#[derive(Default)]
struct Batching(Mutex<HashSet<String>>);

/// The backup.restore job, against the live guild.
pub async fn handle_restore_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let host = LiveGuild::new(&ctx.app, &job.guild_id);
    handle_restore_job_with(&ctx.app, &job, ctx.late_by_ms, ctx.attempt, &host).await
}

/// The backup.restore job: one bounded batch per run. Returns a reschedule with the new payload while there is more to
/// do and `Done` when it is finished. Discord errors never escape; a database failure is an error (the scheduler retries).
pub async fn handle_restore_job_with(app: &App, job: &Job, late_by_ms: i64, attempt: i32, host: &dyn RestoreHost) -> BotResult<JobOutcome> {
    let batching = app.ext::<Batching>();
    let guild_id = job.guild_id.clone();
    let busy = { !batching.0.lock().map(|mut s| s.insert(guild_id.clone())).unwrap_or(false) };
    if busy {
        // The old run is still going or timed out: the next batch may repeat its work, so it may adopt what that run made.
        let payload = match &job.payload {
            Value::Object(m) => {
                let mut m = m.clone();
                m.insert("retry".into(), Value::Bool(true));
                Value::Object(m)
            }
            other => other.clone(),
        };
        return Ok(JobOutcome::Reschedule { next_run_at: at_ms(app.clock().now_ms() + B::STEP_MS), payload: Some(payload) });
    }
    struct Release(Arc<Batching>, String);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Ok(mut s) = self.0.0.lock() {
                s.remove(&self.1);
            }
        }
    }
    let _release = Release(batching.clone(), guild_id);
    run_restore(app, job, late_by_ms, attempt, host).await
}

async fn run_restore(app: &App, job: &Job, late_by_ms: i64, attempt: i32, host: &dyn RestoreHost) -> BotResult<JobOutcome> {
    let now_ms = app.clock().now_ms();
    let Some(mut state) = normalise_restore_state(&job.payload, now_ms) else {
        return Err(PermanentJobError::new("backup.restore: the job payload is unusable").into());
    };
    // Attempt 2 or later: an earlier run of this very payload did not finish, so it may have made roles or channels whose
    // ids were never saved. Only this batch may adopt them; it is cleared before saving.
    if attempt > 1 {
        state.retry = true;
    }
    let Some(row) = get_backup_with_data(app.db(), &job.guild_id, &state.backup_id).await? else {
        post_summary(host, &state, Some("The backup was deleted, so the restore stopped."), None).await;
        return Ok(JobOutcome::Done);
    };
    let snapshot = match parse_snapshot(&row.data) {
        Ok(s) => s,
        Err(SnapshotError(msg)) => {
            post_summary(host, &state, Some(&msg), None).await;
            return Ok(JobOutcome::Done);
        }
    };
    let mut view = host.view();
    if late_by_ms > B::STALE_MS {
        remember_ids(app, &row, &snapshot, &state, &view).await;
        post_summary(host, &state, Some("The restore waited too long (the bot was away from this server), so it was stopped. Start it again with `/backup restore`; what was already restored is detected and not made twice."), None).await;
        return Err(PermanentJobError::new("backup.restore: stopped because it waited too long (the bot was away from this server)").into());
    }
    if view.me.is_none() {
        view.me = host.fetch_me().await;
    }
    if view.me.is_none() {
        // Cannot judge permissions yet; payload unchanged.
        return Ok(JobOutcome::Reschedule { next_run_at: at_ms(now_ms + 30_000), payload: None });
    }
    let reason = format!(
        "Server backup restore {}{}",
        last_chars(&state.backup_id, 6),
        state.started_by_id.as_deref().map(|s| format!(" (started by {s})")).unwrap_or_default()
    );
    let missing = missing_restore_permissions(view.me.as_ref());
    if !missing.is_empty() {
        remember_ids(app, &row, &snapshot, &state, &view).await;
        let stopped = format!(
            "I no longer have {}, so the restore stopped before making anything it could not protect. Give my role {} back, then run `/backup restore` again; what was already restored is not made twice.",
            bold_and(&missing),
            if missing.len() > 1 { "those permissions" } else { "that permission" }
        );
        post_summary(host, &state, Some(&stopped), None).await;
        return Err(PermanentJobError::new(format!("backup.restore: stopped, the bot lacks {}", missing.join(" and "))).into());
    }
    let api: &dyn RestoreApi = host;
    let mut env = make_env(view, &snapshot, state, &reason, Some(api));
    let result = run_restore_batch(&mut env).await;
    let mut state = env.state.clone();
    let guild_after = env.guild.clone();
    drop(env);
    state.retry = false; // this batch's progress is saved below; the next one is a first try
    if result == BatchResult::More {
        return Ok(JobOutcome::Reschedule { next_run_at: at_ms(app.clock().now_ms() + B::STEP_MS), payload: Some(state.to_json()) });
    }
    remember_ids(app, &row, &snapshot, &state, &guild_after).await;
    post_summary(host, &state, None, Some(&snapshot.guild_name)).await;
    Ok(JobOutcome::Done)
}

/// `id.slice(-n)`.
fn last_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars[chars.len().saturating_sub(n)..].iter().collect()
}

/// Stores old id -> new id in the backup, so a second restore of it adopts these instead of recreating them.
async fn remember_ids(app: &App, row: &ServerBackup, snapshot: &Snapshot, state: &RestoreState, guild: &GuildView) {
    let mut merged = IndexMap::new();
    let mut all = snapshot.id_map.clone();
    all.extend(state.id_map.clone());
    for (o, n) in all {
        if guild.has_role_or_channel(&n) {
            merged.insert(o, n);
        }
    }
    if state.id_map.is_empty() {
        return;
    }
    let mut data = row.data.clone();
    if let Value::Object(m) = &mut data {
        m.insert("idMap".into(), json!(merged));
    }
    let res = sqlx::query(r#"UPDATE "ServerBackup" SET "data" = $1 WHERE "id" = $2"#).bind(&data).bind(&row.id).execute(app.db()).await;
    if let Err(e) = res {
        bot_util::log_warn!(format!("backups: could not store the id map of backup {}: {e}", row.id));
    }
}

// ── Summary ───────────────────────────────────────────────────────────────────

fn part(label: &str, n: i64) -> Option<String> {
    (n > 0).then(|| format!("{label} {}", format_number(n as f64)))
}

fn join_parts(parts: &[Option<String>]) -> String {
    let s = parts.iter().flatten().cloned().collect::<Vec<_>>().join(" · ");
    if s.is_empty() { "nothing to do".into() } else { s }
}

/// The embed posted when a restore ends (or is stopped).
pub fn build_summary_embed(state: &RestoreState, stopped: Option<&str>) -> CreateEmbed {
    let full = state.mode == "full";
    let desc = format!(
        "{}{}. **Nothing was deleted.**",
        stopped.map(|s| format!("{s}\n\n")).unwrap_or_default(),
        if full { "Mode **full**" } else { "Mode **missing only**" }
    );
    let mut embed = if stopped.is_some() { warn_embed("Restore Stopped", Some(&desc), None) } else { success_embed("Restore Finished", Some(&desc), None) };
    let c = |k: &str| state.count(k);
    embed = embed
        .field("Roles", join_parts(&[part("created", c("rolesCreated")), part("updated", c("rolesUpdated")), part("already there", c("rolesKept")), part("skipped", c("rolesSkipped"))]), false)
        .field(
            "Categories",
            join_parts(&[part("created", c("categoriesCreated")), part("updated", c("categoriesUpdated")), part("already there", c("categoriesKept")), part("skipped", c("categoriesSkipped"))]),
            true,
        )
        .field(
            "Channels",
            join_parts(&[part("created", c("channelsCreated")), part("updated", c("channelsUpdated")), part("already there", c("channelsKept")), part("skipped", c("channelsSkipped"))]),
            true,
        )
        .field(
            "Permissions and order",
            join_parts(&[
                part("channel permissions set", c("overwritesSet")),
                part("already right", c("overwritesKept")),
                part("skipped", c("overwritesSkipped")),
                part("positions set", c("positionsSet")),
                part("placed lower than before", c("positionsLimited")),
            ]),
            false,
        );
    let dropped = snap::bits_str(&state.dropped);
    let mut notes = vec![];
    if c("rolesSkipped") > 0 {
        notes.push("Roles are skipped when they belong to an integration, or (full mode) sit above my highest role.");
    }
    if c("positionsLimited") > 0 {
        notes.push("A role cannot be placed above my own highest role, so some were placed lower than before. Move my role up, then run the restore again.");
    }
    if !notes.is_empty() {
        embed = embed.field("Good to know", clamp_text(&notes.iter().map(|n| format!("• {n}")).collect::<Vec<_>>().join("\n"), 1024), false);
    }
    if c("masked") > 0 && dropped != 0 {
        embed = embed.field(
            "Permissions I could not give",
            clamp_text(
                &format!(
                    "{} role(s) or channel permission(s) were restored without: {}. I can only give permissions I have myself. Give my role those permissions, then run the restore again.",
                    format_number(c("masked") as f64),
                    bit_names(dropped).join(", ")
                ),
                1024,
            ),
            false,
        );
    }
    if !state.errors.is_empty() {
        let shown: Vec<&String> = state.errors.iter().take(8).collect();
        let more = c("errors") - shown.len() as i64;
        let body = format!(
            "{}{}",
            shown.iter().map(|e| format!("• {e}")).collect::<Vec<_>>().join("\n"),
            if more > 0 { format!("\n… and {} more", format_number(more as f64)) } else { String::new() }
        );
        embed = embed.field(format!("Problems ({})", format_number(c("errors") as f64)), clamp_text(&body, 1024), false);
    }
    embed
}

/// Posts the summary where the restore was started (the channel may have been recreated by the restore itself); falls
/// back to a DM to whoever started it. Never fails.
pub async fn post_summary(host: &dyn RestoreHost, state: &RestoreState, stopped: Option<&str>, _snapshot_name: Option<&str>) -> bool {
    let embed = build_summary_embed(state, stopped);
    let who = state.started_by_id.clone();
    let post = SummaryPost { content: who.as_ref().map(|w| format!("<@{w}>")), embed: embed.clone(), ping_user: who.clone() };
    let view = host.view();
    let channel = state.channel_id.as_deref().and_then(|c| {
        if view.channels.contains_key(c) {
            Some(c.to_string())
        } else {
            state.id_map.get(c).filter(|m| view.channels.contains_key(*m)).cloned()
        }
    });
    let res = async {
        if let Some(ch) = channel
            && host.channel_usable(&ch)
        {
            host.send_channel(&ch, &post).await?;
            return Ok(true);
        }
        if let Some(w) = &who {
            host.send_dm(w, &embed).await?;
            return Ok(true);
        }
        Ok::<bool, snap::ApiError>(false)
    }
    .await;
    match res {
        Ok(b) => b,
        Err(e) => {
            bot_util::log_warn!(format!("backups: could not post the restore summary for guild {}: {}", view.id, e.message));
            false
        }
    }
}

// ── Embeds for the command ────────────────────────────────────────────────────

pub fn owner_only_embed() -> CreateEmbed {
    error_embed(
        "Server Owner Only",
        Some("Only the owner of this server can restore or delete backups. A restore rebuilds roles and permissions, so it is kept away from every other account, administrators included."),
        None,
    )
}

pub const BACKUP_LIMITS_NOTE: &str = "A backup holds roles, categories, channels and their permission overwrites. Threads, messages, members and their roles, emojis, stickers, webhooks and bot settings are not included.";

pub fn build_info_embed(settings: Option<&GuildSettings>, row: &BackupInfo, snapshot: &Snapshot) -> CreateEmbed {
    let stats = snapshot_stats(snapshot);
    let made = if row.automatic {
        "Made automatically".to_string()
    } else {
        format!("Made by {}", row.created_by_id.as_deref().map(|u| format!("<@{u}>")).unwrap_or_else(|| "someone".into()))
    };
    guild_embed(settings, Some(&format!("💾 {}", row.display_name())), Some(&format!("{made} {}.", discord_timestamp(&row.created_at, "R"))), None)
        .field(
            "Contains",
            format!(
                "{} roles · {} categories · {} channels · {} channel permissions",
                format_number(stats.roles as f64),
                format_number(stats.categories as f64),
                format_number(stats.channels as f64),
                format_number(stats.overwrites as f64)
            ),
            false,
        )
        .field("Size", format!("{} bytes", format_number(row.size_bytes as f64)), true)
        .field("Server then", clamp_text(if snapshot.guild_name.is_empty() { "unknown" } else { &snapshot.guild_name }, 100), true)
        .field("Not included", BACKUP_LIMITS_NOTE, false)
        .footer(CreateEmbedFooter::new(format!("ID {}", row.id)))
}

pub fn build_list_embed(settings: Option<&GuildSettings>, rows: &[BackupInfo]) -> CreateEmbed {
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{} **{}** · {} · {} roles, {} channels",
                if r.automatic { "🤖" } else { "👤" },
                clamp_text(r.display_name(), 64),
                discord_timestamp(&r.created_at, "R"),
                format_number(r.role_count as f64),
                format_number(r.channel_count as f64)
            )
        })
        .collect();
    let desc = if lines.is_empty() { "No backups yet. Make one with `/backup create`.".to_string() } else { clamp_text(&lines.join("\n"), 4000) };
    guild_embed(settings, Some(&format!("💾 Backups ({})", rows.len())), Some(&desc), Some("Pick one by name with /backup info, export, restore or delete."))
}

fn estimate(calls: i64) -> String {
    let seconds = ((calls as f64 / B::CALLS_PER_BATCH as f64).ceil() as i64).max(1) * 5;
    if seconds < 60 {
        return "under a minute".into();
    }
    let minutes = (seconds as f64 / 60.0).ceil() as i64;
    format!("about {minutes} minute{}", if minutes == 1 { "" } else { "s" })
}

/// The confirmation embed of /backup restore: what will happen, in counts.
pub fn build_preview_embed(row: &BackupInfo, p: &Preview) -> CreateEmbed {
    let full = p.mode == "full";
    let desc = format!(
        "Backup **{}** from {}.\n\n{}\n**Nothing is ever deleted:** roles and channels that are not in the backup stay.",
        clamp_text(row.display_name(), 64),
        discord_timestamp(&row.created_at, "f"),
        if full {
            "**Full:** recreates what is missing, and also sets the names, permissions, order and channel permissions of roles and channels that still exist back to the backup."
        } else {
            "**Missing only:** recreates roles and channels that no longer exist, with their channel permissions. Channels that still exist only get the permissions of recreated roles added where they have none; nothing that exists is changed."
        }
    );
    let row3 = |b: &snap::PreviewCounts| join_parts(&[part("create", b.create), if full { part("update", b.update) } else { None }, part("leave", b.keep), part("skip", b.skip)]);
    let mut embed = warn_embed("Restore This Backup?", Some(&desc), None)
        .field("Roles", row3(&p.roles), false)
        .field("Categories", row3(&p.categories), true)
        .field("Channels", row3(&p.channels), true)
        .field("Channel permissions", if p.overwrites > 0 { format!("{} to set", format_number(p.overwrites as f64)) } else { "none to set".into() }, true)
        .field("Time", format!("{}; I post the result here when it is done.", estimate(p.calls)), true);
    let mut warnings: Vec<String> = vec![];
    if !p.can_manage_roles {
        warnings.push("I do not have **Manage Roles**: roles and channel permissions will be skipped.".into());
    }
    if !p.can_manage_channels {
        warnings.push("I do not have **Manage Channels**: categories and channels will be skipped.".into());
    }
    if p.masked > 0 && p.dropped != 0 {
        warnings.push(format!(
            "I lack {}, so {} role(s) or channel permission(s) will be restored without them. Give my role those permissions first for a complete restore.",
            bit_names(p.dropped).join(", "),
            format_number(p.masked as f64)
        ));
    }
    if p.managed_missing > 0 {
        warnings.push(format!("{} role(s) belong to integrations or bots and cannot be recreated.", format_number(p.managed_missing as f64)));
    }
    if p.role_total_after > B::ROLE_CAP {
        warnings.push(format!("The server would have {} roles; Discord allows {}, so some will fail.", format_number(p.role_total_after as f64), B::ROLE_CAP));
    }
    if p.channel_total_after > B::CHANNEL_CAP {
        warnings.push(format!("The server would have {} channels; Discord allows {}, so some will fail.", format_number(p.channel_total_after as f64), B::CHANNEL_CAP));
    }
    if p.unsupported > 0 {
        warnings.push(format!("{} channel(s) of a type a backup cannot hold were not saved.", format_number(p.unsupported as f64)));
    }
    if !warnings.is_empty() {
        embed = embed.field("Heads up", clamp_text(&warnings.iter().map(|w| format!("• {w}")).collect::<Vec<_>>().join("\n"), 1024), false);
    }
    embed.footer(CreateEmbedFooter::new("Confirm within 5 minutes. Only the server owner can confirm."))
}

/// The Restore / Cancel buttons (`backup:restore:{backupId}:{mode}`, `backup:cancel`).
pub fn restore_buttons(backup_id: &str, mode: &str) -> anyhow::Result<CreateActionRow> {
    Ok(CreateActionRow::Buttons(vec![
        CreateButton::new(bot_config::custom_id!("backup", "restore", backup_id, mode)?).label("Restore").style(ButtonStyle::Danger),
        CreateButton::new(bot_config::custom_id!("backup", "cancel")?).label("Cancel").style(ButtonStyle::Secondary),
    ]))
}

fn phase_label(p: &str) -> &'static str {
    match p {
        "roles" => "roles",
        "rolePositions" => "role order",
        "categories" => "categories",
        "channels" => "channels",
        "channelPositions" => "channel order",
        "overwrites" => "channel permissions",
        _ => "finishing",
    }
}

/// Text for /backup status: where a running restore is.
pub fn restore_status_text(job: Option<&ScheduledJob>, now_ms: i64) -> String {
    let Some(job) = job else { return "No restore is running.".into() };
    let p = normalise_restore_state(&job.payload, now_ms);
    if is_live(Some(job))
        && let Some(p) = p
    {
        return format!(
            "🔄 Restoring ({}), now on **{}**. Made so far: {} roles, {} channels. Started {}{}.",
            if p.mode == "full" { "full" } else { "missing only" },
            phase_label(&p.phase),
            format_number(p.count("rolesCreated") as f64),
            format_number((p.count("categoriesCreated") + p.count("channelsCreated")) as f64),
            discord_timestamp_ms(p.started_at as f64, "R"),
            p.started_by_id.as_deref().map(|u| format!(" by <@{u}>")).unwrap_or_default()
        );
    }
    if job.status == JobStatus::Failed {
        return format!(
            "⚠️ The last restore stopped with an error: {} Start it again with `/backup restore`; what was restored is not made twice.",
            clamp_text(job.last_error.as_deref().filter(|s| !s.is_empty()).unwrap_or("unknown"), 200)
        );
    }
    "No restore is running.".into()
}

/// What /backup status shows.
pub struct StatusInput<'a> {
    pub settings: Option<&'a GuildSettings>,
    pub counts: BackupCounts,
    pub newest: Option<DateTime<Utc>>,
    pub restore_job: Option<&'a ScheduledJob>,
    pub problems: Vec<String>,
    pub now_ms: i64,
}

pub fn build_status_embed(s: StatusInput<'_>) -> CreateEmbed {
    let hours = s.settings.map(|x| x.backup_interval_hours as i64).unwrap_or(0);
    let on = is_module_enabled(s.settings, Some(ModuleKey::Backups));
    let keep = clamp(s.settings.map(|x| x.backup_keep as i64).unwrap_or(7), 1, BACKUPS_AUTO_KEEP as i64);
    let auto = if hours <= 0 {
        "Off. Set the interval on the dashboard (Server Backups).".to_string()
    } else {
        let next = s.newest.map(|n| n.timestamp_millis() + hours * 3_600_000);
        format!(
            "Every **{hours}** hour{}, keeping the newest **{keep}**.{}{}{}",
            if hours == 1 { "" } else { "s" },
            match &s.newest {
                Some(n) => format!(" Last: {}.", discord_timestamp(n, "R")),
                None => " None made yet.".into(),
            },
            match next {
                Some(n) if n > s.now_ms => format!(" Next: {}.", discord_timestamp_ms(n as f64, "R")),
                _ => String::new(),
            },
            if on { "" } else { " **Paused: the module is off** (`/module enable backups`)." }
        )
    };
    let mut embed = guild_embed(s.settings, Some("💾 Server Backups"), None, None)
        .field("Automatic backups", auto, false)
        .field("Stored", format!("{} / {} manual · {} automatic", format_number(s.counts.manual as f64), BACKUPS_MANUAL, format_number(s.counts.automatic as f64)), true)
        .field("Restore", restore_status_text(s.restore_job, s.now_ms), false);
    if !s.problems.is_empty() {
        embed = embed.field("Needs attention", clamp_text(&s.problems.iter().map(|p| format!("• {p}")).collect::<Vec<_>>().join("\n"), 1024), false);
    }
    embed
}

// ── The persistent buttons ────────────────────────────────────────────────────

async fn edit(ctx: &ComponentCtx, embed: CreateEmbed) {
    let edit = serenity::all::EditInteractionResponse::new().embeds(vec![embed]).components(vec![]);
    if let Err(e) = ctx.edit_reply(edit).await {
        bot_util::log_debug!(format!("backups: edit failed: {e}"));
    }
}

/// `backup:restore:{backupId}:{mode}` and `backup:cancel` on the confirmation. SERVER OWNER only (checked here, before
/// any await); the confirmation expires 5 minutes after its message was sent. Works while the module is off.
pub async fn handle_backup_button(ctx: ComponentCtx) -> BotResult {
    if let Err(e) = backup_button(&ctx).await {
        bot_util::log_error!("backups: button failed", bot_core::error::log_arg(&e));
        safe_reply(
            &ctx.responder,
            Payload::embed(error_embed("Something Went Wrong", Some("The restore could not be started. Please try again in a moment."), None)).ephemeral(true),
        )
        .await;
    }
    Ok(())
}

async fn backup_button(ctx: &ComponentCtx) -> anyhow::Result<()> {
    let (_, params) = ctx.parsed();
    let action = params.first().cloned().unwrap_or_default();
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let live = LiveGuild::new(&ctx.app, &guild_id);
    let Some(cached) = live.cached().filter(|_| !guild_id.is_empty()) else {
        safe_reply(&ctx.responder, Payload::embed(error_embed("Try Again", Some("I cannot see this server right now. Please try again in a moment."), None)).ephemeral(true)).await;
        return Ok(());
    };
    if ctx.user().id != cached.owner_id {
        safe_reply(&ctx.responder, Payload::embed(owner_only_embed()).ephemeral(true)).await;
        return Ok(());
    }
    if action == "cancel" {
        let msg = Payload::embed(info_embed("Restore Cancelled", Some("Nothing was changed."), None)).components(vec![]).to_update();
        if let Err(e) = ctx.update(msg).await
            && !is_interaction_gone(&e)
        {
            return Err(e.into());
        }
        return Ok(());
    }
    let backup_id = params.get(1).cloned().unwrap_or_default();
    let mode = params.get(2).cloned().unwrap_or_default();
    if action != "restore" || backup_id.is_empty() || !RESTORE_MODES.contains(&mode.as_str()) {
        safe_reply(&ctx.responder, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    }
    let sent_ms = ctx.interaction.message.timestamp.timestamp_millis();
    if ctx.app.clock().now_ms() - sent_ms > B::CONFIRM_TTL_MS {
        let msg = Payload::embed(warn_embed("Confirmation Expired", Some("This confirmation is older than 5 minutes. Run `/backup restore` again."), None))
            .components(vec![])
            .to_update();
        if let Err(e) = ctx.update(msg).await
            && !is_interaction_gone(&e)
        {
            return Err(e.into());
        }
        return Ok(());
    }
    if let Err(e) = ctx.defer_update().await {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        return Err(e.into());
    }
    let Some(row) = get_backup_with_data(ctx.app.db(), &guild_id, &backup_id).await? else {
        edit(ctx, error_embed("Backup Not Found", Some("That backup no longer exists."), None)).await;
        return Ok(());
    };
    let snapshot = match parse_snapshot(&row.data) {
        Ok(s) => s,
        Err(SnapshotError(msg)) => {
            edit(ctx, error_embed("Backup Unreadable", Some(&msg), None)).await;
            return Ok(());
        }
    };
    let view = live.view();
    let missing = missing_restore_permissions(view.me.as_ref());
    if !missing.is_empty() {
        edit(ctx, missing_permissions_embed(&missing)).await;
        return Ok(());
    }
    let user_id = ctx.user().id.to_string();
    let channel_id = ctx.interaction.channel_id.to_string();
    if start_restore(&ctx.app, &view, &backup_id, &snapshot, &mode, Some(&user_id), Some(&channel_id)).await?.is_err() {
        edit(ctx, warn_embed("Restore Already Running", Some("A restore is already running on this server. Check `/backup status`."), None)).await;
        return Ok(());
    }
    bot_util::log_info!(format!("backups: restore of {backup_id} ({mode}) started in guild {guild_id} by {user_id}"));
    edit(
        ctx,
        success_embed(
            "Restore Started",
            Some(&format!(
                "Restoring in the background ({}). I post the result in <#{channel_id}> when it is done; `/backup status` shows the progress. Nothing is deleted.",
                if mode == "full" { "full" } else { "missing only" }
            )),
            None,
        ),
    )
    .await;
    Ok(())
}
