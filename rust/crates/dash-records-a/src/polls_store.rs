//! Port of `src/dashboard/collections/polls.store.js`: the `polls` collection. Reads are plain queries (vote counts
//! included); every write is a dashboard task the bot runs with /poll's own service. The first page holds every active
//! poll plus the newest ended ones, and `nextCursor` loads older ones.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::Poll;
use dash_collections::collections::giveaways_polls::{PollTally, poll_item};
use dash_collections::{
    ActionAnswer, DashboardDiscord, ListAnswer, ListOpts, ReadCtx, RecordError, RecordStore, Saved, StoreCtx, StoreError, TaskStatus,
};
use serde_json::{Map, Value, json};

use crate::common::{internal, iso_ms, now_ms_naive, row_json, run_task};
use crate::giveaways_store::{is_item_id, names_for, read_history_cursor};

/// Ended polls per page.
pub const HISTORY_PAGE: usize = 25;

/// `votersSql(ids)`: distinct voters per poll (a multi-choice voter has several vote rows), one statement per page,
/// with one `$n` per id (`Prisma.join`).
pub fn voters_sql(n: usize) -> String {
    let params: Vec<String> = (1..=n.max(1)).map(|i| format!("${i}")).collect();
    format!(
        "SELECT \"pollId\", COUNT(DISTINCT \"userId\")::int AS voters FROM \"PollVote\"
  WHERE \"pollId\" IN ({}) GROUP BY \"pollId\"",
        params.join(",")
    )
}

/// The `polls` store.
pub struct PollsStore {
    app: App,
}

impl PollsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

fn not_found() -> StoreError {
    RecordError::not_found("That poll no longer exists.").into()
}

/// `talliesFor(rows)`: pollId -> { counts, votes, voters }.
async fn tallies_for(app: &App, rows: &[Poll]) -> Result<HashMap<String, PollTally>, StoreError> {
    let mut out = HashMap::new();
    if rows.is_empty() {
        return Ok(out);
    }
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let groups: Vec<(String, i32, i64)> = sqlx::query_as(
        r#"SELECT "pollId", "optionIndex", COUNT(*)::bigint FROM "PollVote" WHERE "pollId" = ANY($1) GROUP BY "pollId", "optionIndex""#,
    )
    .bind(&ids)
    .fetch_all(app.db())
    .await
    .map_err(internal)?;
    let sql = voters_sql(ids.len());
    let mut vq = sqlx::query_as::<_, (String, i32)>(sqlx::AssertSqlSafe(sql));
    for id in &ids {
        vq = vq.bind(id);
    }
    let voters = vq.fetch_all(app.db()).await.map_err(internal)?;
    for r in rows {
        out.insert(r.id.clone(), PollTally { counts: vec![0; r.options.len()], votes: 0, voters: 0 });
    }
    for (poll_id, index, n) in groups {
        if let Some(t) = out.get_mut(&poll_id)
            && index >= 0
            && (index as usize) < t.counts.len()
        {
            t.counts[index as usize] = n;
            t.votes += n;
        }
    }
    for (poll_id, n) in voters {
        if let Some(t) = out.get_mut(&poll_id) {
            t.voters = i64::from(n);
        }
    }
    Ok(out)
}

async fn items_for(app: &App, rows: &[Poll], discord: Option<&Arc<dyn DashboardDiscord>>) -> Result<Vec<Value>, StoreError> {
    let tallies = tallies_for(app, rows).await?;
    let authors: Vec<String> = rows.iter().map(|r| r.author_id.clone()).collect();
    let names = names_for(discord, &authors).await;
    Ok(rows.iter().map(|r| poll_item(&row_json(r), tallies.get(&r.id), &names)).collect())
}

async fn find_row(app: &App, guild_id: &str, id: &str) -> Result<Option<Poll>, StoreError> {
    if !is_item_id(id) {
        return Ok(None);
    }
    sqlx::query_as::<_, Poll>(r#"SELECT * FROM "Poll" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)
}

async fn load_item(app: &App, guild_id: &str, id: &str, discord: Option<&Arc<dyn DashboardDiscord>>) -> Result<Option<Value>, StoreError> {
    let Some(row) = find_row(app, guild_id, id).await? else { return Ok(None) };
    Ok(items_for(app, &[row], discord).await?.into_iter().next())
}

/// `startPayload(d)`: the poll.start payload.
pub fn start_payload(d: &Map<String, Value>) -> Value {
    let mut v = json!({
        "question": d.get("question"),
        "options": match d.get("options") { None | Some(Value::Null) => json!([]), Some(o) => o.clone() },
        "channelId": d.get("channelId"),
        "maxChoices": d.get("maxChoices"),
        "duration": crate::common::nn(d.get("duration")),
        "allowedRoleIds": match d.get("allowedRoleIds") { None | Some(Value::Null) => json!([]), Some(o) => o.clone() },
        "deniedRoleIds": match d.get("deniedRoleIds") { None | Some(Value::Null) => json!([]), Some(o) => o.clone() },
        "hideResults": d.get("hideResults") == Some(&Value::Bool(true)),
    });
    if let Value::Object(m) = &mut v {
        for k in ["question", "channelId", "maxChoices"] {
            if !d.contains_key(k) {
                m.shift_remove(k);
            }
        }
    }
    v
}

#[async_trait]
impl RecordStore for PollsStore {
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
        let search = if q.is_empty() { "" } else { r#" AND "question" ILIKE $2"# };
        let pattern = format!("%{q}%");
        let mut rows: Vec<Poll> = Vec::new();
        if after.is_none() {
            let sql = format!(
                r#"SELECT * FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ACTIVE'{search} ORDER BY "createdAt" DESC, "id" DESC LIMIT {}"#,
                limits::ACTIVE_POLLS + 50
            );
            let mut query = sqlx::query_as::<_, Poll>(sqlx::AssertSqlSafe(sql)).bind(guild_id);
            if !q.is_empty() {
                query = query.bind(&pattern);
            }
            rows = query.fetch_all(self.app.db()).await.map_err(internal)?;
        }
        let n = if q.is_empty() { 2 } else { 3 };
        let after_sql =
            if after.is_some() { format!(r#" AND ("createdAt" < ${n} OR ("createdAt" = ${n} AND "id" < ${}))"#, n + 1) } else { String::new() };
        let sql = format!(
            r#"SELECT * FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ENDED'{search}{after_sql} ORDER BY "createdAt" DESC, "id" DESC LIMIT {}"#,
            HISTORY_PAGE + 1
        );
        let mut query = sqlx::query_as::<_, Poll>(sqlx::AssertSqlSafe(sql)).bind(guild_id);
        if !q.is_empty() {
            query = query.bind(&pattern);
        }
        if let Some((ms, id)) = &after {
            query = query.bind(now_ms_naive(*ms)).bind(id.clone());
        }
        let mut ended = query.fetch_all(self.app.db()).await.map_err(internal)?;
        let more = ended.len() > HISTORY_PAGE;
        ended.truncate(HISTORY_PAGE);
        let next_cursor = if more {
            ended.last().and_then(|l| Some(format!("{}:{}", iso_ms(row_json(l).get("createdAt"))?, l.id)))
        } else {
            None
        };
        rows.extend(ended);
        let items = items_for(&self.app, &rows, ctx.discord.as_ref()).await?;
        Ok(ListAnswer { items, next_cursor })
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        // loadItem(guildId, id): no discord, so the author stays an id.
        load_item(&self.app, guild_id, id, None).await
    }

    /// Posts a poll (/poll create).
    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        crate::common::require_tasks(ctx)?;
        let since = now_ms_naive(self.app.clock().now_ms() - 2000);
        let out = run_task(ctx, "poll.start", start_payload(&data)).await?;
        // `if (!id)`: an empty id is as good as none.
        let mut id = out.data.as_ref().and_then(|d| d.get("pollId")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        // The answer is kept in Redis; without it (or while the bot is still working) look the new poll up.
        if id.is_none() {
            let r: Option<(String,)> = sqlx::query_as(
                r#"SELECT "id" FROM "Poll" WHERE "guildId" = $1 AND "authorId" = $2 AND "createdAt" >= $3 ORDER BY "createdAt" DESC LIMIT 1"#,
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
                    "The poll was posted, but it could not be loaded. Reload the list.".into()
                };
                Err(RecordError::unavailable(message).saved().into())
            }
        }
    }

    async fn update(&self, _guild_id: &str, _id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> Result<Saved, StoreError> {
        Err(RecordError::invalid("A posted poll cannot be edited. End it, or delete it and post a new one.").into())
    }

    /// Removes the poll, its votes and its message (ending it silently first when it is still active).
    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
        crate::common::require_tasks(ctx)?;
        if find_row(&self.app, guild_id, id).await?.is_none() {
            return Err(not_found());
        }
        run_task(ctx, "poll.delete", json!({ "pollId": id })).await?;
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["end"]
    }

    /// /poll end
    async fn action(&self, guild_id: &str, id: &str, action_id: &str, _data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        if action_id != "end" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        crate::common::require_tasks(ctx)?;
        if find_row(&self.app, guild_id, id).await?.is_none() {
            return Err(not_found());
        }
        let out = run_task(ctx, "poll.end", json!({ "pollId": id })).await?;
        let Some(item) = load_item(&self.app, guild_id, id, ctx.discord.as_ref()).await? else { return Err(not_found()) };
        Ok(ActionAnswer { item: Some(item), message: Some(out.message) })
    }
}
