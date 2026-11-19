//! Port of `src/dashboard/collections/levels.store.js`: the `levels` collection, the UserLevel rows of the guild
//! ordered like the leaderboard (XP descending, then user id), in pages. Every change runs in the bot (task
//! `xp.adjust`, the same xpService calls as /levelconfig xp); the store then reads the member's row back.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bot_core::App;
use bot_db::UserLevel;
use dash_collections::collections::levels::{
    PAGE_SIZE, PENDING_CREATE, empty_level_item, is_user_id, level_item, load_profiles, rank_cursor, read_rank_cursor, search_user_ids,
};
use dash_collections::{
    ActionAnswer, DashboardDiscord, ListAnswer, ListOpts, MemberProfile, ReadCtx, RecordError, RecordStore, Saved, StoreCtx, StoreError,
    TaskStatus,
};
use serde_json::{Map, Value, json};

use crate::common::{internal, require_tasks, row_json, run_task, str_at};

/// The `levels` store.
pub struct LevelsStore {
    app: App,
}

impl LevelsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

const ORDER: &str = r#"ORDER BY "xp" DESC, "userId" ASC"#;

/// `rankOf(guildId, row)`: 1-based leaderboard position (more XP, or the same XP and a smaller id, ranks ahead).
async fn rank_of(app: &App, guild_id: &str, row: &UserLevel) -> Result<i64, StoreError> {
    let (n,): (i64,) = sqlx::query_as(
        r#"SELECT COUNT(*)::bigint FROM "UserLevel" WHERE "guildId" = $1 AND ("xp" > $2 OR ("xp" = $2 AND "userId" < $3))"#,
    )
    .bind(guild_id)
    .bind(row.xp)
    .bind(&row.user_id)
    .fetch_one(app.db())
    .await
    .map_err(internal)?;
    Ok(n + 1)
}

/// `toItems(guildId, rows, { discord, contiguous })`.
async fn to_items(app: &App, guild_id: &str, rows: &[UserLevel], discord: Option<&Arc<dyn DashboardDiscord>>, contiguous: bool) -> Result<Vec<Value>, StoreError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = rows.iter().map(|r| r.user_id.clone()).collect();
    let profiles = load_profiles(discord, &ids).await;
    let mut ranks = Vec::with_capacity(rows.len());
    if contiguous {
        let first = rank_of(app, guild_id, &rows[0]).await?;
        ranks.extend((0..rows.len() as i64).map(|i| first + i));
    } else {
        for r in rows {
            ranks.push(rank_of(app, guild_id, r).await?);
        }
    }
    Ok(rows.iter().zip(ranks).map(|(row, rank)| level_item(&row_json(row), Some(rank), profile(&profiles, &row.user_id))).collect())
}

fn profile<'a>(profiles: &'a HashMap<String, Option<MemberProfile>>, id: &str) -> Option<&'a MemberProfile> {
    profiles.get(id).and_then(Option::as_ref)
}

/// `readItem(guildId, userId, ctx)`.
async fn read_item(app: &App, guild_id: &str, user_id: &str, discord: Option<&Arc<dyn DashboardDiscord>>) -> Result<Option<Value>, StoreError> {
    let row = sqlx::query_as::<_, UserLevel>(r#"SELECT * FROM "UserLevel" WHERE "guildId" = $1 AND "userId" = $2"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)?;
    let Some(row) = row else { return Ok(None) };
    Ok(to_items(app, guild_id, &[row], discord, true).await?.into_iter().next())
}

struct Adjusted {
    item: Value,
    message: String,
    status: TaskStatus,
}

/// `adjust(guildId, userId, payload, ctx)`: one xp.adjust for a member, and the item as it is now.
async fn adjust(app: &App, guild_id: &str, user_id: &str, mut payload: Map<String, Value>, ctx: &StoreCtx) -> Result<Adjusted, StoreError> {
    require_tasks(ctx)?;
    payload.insert("userId".into(), json!(user_id));
    let outcome = run_task(ctx, "xp.adjust", Value::Object(payload)).await?;
    let item = match read_item(app, guild_id, user_id, ctx.discord.as_ref()).await? {
        Some(item) => item,
        None => {
            let profiles = load_profiles(ctx.discord.as_ref(), &[user_id.to_string()]).await;
            empty_level_item(user_id, profile(&profiles, user_id))
        }
    };
    Ok(Adjusted { item, message: outcome.message, status: outcome.status })
}

/// `userIdOf(id)`: a row id is the member's user id.
fn user_id_of(id: &str) -> Result<&str, StoreError> {
    if is_user_id(id) { Ok(id) } else { Err(RecordError::not_found("That member has no XP in this server.").into()) }
}

fn op_payload(op: &str, amount: Option<&Value>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("op".into(), json!(op));
    if let Some(a) = amount {
        m.insert("amount".into(), a.clone());
    }
    m
}

#[async_trait]
impl RecordStore for LevelsStore {
    async fn list(&self, guild_id: &str, opts: &ListOpts, ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let discord = ctx.discord.as_ref();
        let q = bot_util::js::trim(&opts.query);
        if !q.is_empty() {
            let ids = search_user_ids(discord, guild_id, q).await?;
            if ids.is_empty() {
                return Ok(ListAnswer::items(Vec::new()));
            }
            let sql = format!(r#"SELECT * FROM "UserLevel" WHERE "guildId" = $1 AND "userId" = ANY($2) {ORDER} LIMIT {PAGE_SIZE}"#);
            let rows = sqlx::query_as::<_, UserLevel>(sqlx::AssertSqlSafe(sql)).bind(guild_id).bind(&ids).fetch_all(self.app.db()).await.map_err(internal)?;
            return Ok(ListAnswer::items(to_items(&self.app, guild_id, &rows, discord, false).await?));
        }
        let take = PAGE_SIZE as i64 + 1;
        let rows = match read_rank_cursor(&opts.cursor) {
            Some(c) => {
                let sql = format!(r#"SELECT * FROM "UserLevel" WHERE "guildId" = $1 AND {} {ORDER} LIMIT $4"#, c.sql_after("xp", 2, 3));
                sqlx::query_as::<_, UserLevel>(sqlx::AssertSqlSafe(sql))
                    .bind(guild_id)
                    .bind(c.value as i32)
                    .bind(&c.user_id)
                    .bind(take)
                    .fetch_all(self.app.db())
                    .await
            }
            None => {
                let sql = format!(r#"SELECT * FROM "UserLevel" WHERE "guildId" = $1 {ORDER} LIMIT $2"#);
                sqlx::query_as::<_, UserLevel>(sqlx::AssertSqlSafe(sql)).bind(guild_id).bind(take).fetch_all(self.app.db()).await
            }
        }
        .map_err(internal)?;
        let page = &rows[..rows.len().min(PAGE_SIZE)];
        let items = to_items(&self.app, guild_id, page, discord, true).await?;
        let next_cursor = (rows.len() > PAGE_SIZE).then(|| page.last().map(|l| rank_cursor(i64::from(l.xp), &l.user_id))).flatten();
        Ok(ListAnswer { items, next_cursor })
    }

    async fn get(&self, guild_id: &str, id: &str, ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        if !is_user_id(id) {
            return Ok(None);
        }
        read_item(&self.app, guild_id, id, ctx.discord.as_ref()).await
    }

    /// Gives XP to a member (/levelconfig xp give) and answers their row.
    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let user_id = str_at(&data, "userId").unwrap_or_default().to_string();
        let a = adjust(&self.app, guild_id, &user_id, op_payload("give", data.get("amount")), ctx).await?;
        if a.status == TaskStatus::Pending {
            return Err(RecordError::unavailable(PENDING_CREATE).saved().into());
        }
        tracing::info!(
            "Dashboard XP give {} to {user_id} in {guild_id} by {}: {}",
            dash_schema::js::to_js_string(data.get("amount")),
            ctx.user_id,
            a.message
        );
        Ok(Saved::item(a.item))
    }

    async fn update(&self, _guild_id: &str, _id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> Result<Saved, StoreError> {
        Err(RecordError::invalid("Use Give XP, Remove XP, Set XP or Reset XP to change a member.").into())
    }

    async fn remove(&self, _guild_id: &str, _id: &str, _ctx: &StoreCtx) -> Result<(), StoreError> {
        Err(RecordError::invalid("Use \"Reset XP\" to wipe a member's XP.").into())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["give", "take", "set", "reset"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        let payload = match action_id {
            "give" => op_payload("give", data.get("amount")),
            "take" => op_payload("take", data.get("amount")),
            "set" => op_payload("set", data.get("xp")),
            "reset" => op_payload("reset", None),
            _ => return Err(RecordError::not_found("Unknown action.").into()),
        };
        let user_id = user_id_of(id)?;
        let a = adjust(&self.app, guild_id, user_id, payload, ctx).await?;
        Ok(ActionAnswer { item: Some(a.item), message: Some(a.message) })
    }
}
