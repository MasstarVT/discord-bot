//! What the dashboard server (src/dashboard/server.js) calls on `deps.records` (src/dashboard/recordsBridge.js) and
//! `deps.settings` (src/dashboard/settingsBridge.js). The bridges are implemented in bins/dashboard; dash-server only
//! sees these traits.

use std::sync::Arc;

use async_trait::async_trait;
use dash_schema::FieldError;
use serde_json::{Map, Value};

use crate::discord::DashboardDiscord;
use crate::errors::Saved;
use crate::store::{ActionAnswer, Actor, ListAnswer, ListOpts, StoreError};

/// `createRecordsBridge(...)`: dispatches each request to the collection's store. Unknown keys answer
/// RecordError('not_found', 'Unknown list.') and unknown actions RecordError('not_found', 'Unknown action.').
/// Reads get a read context (no actor); writes a store context for `actor` (see [`crate::StoreCtx::for_actor`]).
#[async_trait]
pub trait RecordsBridge: Send + Sync {
    /// `records.list(guildId, key, { query, cursor })`.
    async fn list(&self, guild_id: &str, key: &str, opts: ListOpts) -> Result<ListAnswer, StoreError>;
    /// `records.get(guildId, key, id)`: None answers 404.
    async fn get(&self, guild_id: &str, key: &str, id: &str) -> Result<Option<Value>, StoreError>;
    /// `records.create(guildId, key, data, actor)`.
    async fn create(&self, guild_id: &str, key: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError>;
    /// `records.update(guildId, key, id, data, actor)`.
    async fn update(&self, guild_id: &str, key: &str, id: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError>;
    /// `records.remove(guildId, key, id, actor)`.
    async fn remove(&self, guild_id: &str, key: &str, id: &str, actor: &Actor) -> Result<(), StoreError>;
    /// `records.action(guildId, key, id, actionId, data, actor)`.
    async fn action(
        &self,
        guild_id: &str,
        key: &str,
        id: &str,
        action_id: &str,
        data: Map<String, Value>,
        actor: &Actor,
    ) -> Result<ActionAnswer, StoreError>;
}

/// The context of [`SettingsBridge::write_settings`] (`{ discord, logger, userId }`).
#[derive(Clone, Default)]
pub struct SettingsWriteCtx {
    pub user_id: String,
    pub discord: Option<Arc<dyn DashboardDiscord>>,
}

impl std::fmt::Debug for SettingsWriteCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsWriteCtx").field("user_id", &self.user_id).field("discord", &self.discord.is_some()).finish()
    }
}

/// `writeSettings`' answer: the normalised row after the write and the notices for the admin.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsWritten {
    /// The GuildSettings row as JSON (what `dash_schema::read_settings_values` reads).
    pub row: Value,
    pub notices: Vec<String>,
}

/// Why a settings write failed: `Field` is a broken rule of the merged row or a card background that cannot be loaded
/// (400 invalid_request with its path; nothing saved); `Internal` a database failure (500 save_failed).
#[derive(Debug)]
pub enum SettingsWriteError {
    Field(FieldError),
    Internal(anyhow::Error),
}

impl std::fmt::Display for SettingsWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Field(e) => f.write_str(&e.message),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SettingsWriteError {}

impl From<FieldError> for SettingsWriteError {
    fn from(e: FieldError) -> Self {
        Self::Field(e)
    }
}

impl From<anyhow::Error> for SettingsWriteError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

/// settingsBridge.js: the dashboard's only path to GuildSettings.
#[async_trait]
pub trait SettingsBridge: Send + Sync {
    /// `readGuildSettings(guildId)`: the guild's current settings row (bypassing the bot's memo), normalised
    /// (`normalizeRow`), as JSON. `Err` when Postgres is unreachable (the server answers 503 settings_unavailable).
    async fn read_guild_settings(&self, guild_id: &str) -> anyhow::Result<Value>;
    /// `writeSettings(guildId, changes, ctx)`: saves validated changes (`validate_settings_changes(..).changes`, ids
    /// already checked against the server) and repeats the commands' side effects.
    async fn write_settings(
        &self,
        guild_id: &str,
        changes: Map<String, Value>,
        ctx: SettingsWriteCtx,
    ) -> Result<SettingsWritten, SettingsWriteError>;
}
