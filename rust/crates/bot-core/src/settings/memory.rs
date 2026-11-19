//! The in-memory settings store (offline tests): rows live in a map, defaults come from the schema metadata, writes
//! are validated against the column types like the database would, and the whole store can be made unavailable.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bot_db::schema::{ClientDefault, ModelInfo};
use bot_db::{DbError, GUILD_SETTINGS_INFO, GuildSettings, to_sql_value};
use serde_json::{Map, Value, json};

use super::{SettingsMutator, SettingsPatch, SettingsStore};

/// A new row of `info` as Prisma would create it (literal defaults, generated ids and dates, `[]` for lists, `null`
/// for optional columns), with `fields` applied on top. Prisma field names, schema order.
pub fn default_row_json(info: &ModelInfo, fields: &Map<String, Value>) -> Map<String, Value> {
    let now = bot_db::format_js_date(&bot_db::now_utc());
    let mut row = Map::new();
    for col in info.columns {
        let v = if let Some(v) = fields.get(col.field) {
            v.clone()
        } else if let Some(text) = col.default_json {
            serde_json::from_str(text).unwrap_or(Value::Null)
        } else {
            match col.client_default {
                ClientDefault::Cuid => json!(bot_db::new_cuid()),
                ClientDefault::Uuid => json!(bot_db::new_uuid()),
                ClientDefault::Now | ClientDefault::UpdatedAt => json!(now),
                _ if col.list => json!([]),
                _ => Value::Null,
            }
        };
        row.insert(col.field.to_string(), v);
    }
    row
}

fn unavailable() -> DbError {
    DbError::Timeout("the in-memory database is unavailable".into())
}

/// Checks a patch against the GuildSettings columns (unknown column, wrong type) and applies it to a row's JSON.
fn apply(row: &GuildSettings, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
    let mut json = match row.to_json() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    for (k, v) in data.iter() {
        let col = GUILD_SETTINGS_INFO
            .column(k)
            .ok_or_else(|| DbError::UnknownColumn { model: "GuildSettings".into(), column: k.clone() })?;
        to_sql_value(col, v)?;
        json.insert(col.field.to_string(), v.clone());
    }
    // Prisma sends no UPDATE for empty data, so `updatedAt` only moves when something is written.
    if !data.is_empty() && !data.iter().any(|(k, _)| k == "updatedAt") {
        json.insert("updatedAt".into(), json!(bot_db::format_js_date(&bot_db::now_utc())));
    }
    GuildSettings::from_json(&Value::Object(json))
}

/// The in-memory store.
#[derive(Default)]
pub struct MemorySettingsStore {
    rows: Mutex<HashMap<String, GuildSettings>>,
    down: AtomicBool,
    lock: tokio::sync::Mutex<()>,
}

impl MemorySettingsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A default row for `guild_id` with `fields` applied (Prisma names).
    pub fn new_row(guild_id: &str, fields: Map<String, Value>) -> Result<GuildSettings, DbError> {
        let mut f = fields;
        f.insert("guildId".into(), json!(guild_id));
        GuildSettings::from_json(&Value::Object(default_row_json(&GUILD_SETTINGS_INFO, &f)))
    }

    /// Stores a row as is.
    pub fn put(&self, row: GuildSettings) {
        self.rows.lock().unwrap_or_else(|e| e.into_inner()).insert(row.guild_id.clone(), row);
    }

    /// The stored row (no I/O semantics, no failure switch).
    pub fn row(&self, guild_id: &str) -> Option<GuildSettings> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner()).get(guild_id).cloned()
    }

    /// Deletes a row (as `prisma.guildSettings.deleteMany` in the JS tests).
    pub fn remove(&self, guild_id: &str) {
        self.rows.lock().unwrap_or_else(|e| e.into_inner()).remove(guild_id);
    }

    /// Makes every operation fail (`false`), like an unreachable database, or work again (`true`).
    pub fn set_available(&self, available: bool) {
        self.down.store(!available, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), DbError> {
        if self.down.load(Ordering::SeqCst) { Err(unavailable()) } else { Ok(()) }
    }

    fn get_or_create(&self, guild_id: &str) -> Result<GuildSettings, DbError> {
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = rows.get(guild_id) {
            return Ok(r.clone());
        }
        let row = Self::new_row(guild_id, Map::new())?;
        rows.insert(guild_id.to_string(), row.clone());
        Ok(row)
    }
}

#[async_trait]
impl SettingsStore for MemorySettingsStore {
    async fn find(&self, guild_id: &str) -> Result<Option<GuildSettings>, DbError> {
        self.check()?;
        Ok(self.row(guild_id))
    }

    async fn create_if_missing(&self, guild_id: &str) -> Result<GuildSettings, DbError> {
        self.check()?;
        self.get_or_create(guild_id)
    }

    async fn upsert(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        self.check()?;
        let _guard = self.lock.lock().await;
        let current = self.get_or_create(guild_id)?;
        let row = apply(&current, data)?;
        self.put(row.clone());
        Ok(row)
    }

    async fn update(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        self.check()?;
        let _guard = self.lock.lock().await;
        let current = self.row(guild_id).ok_or(DbError::Sqlx(sqlx::Error::RowNotFound))?;
        let row = apply(&current, data)?;
        self.put(row.clone());
        Ok(row)
    }

    async fn update_with(&self, guild_id: &str, mutator: SettingsMutator) -> anyhow::Result<GuildSettings> {
        self.check()?;
        let _guard = self.lock.lock().await;
        let current = self.get_or_create(guild_id)?;
        let data = mutator(current.clone())?;
        if data.is_empty() {
            return Ok(current);
        }
        let row = apply(&current, &data)?;
        self.put(row.clone());
        Ok(row)
    }
}
