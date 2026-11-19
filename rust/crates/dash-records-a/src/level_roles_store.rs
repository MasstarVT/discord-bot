//! Port of `src/dashboard/collections/levelRoles.store.js`: the `levelRoles` collection (the LevelRole table). Same
//! rules as /levelconfig role add|remove: one reward per level, one level per role, at most LIMITS.LEVEL_ROLES per
//! server. Nothing is cached anywhere, so there is nothing to invalidate.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{InsertBuilder, LEVEL_ROLE_INFO, LevelRole};
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordStore, Saved, StoreCtx, StoreError};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::common::{TX_OPTS, TxError, int, internal, lock_guild_settings, str_at};

/// The `levelRoles` store.
pub struct LevelRolesStore {
    app: App,
}

impl LevelRolesStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

/// `toItem(row)`.
pub fn level_role_item(row: &LevelRole) -> Value {
    json!({ "id": row.id, "level": row.level, "roleId": row.role_id, "title": format!("Level {}", row.level) })
}

/// `assertFree(tx, guildId, { level, roleId }, exceptId)`: a 'conflict' when the level or the role already has a reward.
async fn assert_free(tx: &mut PgConnection, guild_id: &str, level: i64, role_id: &str, except_id: Option<&str>) -> Result<(), TxError> {
    let except = if except_id.is_some() { r#" AND "id" <> $3"# } else { "" };
    let q = format!(r#"SELECT "roleId" FROM "LevelRole" WHERE "guildId" = $1 AND "level" = $2{except} LIMIT 1"#);
    let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(q)).bind(guild_id).bind(level as i32);
    if let Some(e) = except_id {
        query = query.bind(e);
    }
    if query.fetch_optional(&mut *tx).await?.is_some() {
        return Err(RecordError::new(dash_collections::RecordErrorCode::Conflict, format!("Level {level} already has a reward. Edit that reward instead."))
            .with_path("level")
            .into());
    }
    let q = format!(r#"SELECT "level" FROM "LevelRole" WHERE "guildId" = $1 AND "roleId" = $2{except} LIMIT 1"#);
    let mut query = sqlx::query_scalar::<_, i32>(sqlx::AssertSqlSafe(q)).bind(guild_id).bind(role_id);
    if let Some(e) = except_id {
        query = query.bind(e);
    }
    if let Some(level) = query.fetch_optional(&mut *tx).await? {
        return Err(RecordError::new(
            dash_collections::RecordErrorCode::Conflict,
            format!("That role is already the reward for level {level}. Remove that reward first, or pick another role."),
        )
        .with_path("roleId")
        .into());
    }
    Ok(())
}

fn unique_error() -> StoreError {
    RecordError::new(dash_collections::RecordErrorCode::Conflict, "That level or role already has a reward.").into()
}

fn gone() -> RecordError {
    RecordError::not_found("That level reward no longer exists.")
}

async fn find(tx: &mut PgConnection, guild_id: &str, id: &str) -> Result<Option<LevelRole>, TxError> {
    Ok(sqlx::query_as::<_, LevelRole>(r#"SELECT * FROM "LevelRole" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(&mut *tx)
        .await?)
}

#[async_trait]
impl RecordStore for LevelRolesStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let rows = sqlx::query_as::<_, LevelRole>(r#"SELECT * FROM "LevelRole" WHERE "guildId" = $1 ORDER BY "level" ASC LIMIT $2"#)
            .bind(guild_id)
            .bind(limits::LEVEL_ROLES as i64)
            .fetch_all(self.app.db())
            .await
            .map_err(internal)?;
        Ok(ListAnswer::items(rows.iter().map(level_role_item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        let row = sqlx::query_as::<_, LevelRole>(r#"SELECT * FROM "LevelRole" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
            .bind(id)
            .bind(guild_id)
            .fetch_optional(self.app.db())
            .await
            .map_err(internal)?;
        Ok(row.as_ref().map(level_role_item))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        self.app.settings().ensure_guild_settings(guild_id).await.map_err(internal)?;
        let level = int(data.get("level")).unwrap_or(0);
        let role_id = str_at(&data, "roleId").unwrap_or_default().to_string();
        let g = guild_id.to_string();
        let out = bot_db::transaction(self.app.db(), TX_OPTS, move |tx| {
            Box::pin(async move {
                lock_guild_settings(tx, &g).await?;
                assert_free(tx, &g, level, &role_id, None).await?;
                let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "LevelRole" WHERE "guildId" = $1"#).bind(&g).fetch_one(&mut *tx).await?;
                if n >= limits::LEVEL_ROLES as i64 {
                    return Err(RecordError::new(
                        dash_collections::RecordErrorCode::Limit,
                        format!("You can have at most {} level rewards.", limits::LEVEL_ROLES),
                    )
                    .into());
                }
                let row: LevelRole = InsertBuilder::new(&LEVEL_ROLE_INFO)
                    .values([("guildId", json!(g)), ("level", json!(level)), ("roleId", json!(role_id))])?
                    .build()?
                    .fetch_one(&mut *tx)
                    .await?;
                Ok::<_, TxError>(row)
            })
        })
        .await;
        match out {
            Ok(row) => {
                tracing::info!("Dashboard level reward {} (level {}) created in {guild_id} by {}", row.id, row.level, ctx.user_id);
                Ok(Saved::item(level_role_item(&row)))
            }
            Err(e) if e.is_unique() => Err(unique_error()),
            Err(e) => Err(e.into()),
        }
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, _ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let (g, id) = (guild_id.to_string(), id.to_string());
        let out = bot_db::transaction(self.app.db(), TX_OPTS, move |tx| {
            Box::pin(async move {
                lock_guild_settings(tx, &g).await?;
                let Some(current) = find(tx, &g, &id).await? else { return Err(gone().into()) };
                let level = int(data.get("level")).unwrap_or(i64::from(current.level));
                let role_id = str_at(&data, "roleId").map_or_else(|| current.role_id.clone(), str::to_string);
                if level == i64::from(current.level) && role_id == current.role_id {
                    return Ok(current);
                }
                assert_free(tx, &g, level, &role_id, Some(&id)).await?;
                let row: LevelRole = bot_db::UpdateBuilder::new(&LEVEL_ROLE_INFO)
                    .set("level", json!(level))?
                    .set("roleId", json!(role_id))?
                    .filter("id", json!(id))?
                    .build()?
                    .fetch_one(&mut *tx)
                    .await?;
                Ok::<_, TxError>(row)
            })
        })
        .await;
        match out {
            Ok(row) => Ok(Saved::item(level_role_item(&row))),
            Err(e) if e.is_unique() => Err(unique_error()),
            Err(e) => Err(e.into()),
        }
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
        let n = sqlx::query(r#"DELETE FROM "LevelRole" WHERE "id" = $1 AND "guildId" = $2"#)
            .bind(id)
            .bind(guild_id)
            .execute(self.app.db())
            .await
            .map_err(internal)?
            .rows_affected();
        if n == 0 {
            return Err(gone().into());
        }
        tracing::info!("Dashboard level reward {id} deleted in {guild_id} by {}", ctx.user_id);
        Ok(())
    }
}
