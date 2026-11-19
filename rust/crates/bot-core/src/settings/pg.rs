//! The PostgreSQL settings store: the statements `settingsService.js` runs, through bot-db.

use async_trait::async_trait;
use bot_db::query::SelectBuilder;
use bot_db::{DbError, GUILD_SETTINGS_INFO, GuildSettings, InsertBuilder, PgPool, TxOptions, UpdateBuilder};
use serde_json::{Value, json};

use super::{SettingsMutator, SettingsPatch, SettingsStore};

/// `LOCK_ROW(guildId)`.
pub const LOCK_ROW_SQL: &str = r#"SELECT 1 FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#;

/// `INSERT_IF_MISSING(guildId)`: creates the row only when it is missing, inside a transaction. `id` is a random UUID
/// and the dates are `SQL_NOW`, as in the JS.
pub const INSERT_IF_MISSING_SQL: &str = r#"
  INSERT INTO "GuildSettings" ("id", "guildId", "createdAt", "updatedAt")
  VALUES ($1, $2, (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'))
  ON CONFLICT ("guildId") DO NOTHING"#;

/// `RAW_SQL`: every raw statement of the settings service (name, SQL), for the statement tests.
pub const RAW_SQL: &[(&str, &str)] = &[("settings.lockRow", LOCK_ROW_SQL), ("settings.insertIfMissing", INSERT_IF_MISSING_SQL)];

/// The settings store over a PostgreSQL pool.
#[derive(Clone)]
pub struct PgSettingsStore {
    pool: PgPool,
}

impl PgSettingsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn find_query(guild_id: &str) -> Result<bot_db::BuiltQuery, DbError> {
    SelectBuilder::new(&GUILD_SETTINGS_INFO).filter("guildId", json!(guild_id))?.build()
}

fn upsert_query(guild_id: &str, data: &SettingsPatch) -> Result<bot_db::BuiltQuery, DbError> {
    let mut insert = InsertBuilder::new(&GUILD_SETTINGS_INFO).value("guildId", json!(guild_id))?;
    for (k, v) in data.iter() {
        if k == "guildId" {
            continue;
        }
        insert = insert.value(k, v.clone())?;
    }
    let set: Vec<(&str, Value)> = data.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    insert.on_conflict_update(&["guildId"], set)?.build()
}

fn update_query(guild_id: &str, data: &SettingsPatch) -> Result<bot_db::BuiltQuery, DbError> {
    let mut update = UpdateBuilder::new(&GUILD_SETTINGS_INFO);
    for (k, v) in data.iter() {
        update = update.set(k, v.clone())?;
    }
    update.filter("guildId", json!(guild_id))?.build()
}

#[async_trait]
impl SettingsStore for PgSettingsStore {
    async fn find(&self, guild_id: &str) -> Result<Option<GuildSettings>, DbError> {
        find_query(guild_id)?.fetch_optional(&self.pool).await
    }

    async fn create_if_missing(&self, guild_id: &str) -> Result<GuildSettings, DbError> {
        // Prisma's `upsert({ update: {} })` leaves an existing row alone (`updatedAt` included).
        let insert = InsertBuilder::new(&GUILD_SETTINGS_INFO).value("guildId", json!(guild_id))?.on_conflict_do_nothing(&["guildId"])?.build()?;
        if let Some(row) = insert.fetch_optional(&self.pool).await? {
            return Ok(row);
        }
        find_query(guild_id)?.fetch_one(&self.pool).await
    }

    async fn upsert(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        // Prisma sends no UPDATE for `upsert({ update: {} })`: an existing row (its `updatedAt` included) is returned
        // as it is.
        if data.is_empty() {
            return self.create_if_missing(guild_id).await;
        }
        upsert_query(guild_id, data)?.fetch_one(&self.pool).await
    }

    async fn update(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        // `update({ data: {} })` is only a read in Prisma (`updatedAt` is not touched).
        if data.is_empty() {
            return find_query(guild_id)?.fetch_one(&self.pool).await;
        }
        update_query(guild_id, data)?.fetch_one(&self.pool).await
    }

    async fn update_with(&self, guild_id: &str, mutator: SettingsMutator) -> anyhow::Result<GuildSettings> {
        let guild_id = guild_id.to_string();
        bot_db::transaction(&self.pool, TxOptions::default(), move |tx| {
            Box::pin(async move {
                sqlx::query(INSERT_IF_MISSING_SQL).bind(bot_db::new_uuid()).bind(&guild_id).execute(&mut *tx).await.map_err(DbError::from)?;
                sqlx::query(LOCK_ROW_SQL).bind(&guild_id).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
                let current: GuildSettings = find_query(&guild_id)?.fetch_one(&mut *tx).await?;
                let data = mutator(current.clone())?;
                if data.is_empty() {
                    return Ok(current);
                }
                let row: GuildSettings = update_query(&guild_id, &data)?.fetch_one(&mut *tx).await?;
                Ok::<_, anyhow::Error>(row)
            })
        })
        .await
    }
}
