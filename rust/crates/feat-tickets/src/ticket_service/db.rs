//! The DB access of `ticketService.js` for `TicketPanel`, `TicketPanelOption` and `Ticket` (§3.2, §4.2): reads, the
//! writes under advisory and row locks, the raw SQL list, the channelDelete cleanup, reconciliation and the
//! `ticket.managersync` back-fill job. Prisma's `mode: 'insensitive'` is `ILIKE` without escaping, ported as is.

use std::sync::Arc;

use bot_core::App;
use bot_core::scheduler::{Job, JobCtx, JobOutcome, SettleOutcome};
use bot_db::sqlx::{self, AssertSqlSafe, PgConnection, Row};
use bot_db::{
    DbError, InsertBuilder, PgPool, SQL_NOW, TICKET_PANEL_INFO, TICKET_PANEL_OPTION_INFO, Ticket, TicketPanel, TicketPanelOption, TicketPanelStyle,
    TxOptions, UpdateBuilder,
};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId};

use super::io::{
    ChannelState, OverwriteEdit, channel_cached, confirm_gone_channels, guild_available, invalidate_guild_panels, invalidate_panel, io, memo_epoch,
    memo_store, peek_panel, role_cached, sf,
};
use super::rules::{
    DEFAULT_OPTION_BUTTON_STYLE, DEFAULT_OPTION_EMOJI, DEFAULT_OPTION_LABEL, Fail, MANAGER_SYNC_DEADLINE_MS, MANAGER_SYNC_JOB_BUDGET_MS, MANAGER_SYNC_MAX,
    Outcome, PENDING_PREFIX, RECONCILE_FETCH_MAX, RowRef, STAFF_ALLOW, classify_ticket_rows, read_manager_sync, sync_payload_json,
};

// ── Shapes ────────────────────────────────────────────────────────────────────

/// A panel with its options ordered by position, then id (`PANEL_INCLUDE`).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelWithOptions {
    pub panel: TicketPanel,
    pub options: Vec<TicketPanelOption>,
}

impl PanelWithOptions {
    pub fn is_select(&self) -> bool {
        self.panel.style == TicketPanelStyle::Select
    }

    /// The JS object (`{ ...panel, options: [...] }`).
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(&self.panel).unwrap_or(Value::Null);
        v["options"] = serde_json::to_value(&self.options).unwrap_or(Value::Null);
        v
    }
}

/// A ticket with its panel and option (`include: { panel: true, option: true }`).
#[derive(Debug, Clone, PartialEq)]
pub struct TicketFull {
    pub ticket: Ticket,
    pub panel: Option<TicketPanel>,
    pub option: Option<TicketPanelOption>,
}

impl TicketFull {
    pub fn status(&self) -> &'static str {
        self.ticket.status.as_str()
    }

    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(&self.ticket).unwrap_or(Value::Null);
        v["panel"] = self.panel.as_ref().map(|p| serde_json::to_value(p).unwrap_or(Value::Null)).unwrap_or(Value::Null);
        v["option"] = self.option.as_ref().map(|o| serde_json::to_value(o).unwrap_or(Value::Null)).unwrap_or(Value::Null);
        v
    }
}

fn q(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

// ── Raw SQL (one entry per statement, with sample parameters) ─────────────────

/// `numberLockSql`: the per-guild ticket-number lock.
pub const NUMBER_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
/// `nextNumberSql`.
pub const NEXT_NUMBER_SQL: &str = r#"SELECT (COALESCE(MAX("number"), 0) + 1)::int AS next FROM "Ticket" WHERE "guildId" = $1"#;
/// `panelLockSql` / `optionLockSql`: the same advisory lock statement with other keys.
pub const PANEL_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
pub const OPTION_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
/// `panelRowLockSql`.
pub const PANEL_ROW_LOCK_SQL: &str = r#"SELECT 1 AS ok FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 FOR UPDATE"#;

/// `RAW_SQL`: name, statement and sample parameters.
pub const RAW_SQL: [(&str, &str, &[&str]); 5] = [
    ("tickets.numberLock", NUMBER_LOCK_SQL, &["ticket-number:0"]),
    ("tickets.nextNumber", NEXT_NUMBER_SQL, &["0"]),
    ("tickets.panelLock", PANEL_LOCK_SQL, &["ticket-panel:0"]),
    ("tickets.optionLock", OPTION_LOCK_SQL, &["ticket-option:0"]),
    ("tickets.panelRowLock", PANEL_ROW_LOCK_SQL, &["0", "0"]),
];

async fn advisory(conn: &mut PgConnection, key: String) -> Result<(), DbError> {
    sqlx::query(NUMBER_LOCK_SQL).bind(key).execute(conn).await?;
    Ok(())
}

// ── Reads ─────────────────────────────────────────────────────────────────────

/// The options of a panel, ordered.
pub async fn load_options(db: &PgPool, panel_id: &str) -> Result<Vec<TicketPanelOption>, DbError> {
    Ok(sqlx::query_as::<_, TicketPanelOption>(r#"SELECT * FROM "TicketPanelOption" WHERE "panelId" = $1 ORDER BY "position" ASC, "id" ASC"#)
        .bind(panel_id)
        .fetch_all(db)
        .await?)
}

async fn with_options(db: &PgPool, panel: Option<TicketPanel>) -> Result<Option<PanelWithOptions>, DbError> {
    match panel {
        None => Ok(None),
        Some(p) => {
            let options = load_options(db, &p.id).await?;
            Ok(Some(PanelWithOptions { panel: p, options }))
        }
    }
}

/// `prisma.ticketPanel.findFirst({ where: { id, guildId }, include: PANEL_INCLUDE })`.
pub async fn panel_by_id(db: &PgPool, guild_id: &str, panel_id: &str) -> Result<Option<PanelWithOptions>, DbError> {
    let p = sqlx::query_as::<_, TicketPanel>(r#"SELECT * FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(panel_id)
        .bind(guild_id)
        .fetch_optional(db)
        .await?;
    with_options(db, p).await
}

/// `getPanelForOpen(guildId, panelId)`: memo, then the database. A missing panel is not memoised.
pub async fn get_panel_for_open(app: &App, guild_id: &str, panel_id: &str) -> Result<Option<Arc<PanelWithOptions>>, DbError> {
    if let Some(hit) = peek_panel(app, guild_id, panel_id) {
        return Ok(Some(hit));
    }
    let epoch = memo_epoch(app);
    let panel = panel_by_id(app.db(), guild_id, panel_id).await?.map(Arc::new);
    if let Some(p) = &panel {
        memo_store(app, guild_id, epoch, p.clone());
    }
    Ok(panel)
}

/// `findPanel(guildId, ref)`: a panel by id or (case-insensitive) name, with its ordered options.
pub async fn find_panel(db: &PgPool, guild_id: &str, reference: &str) -> Result<Option<PanelWithOptions>, DbError> {
    let p = sqlx::query_as::<_, TicketPanel>(r#"SELECT * FROM "TicketPanel" WHERE "guildId" = $1 AND ("id" = $2 OR "name" ILIKE $2) LIMIT 1"#)
        .bind(guild_id)
        .bind(reference)
        .fetch_optional(db)
        .await?;
    with_options(db, p).await
}

/// One row of [`find_panels_for_autocomplete`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelChoice {
    pub id: String,
    pub name: String,
    pub style: TicketPanelStyle,
}

/// `findPanelsForAutocomplete(guildId, q)`: at most 25, by name.
pub async fn find_panels_for_autocomplete(db: &PgPool, guild_id: &str, q_text: &str) -> Result<Vec<PanelChoice>, DbError> {
    let rows = if q_text.is_empty() {
        sqlx::query(r#"SELECT "id", "name", "style" FROM "TicketPanel" WHERE "guildId" = $1 ORDER BY "name" ASC LIMIT 25"#).bind(guild_id).fetch_all(db).await?
    } else {
        sqlx::query(r#"SELECT "id", "name", "style" FROM "TicketPanel" WHERE "guildId" = $1 AND "name" ILIKE $2 ORDER BY "name" ASC LIMIT 25"#)
            .bind(guild_id)
            .bind(format!("%{q_text}%"))
            .fetch_all(db)
            .await?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(PanelChoice { id: r.try_get("id")?, name: r.try_get("name")?, style: r.try_get("style")? });
    }
    Ok(out)
}

/// One row of [`find_options_for_autocomplete`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionChoice {
    pub id: String,
    pub label: String,
    pub emoji: Option<String>,
}

/// `findOptionsForAutocomplete(guildId, panelRef, q)`.
pub async fn find_options_for_autocomplete(db: &PgPool, guild_id: &str, panel_ref: &str, q_text: &str) -> Result<Vec<OptionChoice>, DbError> {
    let base = r#"SELECT o."id", o."label", o."emoji" FROM "TicketPanelOption" o JOIN "TicketPanel" p ON p."id" = o."panelId" WHERE p."guildId" = $1 AND (p."id" = $2 OR p."name" ILIKE $2)"#;
    let rows = if q_text.is_empty() {
        sqlx::query(q(format!(r#"{base} ORDER BY o."position" ASC, o."id" ASC LIMIT 25"#))).bind(guild_id).bind(panel_ref).fetch_all(db).await?
    } else {
        sqlx::query(q(format!(r#"{base} AND o."label" ILIKE $3 ORDER BY o."position" ASC, o."id" ASC LIMIT 25"#)))
            .bind(guild_id)
            .bind(panel_ref)
            .bind(format!("%{q_text}%"))
            .fetch_all(db)
            .await?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(OptionChoice { id: r.try_get("id")?, label: r.try_get("label")?, emoji: r.try_get("emoji")? });
    }
    Ok(out)
}

async fn full(db: &PgPool, t: Option<Ticket>) -> Result<Option<TicketFull>, DbError> {
    let Some(ticket) = t else { return Ok(None) };
    let panel = match &ticket.panel_id {
        Some(id) => sqlx::query_as::<_, TicketPanel>(r#"SELECT * FROM "TicketPanel" WHERE "id" = $1"#).bind(id).fetch_optional(db).await?,
        None => None,
    };
    let option = match &ticket.option_id {
        Some(id) => sqlx::query_as::<_, TicketPanelOption>(r#"SELECT * FROM "TicketPanelOption" WHERE "id" = $1"#).bind(id).fetch_optional(db).await?,
        None => None,
    };
    Ok(Some(TicketFull { ticket, panel, option }))
}

/// `getTicketByChannel(guildId, channelId)`.
pub async fn get_ticket_by_channel(db: &PgPool, guild_id: &str, channel_id: &str) -> Result<Option<TicketFull>, DbError> {
    let t = sqlx::query_as::<_, Ticket>(r#"SELECT * FROM "Ticket" WHERE "guildId" = $1 AND "channelId" = $2 LIMIT 1"#)
        .bind(guild_id)
        .bind(channel_id)
        .fetch_optional(db)
        .await?;
    full(db, t).await
}

/// `getTicketById(guildId, ticketId)`.
pub async fn get_ticket_by_id(db: &PgPool, guild_id: &str, ticket_id: &str) -> Result<Option<TicketFull>, DbError> {
    let t = sqlx::query_as::<_, Ticket>(r#"SELECT * FROM "Ticket" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(ticket_id)
        .bind(guild_id)
        .fetch_optional(db)
        .await?;
    full(db, t).await
}

// ── Panel and option writes (§4.2) ────────────────────────────────────────────

/// The inputs of [`create_panel`] (JS defaults: BUTTONS, no reason form, 1 per member, `ticket-{number}`).
#[derive(Debug, Clone)]
pub struct NewPanel {
    pub guild_id: String,
    pub name: String,
    pub style: TicketPanelStyle,
    pub ask_reason: bool,
    pub max_open_per_user: i32,
    pub channel_name_format: String,
}

impl NewPanel {
    pub fn new(guild_id: &str, name: &str) -> Self {
        Self {
            guild_id: guild_id.into(),
            name: name.into(),
            style: TicketPanelStyle::Buttons,
            ask_reason: false,
            max_open_per_user: 1,
            channel_name_format: "ticket-{number}".into(),
        }
    }
}

/// `createPanel(...)`: creates a panel with its default "Open Ticket" option. The cap and the name check run under a
/// per-guild advisory lock. `PANEL_LIMIT` or `NAME_TAKEN` when refused; `Err` on a database failure.
pub async fn create_panel(app: &App, input: NewPanel) -> anyhow::Result<Outcome<PanelWithOptions>> {
    app.settings().ensure_guild_settings(&input.guild_id).await?;
    let name = input.name.clone();
    let r = bot_db::transaction(app.db(), TxOptions::new(2_000, 10_000), move |tx| {
        Box::pin(async move {
            advisory(tx, format!("ticket-panel:{}", input.guild_id)).await?;
            let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "TicketPanel" WHERE "guildId" = $1"#).bind(&input.guild_id).fetch_one(&mut *tx).await?;
            if count as usize >= bot_config::limits::TICKET_PANELS {
                return Ok::<_, DbError>(Err(Fail::new("PANEL_LIMIT")));
            }
            let clash: Option<String> = sqlx::query_scalar(r#"SELECT "id" FROM "TicketPanel" WHERE "guildId" = $1 AND "name" ILIKE $2 LIMIT 1"#)
                .bind(&input.guild_id)
                .bind(&input.name)
                .fetch_optional(&mut *tx)
                .await?;
            if clash.is_some() {
                return Ok(Err(Fail::with("NAME_TAKEN", json!({ "name": input.name }))));
            }
            let panel: TicketPanel = InsertBuilder::new(&TICKET_PANEL_INFO)
                .values([
                    ("guildId", json!(input.guild_id)),
                    ("name", json!(input.name)),
                    ("style", json!(input.style.as_str())),
                    ("askReason", json!(input.ask_reason)),
                    ("maxOpenPerUser", json!(input.max_open_per_user)),
                    ("channelNameFormat", json!(input.channel_name_format)),
                ])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            let option: TicketPanelOption = InsertBuilder::new(&TICKET_PANEL_OPTION_INFO)
                .values([
                    ("panelId", json!(panel.id)),
                    ("label", json!(DEFAULT_OPTION_LABEL)),
                    ("emoji", json!(DEFAULT_OPTION_EMOJI)),
                    ("buttonStyle", json!(DEFAULT_OPTION_BUTTON_STYLE)),
                    ("position", json!(0)),
                ])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            Ok(Ok(PanelWithOptions { panel, options: vec![option] }))
        })
    })
    .await;
    match r {
        Ok(v) => Ok(v),
        Err(e) if e.is_unique_violation() => Ok(Err(Fail::with("NAME_TAKEN", json!({ "name": name })))),
        Err(e) => Err(e.into()),
    }
}

/// `updatePanelFields(panelId, guildId, data)`: writes panel columns (guild-scoped) and drops the memo. `data` uses
/// the Prisma field names. A rename that collides case-insensitively is `NAME_TAKEN`; a missing panel
/// `PANEL_NOT_FOUND`.
pub async fn update_panel_fields(app: &App, panel_id: &str, guild_id: &str, data: &[(&str, Value)]) -> anyhow::Result<Outcome<()>> {
    let r = update_panel_fields_inner(app.db(), panel_id, guild_id, data).await;
    invalidate_panel(app, panel_id);
    r
}

async fn update_panel_fields_inner(db: &PgPool, panel_id: &str, guild_id: &str, data: &[(&str, Value)]) -> anyhow::Result<Outcome<()>> {
    let new_name = data.iter().find(|(k, _)| *k == "name").and_then(|(_, v)| v.as_str().map(str::to_string));
    if let Some(name) = &new_name {
        let clash: Option<String> = sqlx::query_scalar(r#"SELECT "id" FROM "TicketPanel" WHERE "guildId" = $1 AND "id" <> $2 AND "name" ILIKE $3 LIMIT 1"#)
            .bind(guild_id)
            .bind(panel_id)
            .bind(name)
            .fetch_optional(db)
            .await
            .map_err(DbError::from)?;
        if clash.is_some() {
            return Ok(Err(Fail::with("NAME_TAKEN", json!({ "name": name }))));
        }
    }
    let mut b = UpdateBuilder::new(&TICKET_PANEL_INFO);
    for (k, v) in data {
        b = b.set(k, v.clone())?;
    }
    let built = b.filter("id", json!(panel_id))?.filter("guildId", json!(guild_id))?.without_returning().build()?;
    match built.execute(db).await {
        Ok(n) if n > 0 => Ok(Ok(())),
        Ok(_) => Ok(Err(Fail::new("PANEL_NOT_FOUND"))),
        Err(e) if e.is_unique_violation() => Ok(Err(Fail::with("NAME_TAKEN", json!({ "name": new_name })))),
        Err(e) => Err(e.into()),
    }
}

/// The columns of a new option ([`add_option`]).
#[derive(Debug, Clone, Default)]
pub struct NewOption {
    pub label: String,
    pub emoji: Option<String>,
    pub description: Option<String>,
    pub button_style: i32,
    pub category_id: Option<String>,
}

/// `addOption(panelId, guildId, data)`: under a per-panel advisory lock (cap, duplicate label, next position). The new
/// option and the panel's option count.
pub async fn add_option(app: &App, panel_id: &str, guild_id: &str, data: NewOption) -> anyhow::Result<Outcome<(TicketPanelOption, usize)>> {
    let (pid, gid) = (panel_id.to_string(), guild_id.to_string());
    let r = bot_db::transaction(app.db(), TxOptions::new(2_000, 10_000), move |tx| {
        Box::pin(async move {
            advisory(tx, format!("ticket-option:{pid}")).await?;
            let found: Option<String> =
                sqlx::query_scalar(r#"SELECT "id" FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#).bind(&pid).bind(&gid).fetch_optional(&mut *tx).await?;
            if found.is_none() {
                return Ok::<_, DbError>(Err(Fail::new("PANEL_NOT_FOUND")));
            }
            let rows = sqlx::query(r#"SELECT "id", "label", "position" FROM "TicketPanelOption" WHERE "panelId" = $1"#).bind(&pid).fetch_all(&mut *tx).await?;
            if rows.len() >= bot_config::limits::TICKET_PANEL_OPTIONS {
                return Ok(Err(Fail::new("OPTION_LIMIT")));
            }
            let lower = data.label.to_lowercase();
            let mut max_pos: i64 = -1;
            for r in &rows {
                let label: String = r.try_get("label")?;
                if label.to_lowercase() == lower {
                    return Ok(Err(Fail::with("OPTION_DUP", json!({ "label": data.label }))));
                }
                max_pos = max_pos.max(i64::from(r.try_get::<i32, _>("position")?));
            }
            let option: TicketPanelOption = InsertBuilder::new(&TICKET_PANEL_OPTION_INFO)
                .values([
                    ("label", json!(data.label)),
                    ("emoji", json!(data.emoji)),
                    ("description", json!(data.description)),
                    ("buttonStyle", json!(data.button_style)),
                    ("categoryId", json!(data.category_id)),
                    ("panelId", json!(pid)),
                    ("position", json!(max_pos + 1)),
                ])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            Ok(Ok((option, rows.len() + 1)))
        })
    })
    .await;
    invalidate_panel(app, panel_id);
    Ok(r?)
}

/// `updateOption(panelId, guildId, optionId, data)`: under the option lock, so two renames cannot create a duplicate.
pub async fn update_option(app: &App, panel_id: &str, guild_id: &str, option_id: &str, data: Vec<(&'static str, Value)>) -> anyhow::Result<Outcome<TicketPanelOption>> {
    let (pid, gid, oid) = (panel_id.to_string(), guild_id.to_string(), option_id.to_string());
    let r = bot_db::transaction(app.db(), TxOptions::new(2_000, 10_000), move |tx| {
        Box::pin(async move {
            advisory(tx, format!("ticket-option:{pid}")).await?;
            let found: Option<String> =
                sqlx::query_scalar(r#"SELECT "id" FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#).bind(&pid).bind(&gid).fetch_optional(&mut *tx).await?;
            if found.is_none() {
                return Ok::<_, DbError>(Err(Fail::new("PANEL_NOT_FOUND")));
            }
            let rows = sqlx::query(r#"SELECT "id", "label" FROM "TicketPanelOption" WHERE "panelId" = $1"#).bind(&pid).fetch_all(&mut *tx).await?;
            let mut options: Vec<(String, String)> = Vec::new();
            for r in &rows {
                options.push((r.try_get("id")?, r.try_get("label")?));
            }
            if !options.iter().any(|(id, _)| *id == oid) {
                return Ok(Err(Fail::new("OPTION_NOT_FOUND")));
            }
            if let Some(label) = data.iter().find(|(k, _)| *k == "label").and_then(|(_, v)| v.as_str()) {
                let lower = label.to_lowercase();
                if options.iter().any(|(id, l)| *id != oid && l.to_lowercase() == lower) {
                    return Ok(Err(Fail::with("OPTION_DUP", json!({ "label": label }))));
                }
            }
            let mut b = UpdateBuilder::new(&TICKET_PANEL_OPTION_INFO);
            for (k, v) in &data {
                b = b.set(k, v.clone())?;
            }
            let option: TicketPanelOption = b.filter("id", json!(oid))?.build()?.fetch_one(&mut *tx).await?;
            Ok(Ok(option))
        })
    })
    .await;
    invalidate_panel(app, panel_id);
    Ok(r?)
}

/// `removeOption(panelId, guildId, optionId)`: a panel keeps at least one. Tickets of the option keep working.
pub async fn remove_option(app: &App, panel_id: &str, guild_id: &str, option_id: &str) -> anyhow::Result<Outcome<()>> {
    let (pid, gid, oid) = (panel_id.to_string(), guild_id.to_string(), option_id.to_string());
    let r = bot_db::transaction(app.db(), TxOptions::new(2_000, 10_000), move |tx| {
        Box::pin(async move {
            advisory(tx, format!("ticket-option:{pid}")).await?;
            let found: Option<String> =
                sqlx::query_scalar(r#"SELECT "id" FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#).bind(&pid).bind(&gid).fetch_optional(&mut *tx).await?;
            if found.is_none() {
                return Ok::<_, DbError>(Err(Fail::new("PANEL_NOT_FOUND")));
            }
            let ids: Vec<String> = sqlx::query_scalar(r#"SELECT "id" FROM "TicketPanelOption" WHERE "panelId" = $1"#).bind(&pid).fetch_all(&mut *tx).await?;
            if !ids.contains(&oid) {
                return Ok(Err(Fail::new("OPTION_NOT_FOUND")));
            }
            if ids.len() <= 1 {
                return Ok(Err(Fail::new("LAST_OPTION")));
            }
            sqlx::query(r#"DELETE FROM "TicketPanelOption" WHERE "id" = $1 AND "panelId" = $2"#).bind(&oid).bind(&pid).execute(&mut *tx).await?;
            Ok(Ok(()))
        })
    })
    .await;
    invalidate_panel(app, panel_id);
    Ok(r?)
}

/// `deletePanel(guild, panel)`: deletes a panel and its published message (best effort). The options cascade; tickets
/// keep their channels with `panelId = null`. Returns how many tickets were still not deleted.
pub async fn delete_panel(app: &App, guild_id: GuildId, panel: &TicketPanel) -> anyhow::Result<i64> {
    if let (Some(c), Some(m)) = (panel.channel_id.as_deref(), panel.message_id.as_deref())
        && channel_cached(app, guild_id, c).is_some()
        && let (Some(cid), Some(mid)) = (sf(c), sf(m))
        && let Err(e) = io(app).delete_message(ChannelId::new(cid), mid.into()).await
        && !matches!(e.code, Some(10003 | 10008 | 50001 | 50013))
    {
        bot_util::log_debug!(format!("tickets: could not delete the panel message: {}", e.message));
    }
    let g = guild_id.to_string();
    let open: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND "status" <> 'DELETED'"#)
        .bind(&g)
        .bind(&panel.id)
        .fetch_one(app.db())
        .await
        .map_err(DbError::from)?;
    sqlx::query(r#"DELETE FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2"#).bind(&panel.id).bind(&g).execute(app.db()).await.map_err(DbError::from)?;
    invalidate_panel(app, &panel.id);
    Ok(open)
}

/// The cached role ids of a guild, `None` when the guild is unavailable (pruning is then skipped).
pub fn cached_role_ids(app: &App, guild_id: GuildId) -> Option<Vec<String>> {
    if !guild_available(app, guild_id) {
        return None;
    }
    app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.roles.keys().map(|r| r.to_string()).collect()))
}

/// `updateManagers(panelId, guild, mutate)`: read-modify-write of `managerRoleIds` under a row lock. Roles that no
/// longer exist are pruned first; `mutate(pruned)` returns the new list or a refusal.
pub async fn update_managers(
    app: &App,
    panel_id: &str,
    guild_id: GuildId,
    mutate: impl FnOnce(Vec<String>) -> Outcome<Vec<String>> + Send + 'static,
) -> anyhow::Result<Outcome<Vec<String>>> {
    let roles = cached_role_ids(app, guild_id);
    let (pid, gid) = (panel_id.to_string(), guild_id.to_string());
    let r = bot_db::transaction(app.db(), TxOptions::new(2_000, 10_000), move |tx| {
        Box::pin(async move {
            let lock = sqlx::query(PANEL_ROW_LOCK_SQL).bind(&pid).bind(&gid).fetch_optional(&mut *tx).await?;
            if lock.is_none() {
                return Ok::<_, DbError>(Err(Fail::new("PANEL_NOT_FOUND")));
            }
            let current: Vec<String> =
                sqlx::query_scalar(r#"SELECT "managerRoleIds" FROM "TicketPanel" WHERE "id" = $1"#).bind(&pid).fetch_one(&mut *tx).await?;
            let pruned = match &roles {
                None => current,
                Some(live) => current.into_iter().filter(|id| live.contains(id)).collect(),
            };
            let next = match mutate(pruned) {
                Ok(v) => v,
                Err(f) => return Ok(Err(f)),
            };
            sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "managerRoleIds" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2"#)))
                .bind(&next)
                .bind(&pid)
                .execute(&mut *tx)
                .await?;
            Ok(Ok(next))
        })
    })
    .await;
    invalidate_panel(app, panel_id);
    Ok(r?)
}

/// Writes (add) or removes (remove) one manager role's overwrite on a ticket channel; false when Discord refused.
pub async fn write_manager_overwrite(app: &App, guild_id: GuildId, channel_id: ChannelId, role_id: &str, add: bool) -> bool {
    let r = if add {
        io(app)
            .edit_overwrite(
                guild_id,
                channel_id,
                OverwriteEdit {
                    id: role_id.to_string(),
                    kind: 0,
                    edits: STAFF_ALLOW.iter().map(|k| (*k, true)).collect(),
                    reason: "Ticket manager role added".into(),
                },
            )
            .await
    } else {
        io(app).delete_overwrite(channel_id, role_id.to_string(), "Ticket manager role removed".into()).await
    };
    match r {
        Ok(()) => true,
        Err(e) if !add && e.is(10009) => true,
        Err(e) => {
            bot_util::log_debug!(format!("tickets: manager overwrite sync failed in {channel_id}: {}", e.message));
            false
        }
    }
}

/// The result of [`sync_manager_overwrites`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncCounts {
    pub updated: i64,
    pub failed: i64,
    pub skipped: i64,
}

const ACTIVE_STATUSES: &str = r#""status" IN ('OPEN', 'CLAIMED', 'CLOSED')"#;

/// `syncManagerOverwrites({ guild, panelId, roleId, mode })`: back-fills a manager role change into the panel's
/// existing ticket channels (newest 50, sequentially, 60 s).
pub async fn sync_manager_overwrites(app: &App, guild_id: GuildId, panel_id: &str, role_id: &str, add: bool) -> anyhow::Result<SyncCounts> {
    let g = guild_id.to_string();
    let total: i64 = sqlx::query_scalar(q(format!(r#"SELECT COUNT(*) FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND {ACTIVE_STATUSES}"#)))
        .bind(&g)
        .bind(panel_id)
        .fetch_one(app.db())
        .await
        .map_err(DbError::from)?;
    let channels: Vec<String> = sqlx::query_scalar(q(format!(
        r#"SELECT "channelId" FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND {ACTIVE_STATUSES} ORDER BY "createdAt" DESC LIMIT {MANAGER_SYNC_MAX}"#
    )))
    .bind(&g)
    .bind(panel_id)
    .fetch_all(app.db())
    .await
    .map_err(DbError::from)?;
    let mut out = SyncCounts { skipped: (total - channels.len() as i64).max(0), ..Default::default() };
    let deadline = app.clock().now_ms() + MANAGER_SYNC_DEADLINE_MS;
    for c in channels {
        if app.clock().now_ms() > deadline {
            out.skipped += 1;
            continue;
        }
        let Some(ch) = channel_cached(app, guild_id, &c) else {
            out.skipped += 1;
            continue;
        };
        if write_manager_overwrite(app, guild_id, ch.id, role_id, add).await {
            out.updated += 1;
        } else {
            out.failed += 1;
        }
    }
    Ok(out)
}

// ── Manager back-fill job (ticket.managersync) ────────────────────────────────

struct Progress {
    done: i64,
    complete: bool,
    failed: i64,
}

async fn reconcile_panel_managers(app: &App, guild_id: GuildId, panel_id: &str, role_ids: &[String], from: i64, deadline: i64) -> anyhow::Result<Progress> {
    let g = guild_id.to_string();
    let managers: Option<Vec<String>> = sqlx::query_scalar(r#"SELECT "managerRoleIds" FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(panel_id)
        .bind(&g)
        .fetch_optional(app.db())
        .await
        .map_err(DbError::from)?;
    let Some(managers) = managers else { return Ok(Progress { done: 0, complete: true, failed: 0 }) };
    let add: Vec<&String> = role_ids.iter().filter(|id| managers.contains(id) && role_cached(app, guild_id, id)).collect();
    let remove: Vec<&String> = role_ids.iter().filter(|id| !managers.contains(id)).collect();
    let tickets: Vec<String> = sqlx::query_scalar(q(format!(
        r#"SELECT "channelId" FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND {ACTIVE_STATUSES} ORDER BY "createdAt" DESC LIMIT {MANAGER_SYNC_MAX}"#
    )))
    .bind(&g)
    .bind(panel_id)
    .fetch_all(app.db())
    .await
    .map_err(DbError::from)?;
    let mut failed = 0;
    let mut i = (from.max(0) as usize).min(tickets.len());
    while i < tickets.len() {
        if app.clock().now_ms() >= deadline {
            return Ok(Progress { done: i as i64, complete: false, failed });
        }
        if let Some(ch) = channel_cached(app, guild_id, &tickets[i]) {
            for id in &add {
                if !write_manager_overwrite(app, guild_id, ch.id, id, true).await {
                    failed += 1;
                }
            }
            for id in &remove {
                if !write_manager_overwrite(app, guild_id, ch.id, id, false).await {
                    failed += 1;
                }
            }
        }
        i += 1;
    }
    Ok(Progress { done: i as i64, complete: true, failed })
}

/// `runManagerSyncJob(job, { guild }, { budgetMs })`: the guild's one back-fill job (dedupe
/// `ticket.managersync:{guildId}`). Each run works for at most `budget_ms`, resuming each panel where the last run
/// stopped; what is left, and anything merged in meanwhile, goes to a follow-up run. It settles itself under the row
/// lock (`settleMergedJob`), so the handler reports `Done` either way.
pub async fn run_manager_sync_job(app: &App, job: &Job, guild_id: GuildId, budget_ms: i64) -> anyhow::Result<SettleOutcome> {
    let claimed = read_manager_sync(&job.payload);
    let deadline = app.clock().now_ms() + budget_ms;
    let mut progress: Vec<(String, i64, bool)> = Vec::new();
    let mut failed = 0;
    for (panel_id, entry) in &claimed {
        if app.clock().now_ms() >= deadline {
            break;
        }
        let p = reconcile_panel_managers(app, guild_id, panel_id, &entry.roles, entry.done, deadline).await?;
        failed += p.failed;
        progress.push((panel_id.clone(), p.done, p.complete));
    }
    let claimed_revs: Vec<(String, i64)> = claimed.iter().map(|(k, v)| (k.clone(), v.rev)).collect();
    let outcome = app
        .scheduler()
        .settle_merged_job(
            job,
            move |live| {
                let mut left = Vec::new();
                for (panel_id, entry) in read_manager_sync(live) {
                    let mine = progress.iter().find(|(k, _, _)| *k == panel_id);
                    let same_rev = claimed_revs.iter().any(|(k, rev)| *k == panel_id && *rev == entry.rev);
                    match mine {
                        Some((_, done, complete)) if same_rev => {
                            if *complete {
                                continue;
                            }
                            left.push((panel_id, super::rules::SyncEntry { done: *done, ..entry }));
                        }
                        _ => left.push((panel_id, entry)),
                    }
                }
                if left.is_empty() { None } else { Some(sync_payload_json(&left)) }
            },
            None,
        )
        .await?;
    let more = if outcome == SettleOutcome::Again { "more to do" } else { "done" };
    let fails = if failed > 0 { format!(", {failed} overwrite(s) failed (missing permissions)") } else { String::new() };
    bot_util::log_info!(format!("tickets: manager sync in guild {guild_id}: {more}{fails}"));
    Ok(outcome)
}

/// The `ticket.managersync` job handler.
pub async fn manager_sync_job(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let Some(g) = sf(&ctx.guild_id) else { return Ok(JobOutcome::Done) };
    run_manager_sync_job(&ctx.app, &job, GuildId::new(g), MANAGER_SYNC_JOB_BUDGET_MS).await?;
    Ok(JobOutcome::Done)
}

// ── Reconciliation and the panel list ─────────────────────────────────────────

/// `reconcileGuildTickets(guild)`: marks tickets whose channel Discord confirmed is gone and removes abandoned
/// placeholders. Never fails; `(deleted, pendingRemoved)`.
pub async fn reconcile_guild_tickets(app: &App, guild_id: GuildId) -> (u64, u64) {
    match reconcile_inner(app, guild_id).await {
        Ok(v) => v,
        Err(e) => {
            bot_util::log_warn!("tickets: reconcile failed", bot_core::error::log_arg(&e));
            (0, 0)
        }
    }
}

async fn reconcile_inner(app: &App, guild_id: GuildId) -> anyhow::Result<(u64, u64)> {
    if !guild_available(app, guild_id) {
        return Ok((0, 0));
    }
    let g = guild_id.to_string();
    let rows = sqlx::query(r#"SELECT "id", "channelId", "createdAt" FROM "Ticket" WHERE "guildId" = $1 AND "status" <> 'DELETED' ORDER BY "createdAt" DESC LIMIT 1000"#)
        .bind(&g)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?;
    let rows = row_refs(&rows)?;
    let c = classify_ticket_rows(&rows, &|id| channel_cached(app, guild_id, id).is_some(), app.clock().now_ms());
    let mut pending_removed = 0;
    let mut deleted = 0;
    if !c.stale_pending.is_empty() {
        pending_removed =
            sqlx::query(r#"DELETE FROM "Ticket" WHERE "id" = ANY($1)"#).bind(&c.stale_pending).execute(app.db()).await.map_err(DbError::from)?.rows_affected();
    }
    if !c.stale_gone.is_empty() {
        let candidates: Vec<&String> = c.stale_gone.iter().take(RECONCILE_FETCH_MAX).collect();
        let channel_of = |id: &str| rows.iter().find(|r| r.id == id).map(|r| r.channel_id.clone()).unwrap_or_default();
        let gone = confirm_gone_channels(app, guild_id, &candidates.iter().map(|id| channel_of(id)).collect::<Vec<_>>()).await;
        let ids: Vec<String> = candidates.into_iter().filter(|id| gone.contains(&channel_of(id))).cloned().collect();
        if !ids.is_empty() {
            deleted = sqlx::query(q(format!(
                r#"UPDATE "Ticket" SET "status" = 'DELETED', "updatedAt" = {SQL_NOW} WHERE "id" = ANY($1) AND "status" <> 'DELETED'"#
            )))
            .bind(&ids)
            .execute(app.db())
            .await
            .map_err(DbError::from)?
            .rows_affected();
        }
    }
    Ok((deleted, pending_removed))
}

/// Rows of `{ id, channelId, createdAt }` as [`RowRef`]s.
pub fn row_refs(rows: &[bot_db::sqlx::postgres::PgRow]) -> Result<Vec<RowRef>, DbError> {
    let mut out = Vec::new();
    for r in rows {
        let created: chrono::NaiveDateTime = r.try_get("createdAt")?;
        out.push(RowRef { id: r.try_get("id")?, channel_id: r.try_get("channelId")?, created_at_ms: created.and_utc().timestamp_millis() });
    }
    Ok(out)
}

/// `listPanelsView(guild)`: reconciles first, then each panel (by name, at most `TICKET_PANELS`) with its active
/// ticket count.
pub async fn list_panels_view(app: &App, guild_id: GuildId) -> anyhow::Result<Vec<(PanelWithOptions, i64)>> {
    reconcile_guild_tickets(app, guild_id).await;
    let g = guild_id.to_string();
    let panels = sqlx::query_as::<_, TicketPanel>(r#"SELECT * FROM "TicketPanel" WHERE "guildId" = $1 ORDER BY "name" ASC"#)
        .bind(&g)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?;
    let counts = sqlx::query(q(format!(r#"SELECT "panelId", COUNT(*) AS n FROM "Ticket" WHERE "guildId" = $1 AND {ACTIVE_STATUSES} GROUP BY "panelId""#)))
        .bind(&g)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?;
    let mut by_panel: Vec<(Option<String>, i64)> = Vec::new();
    for r in counts {
        by_panel.push((r.try_get("panelId").map_err(DbError::from)?, r.try_get("n").map_err(DbError::from)?));
    }
    let mut out = Vec::new();
    for p in panels.into_iter().take(bot_config::limits::TICKET_PANELS) {
        let active = by_panel.iter().find(|(id, _)| id.as_deref() == Some(p.id.as_str())).map(|(_, n)| *n).unwrap_or(0);
        let options = load_options(app.db(), &p.id).await?;
        out.push((PanelWithOptions { panel: p, options }, active));
    }
    Ok(out)
}

// ── channelDelete (§5.1) ──────────────────────────────────────────────────────

/// `onChannelDelete(ctx)`: a ticket channel marks its ticket DELETED (no transcript: the history is gone), and a deleted
/// category, transcript channel or panel channel is cleared from the panels. Never fails.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str, is_category: bool) {
    let r = on_channel_delete_inner(app.db(), guild_id, channel_id, is_category).await;
    match r {
        Ok(()) => invalidate_guild_panels(app, guild_id),
        Err(e) => bot_util::log_error!("tickets: channelDelete cleanup failed", bot_core::error::log_arg(&anyhow::Error::from(e))),
    }
}

async fn on_channel_delete_inner(db: &PgPool, guild_id: &str, channel_id: &str, is_category: bool) -> Result<(), DbError> {
    let (g, id) = (guild_id.to_string(), channel_id.to_string());
    bot_db::transaction(db, TxOptions::default(), move |tx| {
        Box::pin(async move {
            if is_category {
                for field in ["openCategoryId", "claimedCategoryId", "closedCategoryId"] {
                    sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "{field}" = NULL, "updatedAt" = {SQL_NOW} WHERE "guildId" = $1 AND "{field}" = $2"#)))
                        .bind(&g)
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                }
                sqlx::query(r#"UPDATE "TicketPanelOption" SET "categoryId" = NULL WHERE "categoryId" = $1 AND "panelId" IN (SELECT "id" FROM "TicketPanel" WHERE "guildId" = $2)"#)
                    .bind(&id)
                    .bind(&g)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query(q(format!(
                    r#"UPDATE "Ticket" SET "status" = 'DELETED', "updatedAt" = {SQL_NOW} WHERE "guildId" = $1 AND "channelId" = $2 AND "status" <> 'DELETED'"#
                )))
                .bind(&g)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(q(format!(
                    r#"UPDATE "TicketPanel" SET "channelId" = NULL, "messageId" = NULL, "updatedAt" = {SQL_NOW} WHERE "guildId" = $1 AND "channelId" = $2"#
                )))
                .bind(&g)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(q(format!(
                    r#"UPDATE "TicketPanel" SET "transcriptChannelId" = NULL, "updatedAt" = {SQL_NOW} WHERE "guildId" = $1 AND "transcriptChannelId" = $2"#
                )))
                .bind(&g)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            }
            Ok::<_, DbError>(())
        })
    })
    .await
}

// ── Small writes used by the flows ────────────────────────────────────────────

/// `prisma.ticketPanel.updateMany({ where: { id, guildId }, data: { channelId: null, messageId: null } })`.
pub async fn clear_panel_message(app: &App, panel_id: &str, guild_id: &str) -> Result<(), DbError> {
    sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "channelId" = NULL, "messageId" = NULL, "updatedAt" = {SQL_NOW} WHERE "id" = $1 AND "guildId" = $2"#)))
        .bind(panel_id)
        .bind(guild_id)
        .execute(app.db())
        .await?;
    invalidate_panel(app, panel_id);
    Ok(())
}

/// Marks a ticket DELETED unless it already is.
pub async fn mark_deleted(db: &PgPool, ticket_id: &str) -> Result<u64, DbError> {
    Ok(sqlx::query(q(format!(r#"UPDATE "Ticket" SET "status" = 'DELETED', "updatedAt" = {SQL_NOW} WHERE "id" = $1 AND "status" <> 'DELETED'"#)))
        .bind(ticket_id)
        .execute(db)
        .await?
        .rows_affected())
}

/// The member's OPEN/CLAIMED tickets on a panel, as `{ id, channelId, createdAt }`.
pub async fn live_rows(db: &PgPool, guild_id: &str, panel_id: &str, owner_id: &str) -> Result<Vec<RowRef>, DbError> {
    let rows = sqlx::query(
        r#"SELECT "id", "channelId", "createdAt" FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND "ownerId" = $3 AND "status" IN ('OPEN', 'CLAIMED') ORDER BY "createdAt" ASC"#,
    )
    .bind(guild_id)
    .bind(panel_id)
    .bind(owner_id)
    .fetch_all(db)
    .await?;
    row_refs(&rows)
}

/// Whether a channel id is a placeholder of a ticket being created.
pub fn is_pending(channel_id: &str) -> bool {
    channel_id.starts_with(PENDING_PREFIX)
}

/// A channel that resolved as present or unknown counts as live.
pub fn counts_as_live(state: ChannelState) -> bool {
    state != ChannelState::Gone
}
