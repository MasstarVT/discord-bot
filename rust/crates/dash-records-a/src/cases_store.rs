//! Port of `src/dashboard/collections/cases.store.js`: the `cases` collection, Infraction rows of the guild, newest
//! first, in pages. Every write is done by the bot (dashboard tasks), because each one is a Discord action, a DM or a
//! case-log post: create → mod.action, update (reason/proof) → mod.editCase, clearWarnings → mod.clearWarnings.
//! Cases are never deleted.

use std::sync::Arc;

use async_trait::async_trait;
use bot_core::App;
use bot_db::Infraction;
use dash_collections::collections::cases::{MAX_CASE_ID, action_payload, case_item, case_number_query, reason_text};
use dash_collections::collections::levels::{PAGE_SIZE, PENDING_CREATE, load_profiles, search_user_ids};
use dash_collections::{
    ActionAnswer, DashboardDiscord, ListAnswer, ListOpts, ReadCtx, RecordError, RecordStore, Saved, StoreCtx, StoreError, TaskStatus,
};
use serde_json::{Map, Value, json};

use crate::common::{internal, require_tasks, row_json, run_task};

/// The `cases` store.
pub struct CasesStore {
    app: App,
}

impl CasesStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

fn not_found() -> StoreError {
    RecordError::not_found("That case does not exist in this server.").into()
}

/// `caseIdOf(id)`: the case number of an item id, or None.
pub fn case_id_of(id: &str) -> Option<i64> {
    if !(1..=10).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = id.parse().ok()?;
    (1..=MAX_CASE_ID).contains(&n).then_some(n)
}

/// `CURSOR_RE`: `c<1-10 digits>`.
fn cursor_case(cursor: &str) -> Option<i64> {
    let d = cursor.strip_prefix('c')?;
    ((1..=10).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

async fn find_case(app: &App, guild_id: &str, case_id: i64) -> Result<Option<Infraction>, StoreError> {
    sqlx::query_as::<_, Infraction>(r#"SELECT * FROM "Infraction" WHERE "guildId" = $1 AND "caseId" = $2"#)
        .bind(guild_id)
        .bind(case_id as i32)
        .fetch_optional(app.db())
        .await
        .map_err(internal)
}

/// `toItems(rows, discord)`: items with the names and avatars of their members and moderators.
async fn to_items(app: &App, rows: &[Infraction], discord: Option<&Arc<dyn DashboardDiscord>>) -> Vec<Value> {
    let ids: Vec<String> = rows.iter().flat_map(|r| [r.target_user_id.clone(), r.moderator_id.clone()]).collect();
    let profiles = load_profiles(discord, &ids).await;
    let now = app.clock().now_ms() as f64;
    rows.iter().map(|r| case_item(&row_json(r), &profiles, now)).collect()
}

async fn item_for(app: &App, guild_id: &str, case_id: i64, ctx: &StoreCtx) -> Result<Value, StoreError> {
    let Some(row) = find_case(app, guild_id, case_id).await? else { return Err(not_found()) };
    Ok(to_items(app, &[row], ctx.discord.as_ref()).await.remove(0))
}

#[async_trait]
impl RecordStore for CasesStore {
    async fn list(&self, guild_id: &str, opts: &ListOpts, ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let discord = ctx.discord.as_ref();
        let q = bot_util::js::trim(&opts.query);
        let mut targets: Option<Vec<String>> = None;
        if !q.is_empty() {
            if let Some(case_id) = case_number_query(q) {
                let row = find_case(&self.app, guild_id, case_id).await?;
                return Ok(ListAnswer::items(match row {
                    Some(r) => to_items(&self.app, &[r], discord).await,
                    None => Vec::new(),
                }));
            }
            let ids = search_user_ids(discord, guild_id, q).await?;
            if ids.is_empty() {
                return Ok(ListAnswer::items(Vec::new()));
            }
            targets = Some(ids);
        }
        let after = cursor_case(&opts.cursor).map(|n| n.min(MAX_CASE_ID));
        let mut sql = String::from(r#"SELECT * FROM "Infraction" WHERE "guildId" = $1"#);
        if targets.is_some() {
            sql.push_str(r#" AND "targetUserId" = ANY($2)"#);
        }
        if after.is_some() {
            sql.push_str(&format!(r#" AND "caseId" < ${}"#, if targets.is_some() { 3 } else { 2 }));
        }
        sql.push_str(&format!(r#" ORDER BY "caseId" DESC LIMIT {}"#, PAGE_SIZE + 1));
        let mut query = sqlx::query_as::<_, Infraction>(sqlx::AssertSqlSafe(sql)).bind(guild_id);
        if let Some(t) = &targets {
            query = query.bind(t.clone());
        }
        if let Some(a) = after {
            query = query.bind(a as i32);
        }
        let rows = query.fetch_all(self.app.db()).await.map_err(internal)?;
        let page = &rows[..rows.len().min(PAGE_SIZE)];
        let items = to_items(&self.app, page, discord).await;
        let next_cursor = (rows.len() > PAGE_SIZE).then(|| page.last().map(|l| format!("c{}", l.case_id))).flatten();
        Ok(ListAnswer { items, next_cursor })
    }

    async fn get(&self, guild_id: &str, id: &str, ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        let Some(case_id) = case_id_of(id) else { return Ok(None) };
        let Some(row) = find_case(&self.app, guild_id, case_id).await? else { return Ok(None) };
        Ok(to_items(&self.app, &[row], ctx.discord.as_ref()).await.into_iter().next())
    }

    /// Performs the moderation action (task mod.action) and answers the new case.
    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        require_tasks(ctx)?;
        let payload = action_payload(&data);
        let outcome = run_task(ctx, "mod.action", payload.clone()).await?;
        if outcome.status == TaskStatus::Pending {
            return Err(RecordError::unavailable(PENDING_CREATE).saved().into());
        }
        let user_id = payload.get("userId").and_then(Value::as_str).unwrap_or_default().to_string();
        tracing::info!(
            "Dashboard moderation {} of {user_id} in {guild_id} by {}: {}",
            dash_schema::js::to_js_string(payload.get("action")),
            ctx.user_id,
            outcome.message
        );
        let case_id = outcome.data.as_ref().and_then(|d| crate::common::int(d.get("caseId")));
        let mut row = match case_id.filter(|n| *n != 0) {
            Some(n) => find_case(&self.app, guild_id, n).await?,
            None => None,
        };
        // Without the task's data (its result expired in Redis), the newest case of that member is the one just made.
        if row.is_none() {
            row = sqlx::query_as::<_, Infraction>(
                r#"SELECT * FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 ORDER BY "caseId" DESC LIMIT 1"#,
            )
            .bind(guild_id)
            .bind(&user_id)
            .fetch_optional(self.app.db())
            .await
            .map_err(internal)?;
        }
        let Some(row) = row else {
            return Err(RecordError::unavailable(format!("{} Refresh the list to see the case.", outcome.message)).into());
        };
        Ok(Saved::item(to_items(&self.app, &[row], ctx.discord.as_ref()).await.remove(0)))
    }

    /// Changes the reason and/or proof link (task mod.editCase, like /case reason and /case proof).
    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let case_id = case_id_of(id);
        let row = match case_id {
            Some(n) => find_case(&self.app, guild_id, n).await?,
            None => None,
        };
        let (Some(row), Some(case_id)) = (row, case_id) else { return Err(not_found()) };
        let mut payload = Map::new();
        payload.insert("caseId".into(), json!(case_id));
        if let Some(reason) = data.get("reason")
            && !dash_schema::js::strict_eq(reason, &json!(reason_text(Some(&json!(row.reason)))))
        {
            payload.insert("reason".into(), reason.clone());
        }
        if let Some(proof) = data.get("proofUrl")
            && !proof.is_null()
            && !dash_schema::js::strict_eq(proof, &json!(row.proof_url))
        {
            payload.insert("proofUrl".into(), proof.clone());
        }
        if payload.len() == 1 {
            return Ok(Saved::item(to_items(&self.app, &[row], ctx.discord.as_ref()).await.remove(0)));
        }
        require_tasks(ctx)?;
        let outcome = run_task(ctx, "mod.editCase", Value::Object(payload)).await?;
        if outcome.status == TaskStatus::Pending {
            return Err(RecordError::unavailable(outcome.message).saved().into());
        }
        Ok(Saved::item(item_for(&self.app, guild_id, case_id, ctx).await?))
    }

    async fn remove(&self, _guild_id: &str, _id: &str, _ctx: &StoreCtx) -> Result<(), StoreError> {
        Err(RecordError::invalid("Moderation cases cannot be deleted. To pardon warnings, use \"Clear warnings\".").into())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["clearWarnings"]
    }

    /// /case clear-history for this case's member (task mod.clearWarnings: administrators only).
    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        if action_id != "clearWarnings" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        let case_id = case_id_of(id);
        let row = match case_id {
            Some(n) => find_case(&self.app, guild_id, n).await?,
            None => None,
        };
        let (Some(row), Some(case_id)) = (row, case_id) else { return Err(not_found()) };
        require_tasks(ctx)?;
        let reason = data.get("clearReason").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
        let outcome = run_task(ctx, "mod.clearWarnings", json!({ "userId": row.target_user_id, "reason": reason })).await?;
        Ok(ActionAnswer { item: Some(item_for(&self.app, guild_id, case_id, ctx).await?), message: Some(outcome.message) })
    }
}
