//! In-memory implementations of the seams, for tests of the dashboard server (dash-server) and of the stores, without a
//! database, a bot or Discord:
//! - [`MemoryStore`]: a [`RecordStore`] keeping items per guild, with scripted actions and failures;
//! - [`RegistryBridge`]: a [`RecordsBridge`] dispatching to a [`StoreRegistry`] the way recordsBridge.js does;
//! - [`MemorySettings`]: a [`SettingsBridge`] over GuildSettings rows held as JSON;
//! - [`MemoryTasks`]: a [`TaskRunner`] answering scripted results;
//! - [`FakeDiscord`]: a [`DashboardDiscord`] with fixed resources, members and users that records every write.
//!
//! Every double records its calls so a test can assert what was asked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dash_schema::FieldError;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::bridge::{RecordsBridge, SettingsBridge, SettingsWriteCtx, SettingsWriteError, SettingsWritten};
use crate::discord::{DashboardDiscord, DiscordApiError, DiscordResources, InviteRef, MemberProfile, MemberRoles, MessageRef};
use crate::errors::{RecordError, Saved, TaskResult};
use crate::store::{
    ActionAnswer, Actor, ListAnswer, ListOpts, ReadCtx, RecordStore, StoreCtx, StoreError, StoreRegistry, TaskOpts, TaskRunner,
};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One call a double received: the method and its JSON arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub method: String,
    pub args: Value,
}

/// A scripted action handler of a [`MemoryStore`]: (guild, id, data, ctx, current item) -> answer.
pub type ActionHandler =
    Arc<dyn Fn(&str, &str, &Map<String, Value>, &StoreCtx, Option<&Value>) -> Result<ActionAnswer, StoreError> + Send + Sync>;

/// A generic in-memory store: `create` gives the item an id (`<prefix>1`, `<prefix>2`, ...) and stores
/// `{ id, ...data }`; `update` merges the data into the stored item; `remove` deletes it; `list` answers the items in
/// insertion order (filtered by `query` on any string value when one is given). Unknown ids answer
/// RecordError('not_found'). A scripted error (see [`MemoryStore::fail_next`]) is answered by the next call instead.
pub struct MemoryStore {
    prefix: String,
    next: AtomicU64,
    items: Mutex<HashMap<String, IndexMap<String, Value>>>,
    actions: Vec<(&'static str, ActionHandler)>,
    action_ids: Vec<&'static str>,
    fail: Mutex<Option<StoreError>>,
    calls: Mutex<Vec<Call>>,
}

impl MemoryStore {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            next: AtomicU64::new(1),
            items: Mutex::new(HashMap::new()),
            actions: Vec::new(),
            action_ids: Vec::new(),
            fail: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// With an action (`store.actions[id]`).
    pub fn with_action(mut self, id: &'static str, handler: ActionHandler) -> Self {
        self.actions.push((id, handler));
        self.action_ids.push(id);
        self
    }

    /// Puts an item (it must carry a string `id`).
    pub fn insert(&self, guild_id: &str, item: Value) {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        lock(&self.items).entry(guild_id.to_string()).or_default().insert(id, item);
    }

    /// The stored items of a guild.
    pub fn items(&self, guild_id: &str) -> Vec<Value> {
        lock(&self.items).get(guild_id).map(|m| m.values().cloned().collect()).unwrap_or_default()
    }

    /// The next call answers `err` instead.
    pub fn fail_next(&self, err: StoreError) {
        *lock(&self.fail) = Some(err);
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    fn record(&self, method: &str, args: Value) -> Result<(), StoreError> {
        lock(&self.calls).push(Call { method: method.into(), args });
        match lock(&self.fail).take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn not_found() -> StoreError {
        RecordError::not_found("That item does not exist any more.").into()
    }
}

fn ctx_json(ctx: &StoreCtx) -> Value {
    json!({ "guildId": ctx.guild_id, "userId": ctx.user_id, "member": ctx.member, "user": ctx.user })
}

#[async_trait]
impl RecordStore for MemoryStore {
    async fn list(&self, guild_id: &str, opts: &ListOpts, _ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        self.record("list", json!({ "guildId": guild_id, "query": opts.query, "cursor": opts.cursor }))?;
        let q = opts.query.to_lowercase();
        let items = self
            .items(guild_id)
            .into_iter()
            .filter(|i| q.is_empty() || i.as_object().is_some_and(|o| o.values().any(|v| v.as_str().is_some_and(|s| s.to_lowercase().contains(&q)))))
            .collect();
        Ok(ListAnswer::items(items))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        self.record("get", json!({ "guildId": guild_id, "id": id }))?;
        Ok(lock(&self.items).get(guild_id).and_then(|m| m.get(id)).cloned())
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        self.record("create", json!({ "guildId": guild_id, "data": data, "ctx": ctx_json(ctx) }))?;
        let id = format!("{}{}", self.prefix, self.next.fetch_add(1, Ordering::SeqCst));
        let mut item = Map::new();
        item.insert("id".into(), json!(id));
        item.extend(data);
        let item = Value::Object(item);
        self.insert(guild_id, item.clone());
        Ok(Saved::item(item))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        self.record("update", json!({ "guildId": guild_id, "id": id, "data": data, "ctx": ctx_json(ctx) }))?;
        let mut items = lock(&self.items);
        let item = items.get_mut(guild_id).and_then(|m| m.get_mut(id)).ok_or_else(Self::not_found)?;
        if let Value::Object(o) = item {
            o.extend(data);
        }
        Ok(Saved::item(item.clone()))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
        self.record("remove", json!({ "guildId": guild_id, "id": id, "ctx": ctx_json(ctx) }))?;
        let removed = lock(&self.items).get_mut(guild_id).and_then(|m| m.shift_remove(id));
        removed.map(|_| ()).ok_or_else(Self::not_found)
    }

    fn action_ids(&self) -> &[&'static str] {
        &self.action_ids
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        self.record("action", json!({ "guildId": guild_id, "id": id, "actionId": action_id, "data": data, "ctx": ctx_json(ctx) }))?;
        let Some((_, handler)) = self.actions.iter().find(|(a, _)| *a == action_id) else {
            return Err(RecordError::not_found("Unknown action.").into());
        };
        let current = lock(&self.items).get(guild_id).and_then(|m| m.get(id)).cloned();
        handler(guild_id, id, &data, ctx, current.as_ref())
    }
}

/// A [`RecordsBridge`] over a [`StoreRegistry`], dispatching like recordsBridge.js: unknown keys answer
/// RecordError('not_found', 'Unknown list.'), unknown actions RecordError('not_found', 'Unknown action.'), reads get a
/// read context and writes a store context built for the actor (with the bridge's Discord client and task runner).
pub struct RegistryBridge {
    pub registry: StoreRegistry,
    pub discord: Option<Arc<dyn DashboardDiscord>>,
    pub tasks: Option<Arc<dyn TaskRunner>>,
}

impl RegistryBridge {
    pub fn new(registry: StoreRegistry) -> Self {
        Self { registry, discord: None, tasks: None }
    }

    fn read(&self, guild_id: &str) -> ReadCtx {
        ReadCtx { guild_id: guild_id.to_string(), discord: self.discord.clone() }
    }

    fn write(&self, guild_id: &str, actor: &Actor) -> StoreCtx {
        StoreCtx::for_actor(guild_id, actor, self.discord.clone(), self.tasks.clone())
    }
}

#[async_trait]
impl RecordsBridge for RegistryBridge {
    async fn list(&self, guild_id: &str, key: &str, opts: ListOpts) -> Result<ListAnswer, StoreError> {
        self.registry.store(key)?.list(guild_id, &opts, &self.read(guild_id)).await
    }

    async fn get(&self, guild_id: &str, key: &str, id: &str) -> Result<Option<Value>, StoreError> {
        self.registry.store(key)?.get(guild_id, id, &self.read(guild_id)).await
    }

    async fn create(&self, guild_id: &str, key: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError> {
        self.registry.store(key)?.create(guild_id, data, &self.write(guild_id, actor)).await
    }

    async fn update(&self, guild_id: &str, key: &str, id: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError> {
        self.registry.store(key)?.update(guild_id, id, data, &self.write(guild_id, actor)).await
    }

    async fn remove(&self, guild_id: &str, key: &str, id: &str, actor: &Actor) -> Result<(), StoreError> {
        self.registry.store(key)?.remove(guild_id, id, &self.write(guild_id, actor)).await
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
        if !store.action_ids().contains(&action_id) {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        store.action(guild_id, id, action_id, data, &self.write(guild_id, actor)).await
    }
}

/// A [`SettingsBridge`] over GuildSettings rows held as JSON. Reads answer the stored row (or `{ guildId }` for an
/// unknown guild); writes apply `dash_schema::to_stored_changes`, check `check_cross_fields` on the merged row
/// (FieldError, nothing saved) and store it. Scripted failures and notices are supported.
#[derive(Default)]
pub struct MemorySettings {
    rows: Mutex<HashMap<String, Value>>,
    fail_reads: Mutex<bool>,
    fail_writes: Mutex<bool>,
    notices: Mutex<Vec<String>>,
    calls: Mutex<Vec<Call>>,
}

impl MemorySettings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a row.
    pub fn set_row(&self, guild_id: &str, row: Value) {
        lock(&self.rows).insert(guild_id.to_string(), row);
    }

    /// The stored row.
    pub fn row(&self, guild_id: &str) -> Option<Value> {
        lock(&self.rows).get(guild_id).cloned()
    }

    /// Reads fail (a database outage).
    pub fn fail_reads(&self, on: bool) {
        *lock(&self.fail_reads) = on;
    }

    /// Writes fail with an internal error.
    pub fn fail_writes(&self, on: bool) {
        *lock(&self.fail_writes) = on;
    }

    /// The notices the next writes answer.
    pub fn set_notices(&self, notices: Vec<String>) {
        *lock(&self.notices) = notices;
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }
}

#[async_trait]
impl SettingsBridge for MemorySettings {
    async fn read_guild_settings(&self, guild_id: &str) -> anyhow::Result<Value> {
        lock(&self.calls).push(Call { method: "readGuildSettings".into(), args: json!({ "guildId": guild_id }) });
        if *lock(&self.fail_reads) {
            anyhow::bail!("settings unavailable");
        }
        Ok(self.row(guild_id).unwrap_or_else(|| json!({ "guildId": guild_id })))
    }

    async fn write_settings(&self, guild_id: &str, changes: Map<String, Value>, ctx: SettingsWriteCtx) -> Result<SettingsWritten, SettingsWriteError> {
        lock(&self.calls).push(Call { method: "writeSettings".into(), args: json!({ "guildId": guild_id, "changes": changes, "userId": ctx.user_id }) });
        if *lock(&self.fail_writes) {
            return Err(SettingsWriteError::Internal(anyhow::anyhow!("write failed")));
        }
        let current = self.row(guild_id).unwrap_or_else(|| json!({ "guildId": guild_id }));
        let data = dash_schema::to_stored_changes(&changes, Some(&current));
        let mut merged = current.as_object().cloned().unwrap_or_default();
        merged.extend(data);
        let merged = Value::Object(merged);
        if let Some(p) = dash_schema::check_cross_fields(Some(&merged), Some(&changes)) {
            return Err(SettingsWriteError::Field(FieldError::new(p.path, p.message)));
        }
        self.set_row(guild_id, merged.clone());
        Ok(SettingsWritten { row: merged, notices: lock(&self.notices).clone() })
    }
}

/// A [`TaskRunner`] answering scripted results per task kind (default: done with 'Done.' and no data). An `Err` script
/// makes `run` fail (a database failure while queueing).
#[derive(Default)]
pub struct MemoryTasks {
    results: Mutex<HashMap<String, Result<TaskResult, String>>>,
    calls: Mutex<Vec<(String, String, Value, TaskOpts)>>,
}

impl MemoryTasks {
    pub fn new() -> Self {
        Self::default()
    }

    /// The answer to every run of `kind`.
    pub fn answer(&self, kind: &str, result: TaskResult) {
        lock(&self.results).insert(kind.to_string(), Ok(result));
    }

    /// Runs of `kind` fail.
    pub fn fail(&self, kind: &str, message: &str) {
        lock(&self.results).insert(kind.to_string(), Err(message.to_string()));
    }

    /// The runs so far: (guild, kind, payload, opts).
    pub fn calls(&self) -> Vec<(String, String, Value, TaskOpts)> {
        lock(&self.calls).clone()
    }
}

#[async_trait]
impl TaskRunner for MemoryTasks {
    async fn run(&self, guild_id: &str, kind: &str, payload: Value, opts: TaskOpts) -> anyhow::Result<TaskResult> {
        lock(&self.calls).push((guild_id.to_string(), kind.to_string(), payload, opts));
        match lock(&self.results).get(kind).cloned() {
            Some(Ok(r)) => Ok(r),
            Some(Err(m)) => Err(anyhow::anyhow!(m)),
            None => Ok(TaskResult::done("Done.", None)),
        }
    }
}

/// A [`DashboardDiscord`] with fixed resources per guild, members (role ids) and user profiles. Sent and edited
/// messages get increasing snowflake ids and are recorded; a scripted error makes the next call of that method fail.
#[derive(Default)]
pub struct FakeDiscord {
    resources: Mutex<HashMap<String, Arc<DiscordResources>>>,
    members: Mutex<HashMap<(String, String), MemberRoles>>,
    users: Mutex<HashMap<String, MemberProfile>>,
    fail: Mutex<HashMap<String, DiscordApiError>>,
    next_message: AtomicU64,
    calls: Mutex<Vec<Call>>,
}

impl FakeDiscord {
    pub fn new() -> Self {
        Self { next_message: AtomicU64::new(1), ..Self::default() }
    }

    pub fn set_resources(&self, guild_id: &str, resources: DiscordResources) {
        lock(&self.resources).insert(guild_id.to_string(), Arc::new(resources));
    }

    pub fn set_member(&self, guild_id: &str, user_id: &str, roles: Vec<String>) {
        lock(&self.members).insert((guild_id.to_string(), user_id.to_string()), MemberRoles { roles });
    }

    pub fn set_user(&self, profile: MemberProfile) {
        lock(&self.users).insert(profile.id.clone(), profile);
    }

    /// The next call of `method` ('getGuildResources', 'sendMessage', ...) fails with `err`.
    pub fn fail_next(&self, method: &str, err: DiscordApiError) {
        lock(&self.fail).insert(method.to_string(), err);
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    fn record(&self, method: &str, args: Value) -> Result<(), DiscordApiError> {
        lock(&self.calls).push(Call { method: method.into(), args });
        match lock(&self.fail).remove(method) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn message_id(&self) -> String {
        format!("{}", 900_000_000_000_000_000u64 + self.next_message.fetch_add(1, Ordering::SeqCst))
    }
}

#[async_trait]
impl DashboardDiscord for FakeDiscord {
    async fn get_guild_resources(&self, guild_id: &str, fresh: bool) -> Result<Arc<DiscordResources>, DiscordApiError> {
        self.record("getGuildResources", json!({ "guildId": guild_id, "fresh": fresh }))?;
        lock(&self.resources)
            .get(guild_id)
            .cloned()
            .ok_or_else(|| DiscordApiError::new("Unknown Guild", 404, "not_found").with_discord_code(10004))
    }

    async fn get_member(&self, guild_id: &str, user_id: &str) -> Result<Option<MemberRoles>, DiscordApiError> {
        self.record("getMember", json!({ "guildId": guild_id, "userId": user_id }))?;
        Ok(lock(&self.members).get(&(guild_id.to_string(), user_id.to_string())).cloned())
    }

    async fn search_members(&self, guild_id: &str, query: &str) -> Result<Vec<MemberProfile>, DiscordApiError> {
        self.record("searchMembers", json!({ "guildId": guild_id, "query": query }))?;
        let q = query.to_lowercase();
        let users = lock(&self.users);
        let mut out: Vec<MemberProfile> = users
            .values()
            .filter(|u| u.id == query || u.username.to_lowercase().starts_with(&q) || u.nick.as_deref().is_some_and(|n| n.to_lowercase().starts_with(&q)))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.truncate(25);
        Ok(out)
    }

    async fn get_users(&self, ids: &[String]) -> Result<HashMap<String, Option<MemberProfile>>, DiscordApiError> {
        self.record("getUsers", json!({ "ids": ids }))?;
        let users = lock(&self.users);
        Ok(ids.iter().take(100).map(|id| (id.clone(), users.get(id).cloned())).collect())
    }

    async fn send_message(&self, channel_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError> {
        self.record("sendMessage", json!({ "channelId": channel_id, "payload": payload }))?;
        Ok(MessageRef { id: self.message_id(), channel_id: channel_id.to_string() })
    }

    async fn edit_message(&self, channel_id: &str, message_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError> {
        self.record("editMessage", json!({ "channelId": channel_id, "messageId": message_id, "payload": payload }))?;
        Ok(MessageRef { id: message_id.to_string(), channel_id: channel_id.to_string() })
    }

    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordApiError> {
        self.record("deleteMessage", json!({ "channelId": channel_id, "messageId": message_id }))
    }

    async fn create_invite(&self, channel_id: &str, reason: &str) -> Result<InviteRef, DiscordApiError> {
        self.record("createInvite", json!({ "channelId": channel_id, "reason": reason }))?;
        Ok(InviteRef { code: "testcode".into(), url: "https://discord.gg/testcode".into(), expires_at: None })
    }
}
