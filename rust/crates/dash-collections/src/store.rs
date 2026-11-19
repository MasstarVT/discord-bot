//! The seam between the dashboard and the database: what every `src/dashboard/collections/<key>.store.js` exports
//! (`{ list, get, create, update, remove, actions }`) as the object-safe [`RecordStore`] trait, the contexts the records
//! bridge hands a store, the error a store answers, the task runner (src/dashboard/tasks.js) and the registry mapping
//! a collection key to its definition and store.
//!
//! dash-records-a and dash-records-b implement the 22 stores; bins/dashboard registers them in a [`StoreRegistry`] and
//! builds the [`crate::RecordsBridge`] dash-server calls.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use dash_schema::FieldError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::discord::{DashboardDiscord, DiscordApiError};
use crate::errors::{RecordError, Saved, TaskResult};
use crate::index::{Collection, Collections};

/// What a store (or the records bridge) answers on failure. The server maps the four kinds as `recordFailure` does:
/// `Field` 400 invalid_request (with its path), `Record` its code's status, `Discord` 502 discord_failed, `Internal`
/// 500 save_failed on a write and 503 unavailable on a read (logged).
#[derive(Debug)]
pub enum StoreError {
    Field(FieldError),
    Record(RecordError),
    Discord(DiscordApiError),
    Internal(anyhow::Error),
}

impl StoreError {
    /// The error as the goldens and tests record it: `{ kind, ... }`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Field(e) => serde_json::json!({ "kind": "field", "path": e.path, "message": e.message }),
            Self::Record(e) => {
                let mut v = e.to_json();
                if let Value::Object(m) = &mut v {
                    m.shift_insert(0, "kind".into(), Value::String("record".into()));
                }
                v
            }
            Self::Discord(e) => serde_json::json!({ "kind": "discord", "code": e.code, "message": e.message }),
            Self::Internal(e) => serde_json::json!({ "kind": "internal", "message": e.to_string() }),
        }
    }

    /// The RecordError, when it is one.
    pub fn as_record(&self) -> Option<&RecordError> {
        match self {
            Self::Record(e) => Some(e),
            _ => None,
        }
    }

    /// The FieldError, when it is one.
    pub fn as_field(&self) -> Option<&FieldError> {
        match self {
            Self::Field(e) => Some(e),
            _ => None,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(e) => f.write_str(&e.message),
            Self::Record(e) => f.write_str(&e.message),
            Self::Discord(e) => f.write_str(&e.message),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<FieldError> for StoreError {
    fn from(e: FieldError) -> Self {
        Self::Field(e)
    }
}

impl From<RecordError> for StoreError {
    fn from(e: RecordError) -> Self {
        Self::Record(e)
    }
}

impl From<DiscordApiError> for StoreError {
    fn from(e: DiscordApiError) -> Self {
        Self::Discord(e)
    }
}

impl From<anyhow::Error> for StoreError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

/// The acting dashboard user (`ctx.user`: `{ id, username, globalName, avatar }` from the session), for placeholders
/// such as {user.name}.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DashboardUser {
    pub id: String,
    pub username: Option<String>,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

/// The acting user's OAuth guild entry (`ctx.member`: `{ owner, permissions }`), for rules such as who may ping a role.
/// `permissions` is Discord's permission bit string. `highest_role_position` is only known when a caller sets it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DashboardMember {
    pub owner: bool,
    pub permissions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highest_role_position: Option<i64>,
}

/// Who a write is for (the `actor` of recordsBridge.js: `{ userId, user, member }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Actor {
    pub user_id: String,
    pub user: Option<DashboardUser>,
    pub member: Option<DashboardMember>,
}

/// Options of [`TaskRunner::run`] (`{ actorId, waitMs? }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskOpts {
    /// The dashboard user the bot acts for.
    pub actor_id: String,
    /// How long to wait for the bot (default `TASK_WAIT_MS`, 12 s).
    pub wait_ms: Option<u64>,
}

/// src/dashboard/tasks.js `createTaskRunner(...).run`: queues a `dashboard.task` job for the bot and waits for it.
/// `Err` is a database failure while queueing (nothing was queued).
#[async_trait]
pub trait TaskRunner: Send + Sync {
    async fn run(&self, guild_id: &str, kind: &str, payload: Value, opts: TaskOpts) -> anyhow::Result<TaskResult>;
}

/// The read context of `list` and `get` (`{ guildId, discord, logger }`): stores show member names and avatars through
/// `discord`. Logging goes through `tracing`.
#[derive(Clone, Default)]
pub struct ReadCtx {
    pub guild_id: String,
    pub discord: Option<Arc<dyn DashboardDiscord>>,
}

impl fmt::Debug for ReadCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadCtx").field("guild_id", &self.guild_id).field("discord", &self.discord.is_some()).finish()
    }
}

/// The write context (`{ guildId, userId, user, member, discord, logger, runTask }`). `discord` and `tasks` are
/// optional because the JS stores check `typeof ctx.discord?.x === 'function'` / `typeof ctx.runTask === 'function'`
/// and answer "unavailable" without them.
#[derive(Clone, Default)]
pub struct StoreCtx {
    pub guild_id: String,
    pub user_id: String,
    pub user: Option<DashboardUser>,
    pub member: Option<DashboardMember>,
    pub discord: Option<Arc<dyn DashboardDiscord>>,
    pub tasks: Option<Arc<dyn TaskRunner>>,
}

impl fmt::Debug for StoreCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreCtx")
            .field("guild_id", &self.guild_id)
            .field("user_id", &self.user_id)
            .field("user", &self.user)
            .field("member", &self.member)
            .field("discord", &self.discord.is_some())
            .field("tasks", &self.tasks.is_some())
            .finish()
    }
}

impl StoreCtx {
    /// The context recordsBridge.js builds for a write by `actor` in `guild_id`.
    pub fn for_actor(
        guild_id: &str,
        actor: &Actor,
        discord: Option<Arc<dyn DashboardDiscord>>,
        tasks: Option<Arc<dyn TaskRunner>>,
    ) -> Self {
        Self {
            guild_id: guild_id.to_string(),
            user_id: actor.user_id.clone(),
            user: actor.user.clone(),
            member: actor.member.clone(),
            discord,
            tasks,
        }
    }

    /// Whether `runTask` exists (`typeof ctx.runTask === 'function'`).
    pub fn can_run_tasks(&self) -> bool {
        self.tasks.is_some()
    }

    /// `ctx.runTask(kind, payload)`: the task for this user in this guild. `Ok(None)` when there is no task runner
    /// (the caller decides what that means; most stores answer RecordError('unavailable')).
    pub async fn run_task(&self, kind: &str, payload: Value) -> anyhow::Result<Option<TaskResult>> {
        self.run_task_with(kind, payload, None).await
    }

    /// [`StoreCtx::run_task`] with a wait (`opts.waitMs`).
    pub async fn run_task_with(&self, kind: &str, payload: Value, wait_ms: Option<u64>) -> anyhow::Result<Option<TaskResult>> {
        let Some(tasks) = &self.tasks else { return Ok(None) };
        let opts = TaskOpts { actor_id: self.user_id.clone(), wait_ms };
        tasks.run(&self.guild_id, kind, payload, opts).await.map(Some)
    }

    /// The read context of the same guild and client.
    pub fn read_ctx(&self) -> ReadCtx {
        ReadCtx { guild_id: self.guild_id.clone(), discord: self.discord.clone() }
    }
}

/// The list options (`{ query, cursor }`): searchable collections get the search text and the page cursor; the
/// others get both empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOpts {
    pub query: String,
    pub cursor: String,
}

/// A list answer: the items, and the cursor of the next page (searchable collections answering in pages). A store that
/// answers a plain array in the JS answers `next_cursor: None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListAnswer {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

impl ListAnswer {
    pub fn items(items: Vec<Value>) -> Self {
        Self { items, next_cursor: None }
    }
}

/// An action's answer (`{ item, message }`; the server sends `item ?? null` and the message when it is a string).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionAnswer {
    pub item: Option<Value>,
    pub message: Option<String>,
}

impl ActionAnswer {
    pub fn new(item: Value, message: impl Into<String>) -> Self {
        Self { item: Some(item), message: Some(message.into()) }
    }
}

/// One collection's database access (`<key>.store.js` `store`). Every method may fail with any [`StoreError`]; a
/// `RecordError` is an expected failure shown to the admin, anything else is logged and answered as a 500 (writes) or
/// 503 (reads).
///
/// `data` is what [`crate::validate_record_input`] answered for the request (validated and normalised; on update only
/// the fields the request sets). Ids are the collection's item ids (checked against `RECORD_ID_RE` by the server).
#[async_trait]
pub trait RecordStore: Send + Sync {
    /// `list(guildId, { query, cursor }, ctx)`.
    async fn list(&self, guild_id: &str, opts: &ListOpts, ctx: &ReadCtx) -> Result<ListAnswer, StoreError>;
    /// `get(guildId, id, ctx)`: None when the item does not exist in this guild.
    async fn get(&self, guild_id: &str, id: &str, ctx: &ReadCtx) -> Result<Option<Value>, StoreError>;
    /// `create(guildId, data, ctx)`: the new item (and an optional note).
    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError>;
    /// `update(guildId, id, data, ctx)`: the item after the change (and an optional note).
    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError>;
    /// `remove(guildId, id, ctx)`.
    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError>;
    /// The keys of `store.actions` (empty for a store without actions).
    fn action_ids(&self) -> &[&'static str] {
        &[]
    }
    /// `store.actions[actionId](guildId, id, data, ctx)`. Only called for an id of [`RecordStore::action_ids`]; the
    /// default answers RecordError('not_found', 'Unknown action.') like the bridge does for a missing action.
    async fn action(
        &self,
        guild_id: &str,
        id: &str,
        action_id: &str,
        data: Map<String, Value>,
        ctx: &StoreCtx,
    ) -> Result<ActionAnswer, StoreError> {
        let _ = (guild_id, id, action_id, data, ctx);
        Err(RecordError::not_found("Unknown action.").into())
    }
}

/// Collection key -> (definition, store): what recordsBridge.js `STORES` plus collections/index.js `COLLECTIONS` are
/// together. Built by bins/dashboard from the stores of dash-records-a and dash-records-b.
#[derive(Clone)]
pub struct StoreRegistry {
    collections: Collections,
    stores: HashMap<String, Arc<dyn RecordStore>>,
}

impl fmt::Debug for StoreRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&String> = self.stores.keys().collect();
        keys.sort();
        f.debug_struct("StoreRegistry").field("stores", &keys).finish()
    }
}

impl StoreRegistry {
    /// An empty registry over the built-in collections.
    pub fn new() -> Self {
        Self::with_collections(Collections::builtin().clone())
    }

    /// An empty registry over a set of collections (tests use fakes).
    pub fn with_collections(collections: Collections) -> Self {
        Self { collections, stores: HashMap::new() }
    }

    /// Registers the store of a collection. `Err` when the key is not a collection of the set.
    pub fn register(&mut self, key: &str, store: Arc<dyn RecordStore>) -> Result<&mut Self, String> {
        if self.collections.get(key).is_none() {
            return Err(format!("{key} is not a collection"));
        }
        self.stores.insert(key.to_string(), store);
        Ok(self)
    }

    /// [`StoreRegistry::register`], chained.
    pub fn with(mut self, key: &str, store: Arc<dyn RecordStore>) -> Result<Self, String> {
        self.register(key, store)?;
        Ok(self)
    }

    /// The collections of the registry.
    pub fn collections(&self) -> &Collections {
        &self.collections
    }

    /// The definition of a key.
    pub fn collection(&self, key: &str) -> Option<&Arc<Collection>> {
        self.collections.get(key)
    }

    /// `storeFor(key)`: the store, or RecordError('not_found', 'Unknown list.').
    pub fn store(&self, key: &str) -> Result<&Arc<dyn RecordStore>, RecordError> {
        self.stores.get(key).ok_or_else(|| RecordError::not_found("Unknown list."))
    }

    /// Collection keys without a store, in display order (a complete registry has none).
    pub fn missing(&self) -> Vec<String> {
        self.collections.keys().filter(|k| !self.stores.contains_key(*k)).map(str::to_string).collect()
    }
}

impl Default for StoreRegistry {
    fn default() -> Self {
        Self::new()
    }
}
