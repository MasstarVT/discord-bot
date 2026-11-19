//! Port of `src/dashboard/collections/giveaways.store.js`: the `giveaways` collection. Reads are plain queries; every
//! write is a dashboard task the bot runs with /giveaway's own service. The list is paged: the first page holds every
//! running and draft giveaway plus the newest finished ones, and `nextCursor` loads older finished ones.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::Giveaway;
use dash_collections::collections::giveaways_polls::giveaway_item;
use dash_collections::{
    ActionAnswer, DashboardDiscord, ListAnswer, ListOpts, ReadCtx, RecordError, RecordStore, Saved, StoreCtx, StoreError, TaskStatus,
};
use serde_json::{Map, Value, json};

use crate::common::{internal, nn, now_ms_naive, row_json, run_task};

/// Finished giveaways per page.
pub const HISTORY_PAGE: usize = 25;
/// Most user lookups one list makes (discord.getUsers takes at most 100).
const NAME_LOOKUPS: usize = 100;

/// The `giveaways` store.
pub struct GiveawaysStore {
    app: App,
}

impl GiveawaysStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

fn not_found() -> StoreError {
    RecordError::not_found("That giveaway no longer exists.").into()
}

/// `ID_RE`: `/^[A-Za-z0-9_-]{1,40}$/`.
pub fn is_item_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `CURSOR_RE`: `/^(\d{1,15}):([A-Za-z0-9]{1,40})$/` as (ms, id).
pub fn read_history_cursor(cursor: &str) -> Option<(i64, String)> {
    let (ms, id) = cursor.split_once(':')?;
    let ok = (1..=15).contains(&ms.len())
        && ms.bytes().all(|b| b.is_ascii_digit())
        && (1..=40).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then(|| ms.parse().ok().map(|m| (m, id.to_string()))).flatten()
}

/// `namesFor(discord, ids)`: display names of users by id, best effort.
pub async fn names_for(discord: Option<&Arc<dyn DashboardDiscord>>, ids: &[String]) -> HashMap<String, String> {
    let mut wanted: Vec<String> = Vec::new();
    for id in ids {
        if !wanted.contains(id) {
            wanted.push(id.clone());
        }
    }
    wanted.truncate(NAME_LOOKUPS);
    let mut names = HashMap::new();
    let discord = crate::common::discord_with(discord, dash_collections::DiscordMethod::GetUsers);
    let Some(discord) = discord.filter(|_| !wanted.is_empty()) else { return names };
    if let Ok(users) = discord.get_users(&wanted).await {
        for (id, user) in users {
            let Some(u) = user else { continue };
            let name = u.global_name.filter(|g| !g.is_empty()).unwrap_or(u.username);
            if !name.is_empty() {
                names.insert(id, name);
            }
        }
    }
    names
}

const SELECT_WITH_ENTRIES: &str =
    r#"SELECT g.*, (SELECT COUNT(*)::bigint FROM "GiveawayEntry" e WHERE e."giveawayId" = g."id") AS "_entries" FROM "Giveaway" g"#;

/// A row with `_count.entries`, as Prisma's include answers it.
fn with_entries(row: &sqlx::postgres::PgRow) -> Result<Value, StoreError> {
    let g: Giveaway = sqlx::FromRow::from_row(row).map_err(internal)?;
    let n: i64 = sqlx::Row::try_get(row, "_entries").map_err(internal)?;
    let mut v = row_json(&g);
    v["_count"] = json!({ "entries": n });
    Ok(v)
}

/// `itemsFor(rows, discord)`: host and winner names resolved in one lookup (hosts first, then winners in list order).
async fn items_for(rows: &[Value], discord: Option<&Arc<dyn DashboardDiscord>>) -> Vec<Value> {
    let mut ids: Vec<String> = rows.iter().filter_map(|r| r.get("hostId").and_then(Value::as_str).map(str::to_string)).collect();
    for r in rows {
        for w in r.get("winnerIds").and_then(Value::as_array).into_iter().flatten() {
            if let Some(w) = w.as_str() {
                ids.push(w.to_string());
            }
        }
    }
    let names = names_for(discord, &ids).await;
    rows.iter().map(|r| giveaway_item(r, &names)).collect()
}

async fn load_row(app: &App, guild_id: &str, id: &str) -> Result<Option<Value>, StoreError> {
    if !is_item_id(id) {
        return Ok(None);
    }
    let sql = format!(r#"{SELECT_WITH_ENTRIES} WHERE g."id" = $1 AND g."guildId" = $2 LIMIT 1"#);
    let row = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).bind(guild_id).fetch_optional(app.db()).await.map_err(internal)?;
    row.as_ref().map(with_entries).transpose()
}

async fn exists(app: &App, guild_id: &str, id: &str) -> Result<bool, StoreError> {
    if !is_item_id(id) {
        return Ok(false);
    }
    let r: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "Giveaway" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)?;
    Ok(r.is_some())
}

/// `loadItem(guildId, id, discord)`.
async fn load_item(app: &App, guild_id: &str, id: &str, discord: Option<&Arc<dyn DashboardDiscord>>) -> Result<Option<Value>, StoreError> {
    let Some(row) = load_row(app, guild_id, id).await? else { return Ok(None) };
    Ok(items_for(&[row], discord).await.into_iter().next())
}

/// `startPayload(d)`: the giveaway.start payload.
pub fn start_payload(d: &Map<String, Value>) -> Value {
    let list = |k: &str| match d.get(k) {
        None | Some(Value::Null) => json!([]),
        Some(v) => v.clone(),
    };
    let v = json!({
        "prize": d.get("prize").cloned(),
        "description": nn(d.get("description")),
        "channelId": d.get("channelId").cloned(),
        "winnerCount": d.get("winnerCount").cloned(),
        "duration": nn(d.get("duration")),
        "endsAt": nn(d.get("endsAt")),
        "requiredRoleIds": list("requiredRoleIds"),
        "deniedRoleIds": list("deniedRoleIds"),
        "roleWeights": list("roleWeights"),
        "xpPrize": nn(d.get("xpPrize")),
        "coinPrize": nn(d.get("coinPrize")),
        "pingRoleId": nn(d.get("pingRoleId")),
        "embedColor": nn(d.get("embedColor")),
    });
    defined_only(v, d, &["prize", "channelId", "winnerCount"])
}

/// Drops the keys whose value is JS undefined (absent in the input: `JSON.stringify` leaves them out).
fn defined_only(v: Value, d: &Map<String, Value>, keys: &[&str]) -> Value {
    let mut v = v;
    if let Value::Object(m) = &mut v {
        for k in keys {
            if !d.contains_key(*k) {
                m.shift_remove(*k);
            }
        }
    }
    v
}

impl GiveawaysStore {
    /// `act(guildId, id, ctx, kind, payload)`: a task about one existing giveaway, then the fresh item and the message.
    async fn act(&self, guild_id: &str, id: &str, ctx: &StoreCtx, kind: &str, payload: Map<String, Value>) -> Result<ActionAnswer, StoreError> {
        crate::common::require_tasks(ctx)?;
        if !exists(&self.app, guild_id, id).await? {
            return Err(not_found());
        }
        let mut p = Map::new();
        p.insert("giveawayId".into(), json!(id));
        p.extend(payload);
        let out = run_task(ctx, kind, Value::Object(p)).await?;
        let Some(item) = load_item(&self.app, guild_id, id, ctx.discord.as_ref()).await? else { return Err(not_found()) };
        Ok(ActionAnswer { item: Some(item), message: Some(out.message) })
    }
}

#[async_trait]
impl RecordStore for GiveawaysStore {
    async fn list(&self, guild_id: &str, opts: &ListOpts, ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let q = bot_util::js::utf16_prefix(bot_util::js::trim(&opts.query), 100).to_string();
        let after = if opts.cursor.is_empty() {
            None
        } else {
            match read_history_cursor(&opts.cursor) {
                Some(c) => Some(c),
                None => return Ok(ListAnswer::items(Vec::new())),
            }
        };
        let search = if q.is_empty() { "" } else { r#" AND g."prize" ILIKE $2"# };
        let pattern = format!("%{q}%");
        let mut open: Vec<Value> = Vec::new();
        if after.is_none() {
            let sql = format!(
                r#"{SELECT_WITH_ENTRIES} WHERE g."guildId" = $1 AND g."status" IN ('ACTIVE', 'DRAFT'){search} ORDER BY g."endsAt" ASC, g."id" ASC LIMIT {}"#,
                limits::ACTIVE_GIVEAWAYS + 50
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guild_id);
            if !q.is_empty() {
                query = query.bind(&pattern);
            }
            for r in query.fetch_all(self.app.db()).await.map_err(internal)? {
                open.push(with_entries(&r)?);
            }
            // Running ones (soonest end first) before drafts.
            open.sort_by_key(|r| r.get("status").and_then(Value::as_str) != Some("ACTIVE"));
        }
        let n = if q.is_empty() { 2 } else { 3 };
        let after_sql = if after.is_some() {
            format!(r#" AND (g."createdAt" < ${n} OR (g."createdAt" = ${n} AND g."id" < ${}))"#, n + 1)
        } else {
            String::new()
        };
        let sql = format!(
            r#"{SELECT_WITH_ENTRIES} WHERE g."guildId" = $1 AND g."status" IN ('ENDED', 'CANCELLED'){search}{after_sql} ORDER BY g."createdAt" DESC, g."id" DESC LIMIT {}"#,
            HISTORY_PAGE + 1
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guild_id);
        if !q.is_empty() {
            query = query.bind(&pattern);
        }
        if let Some((ms, id)) = &after {
            query = query.bind(now_ms_naive(*ms)).bind(id.clone());
        }
        let mut done: Vec<Value> = Vec::new();
        for r in query.fetch_all(self.app.db()).await.map_err(internal)? {
            done.push(with_entries(&r)?);
        }
        let more = done.len() > HISTORY_PAGE;
        done.truncate(HISTORY_PAGE);
        let next_cursor = if more {
            done.last().and_then(|l| {
                let ms = crate::common::iso_ms(l.get("createdAt"))?;
                Some(format!("{ms}:{}", l.get("id")?.as_str()?))
            })
        } else {
            None
        };
        open.extend(done);
        let items = items_for(&open, ctx.discord.as_ref()).await;
        Ok(ListAnswer { items, next_cursor })
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        // loadItem(guildId, id): no discord, so names stay ids.
        load_item(&self.app, guild_id, id, None).await
    }

    /// Starts a giveaway (/giveaway start).
    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        crate::common::require_tasks(ctx)?;
        let since = now_ms_naive(self.app.clock().now_ms() - 2000);
        let out = run_task(ctx, "giveaway.start", start_payload(&data)).await?;
        // `if (!id)`: an empty id is as good as none.
        let mut id =
            out.data.as_ref().and_then(|d| d.get("giveawayId")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        // The answer is kept in Redis; without it (or while the bot is still working) look the new giveaway up.
        if id.is_none() {
            let r: Option<(String,)> = sqlx::query_as(
                r#"SELECT "id" FROM "Giveaway" WHERE "guildId" = $1 AND "hostId" = $2 AND "createdAt" >= $3 ORDER BY "createdAt" DESC LIMIT 1"#,
            )
            .bind(guild_id)
            .bind(&ctx.user_id)
            .bind(since)
            .fetch_optional(self.app.db())
            .await
            .map_err(internal)?;
            id = r.map(|(i,)| i);
        }
        let item = match &id {
            Some(id) => load_item(&self.app, guild_id, id, ctx.discord.as_ref()).await?,
            None => None,
        };
        match item {
            Some(item) => Ok(Saved::item(item)),
            None => {
                let message = if out.status == TaskStatus::Pending {
                    out.message
                } else {
                    "The giveaway was started, but it could not be loaded. Reload the list.".into()
                };
                Err(RecordError::unavailable(message).saved().into())
            }
        }
    }

    /// Changes the bonus odds (/giveaway odds add and remove): the only thing a posted giveaway lets you edit.
    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        crate::common::require_tasks(ctx)?;
        if !exists(&self.app, guild_id, id).await? {
            return Err(not_found());
        }
        if let Some(weights) = data.get("roleWeights") {
            run_task(ctx, "giveaway.odds", json!({ "giveawayId": id, "roleWeights": weights })).await?;
        }
        match load_item(&self.app, guild_id, id, ctx.discord.as_ref()).await? {
            Some(item) => Ok(Saved::item(item)),
            None => Err(not_found()),
        }
    }

    async fn remove(&self, _guild_id: &str, _id: &str, _ctx: &StoreCtx) -> Result<(), StoreError> {
        Err(RecordError::invalid("Giveaways cannot be deleted. Cancel a running one; finished ones stay as history.").into())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["end", "reroll", "restart", "cancel"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        match action_id {
            "end" => self.act(guild_id, id, ctx, "giveaway.end", Map::new()).await,
            "reroll" => {
                let p = defined_only(json!({ "winners": data.get("winners") }), &data, &["winners"]);
                self.act(guild_id, id, ctx, "giveaway.reroll", p.as_object().cloned().unwrap_or_default()).await
            }
            "restart" => {
                let p = json!({ "channelId": nn(data.get("channelId")), "duration": nn(data.get("duration")), "endsAt": nn(data.get("endsAt")) });
                self.act(guild_id, id, ctx, "giveaway.restart", p.as_object().cloned().unwrap_or_default()).await
            }
            "cancel" => self.act(guild_id, id, ctx, "giveaway.cancel", Map::new()).await,
            _ => Err(RecordError::not_found("Unknown action.").into()),
        }
    }
}
