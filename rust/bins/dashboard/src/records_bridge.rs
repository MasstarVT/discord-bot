//! Port of `src/dashboard/recordsBridge.js`: the dashboard's database access for per-server records (saved embeds,
//! custom commands, level rewards, ...). It dispatches each request to the collection's store (dash-records-a and
//! dash-records-b, the ports of `src/dashboard/collections/<key>.store.js`).
//!
//! Stores get a context `{ guildId, userId, user, member, discord, runTask }` for writes (the acting dashboard user,
//! the Discord REST client and the task runner of [`crate::tasks`], which has the bot do it for this user in this
//! guild) and a read context `{ guildId, discord }` for `list` and `get`.

use std::sync::Arc;

use async_trait::async_trait;
use bot_core::App;
use dash_collections::{
    ActionAnswer, Actor, DashboardDiscord, ListAnswer, ListOpts, ReadCtx, RecordError, RecordsBridge, Saved, StoreCtx, StoreError, StoreRegistry,
    TaskRunner,
};
use serde_json::{Map, Value};

/// `STORES`: every collection key with its store. Fails when a store names a key the collections do not have, or a
/// collection has no store (`registry.missing()` must be empty; every key of COLLECTIONS has one in the JS).
pub fn build_store_registry(app: &App) -> Result<StoreRegistry, String> {
    let mut registry = StoreRegistry::new();
    dash_records_a::register_stores(&mut registry, app)?;
    dash_records_b::register(&mut registry, app)?;
    let missing = registry.missing();
    if !missing.is_empty() {
        return Err(format!("no record store for {}", missing.join(", ")));
    }
    Ok(registry)
}

/// `createRecordsBridge({ discord, logger, tasks })`. Every method may fail with a RecordError (expected failure,
/// shown to the admin) or another error (logged, answered as a 500 / 503).
pub struct DashboardRecords {
    registry: StoreRegistry,
    discord: Option<Arc<dyn DashboardDiscord>>,
    tasks: Arc<dyn TaskRunner>,
}

impl DashboardRecords {
    /// `tasks` defaults to `createTaskRunner({ logger })` in the JS; pass [`crate::tasks::TaskClient`] (or a test double).
    pub fn new(registry: StoreRegistry, discord: Option<Arc<dyn DashboardDiscord>>, tasks: Arc<dyn TaskRunner>) -> Self {
        Self { registry, discord, tasks }
    }

    /// The registry the bridge dispatches to.
    pub fn registry(&self) -> &StoreRegistry {
        &self.registry
    }

    /// Reads get a context too (no actor): stores show member names and avatars through `discord`.
    fn read_ctx(&self, guild_id: &str) -> ReadCtx {
        ReadCtx { guild_id: guild_id.to_string(), discord: self.discord.clone() }
    }

    /// `ctx(guildId, { userId, user, member })`: `runTask(kind, payload, opts)` runs with `actorId: userId`.
    fn write_ctx(&self, guild_id: &str, actor: &Actor) -> StoreCtx {
        StoreCtx::for_actor(guild_id, actor, self.discord.clone(), Some(self.tasks.clone()))
    }
}

#[async_trait]
impl RecordsBridge for DashboardRecords {
    async fn list(&self, guild_id: &str, key: &str, opts: ListOpts) -> Result<ListAnswer, StoreError> {
        self.registry.store(key)?.list(guild_id, &opts, &self.read_ctx(guild_id)).await
    }

    async fn get(&self, guild_id: &str, key: &str, id: &str) -> Result<Option<Value>, StoreError> {
        self.registry.store(key)?.get(guild_id, id, &self.read_ctx(guild_id)).await
    }

    async fn create(&self, guild_id: &str, key: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError> {
        self.registry.store(key)?.create(guild_id, data, &self.write_ctx(guild_id, actor)).await
    }

    async fn update(&self, guild_id: &str, key: &str, id: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError> {
        self.registry.store(key)?.update(guild_id, id, data, &self.write_ctx(guild_id, actor)).await
    }

    async fn remove(&self, guild_id: &str, key: &str, id: &str, actor: &Actor) -> Result<(), StoreError> {
        self.registry.store(key)?.remove(guild_id, id, &self.write_ctx(guild_id, actor)).await
    }

    async fn action(
        &self,
        guild_id: &str,
        key: &str,
        id: &str,
        action_id: &str,
        data: Map<String, Value>,
        actor: &Actor,
    ) -> Result<ActionAnswer, StoreError> {
        let store = self.registry.store(key)?;
        // `Object.hasOwn(store.actions ?? {}, actionId)`
        if !store.action_ids().contains(&action_id) {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        store.action(guild_id, id, action_id, data, &self.write_ctx(guild_id, actor)).await
    }
}
