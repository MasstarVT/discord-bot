//! Port of `src/services/settingsService.js`: the only way to read or write `GuildSettings`.
//!
//! Rows are cached in Redis for 300 s under `guild:{g}:settings` with the `_cacheV` marker (version 2; any other
//! version, a missing marker or another guild's row is a miss), memoised per process for 30 s and shared as an
//! immutable [`Settings`] (`Arc<GuildSettings>`: the Rust form of the JS deep freeze; clone the field to edit it).
//! Generations make sure a slow reader never memoises a row older than a write that finished before it.
//!
//! The database side is a [`SettingsStore`]: [`PgSettingsStore`] runs the JS statements against PostgreSQL,
//! [`MemorySettingsStore`] keeps rows in process for offline tests.

mod memory;
mod pg;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_config::ModuleKey;
use bot_db::{DbError, GuildSettings};
use futures::FutureExt;
use futures::future::Shared;
use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::cache::Cache;
use crate::clock::SharedClock;
use crate::error::BoxFuture;

pub use memory::{MemorySettingsStore, default_row_json};
pub use pg::{INSERT_IF_MISSING_SQL, LOCK_ROW_SQL, PgSettingsStore, RAW_SQL};

/// Redis TTL of the cached row.
pub const SETTINGS_TTL_SECONDS: u64 = 300;
/// How long a row stays memoised in this process.
pub const SETTINGS_MEMO_TTL_MS: i64 = 30_000;
/// The fields revived from ISO strings to dates.
pub const SETTINGS_DATE_FIELDS: [&str; 2] = ["createdAt", "updatedAt"];
/// Bump whenever the GuildSettings row shape changes. Rows cached with another marker (or none) are misses.
pub const SETTINGS_CACHE_VERSION: i64 = 2;
/// The marker field added to the cached row.
pub const CACHE_VERSION_FIELD: &str = "_cacheV";

const MEMO_MAX: usize = 10_000;
const WARN_INTERVAL_MS: u64 = 60_000;

/// A guild's settings row, shared and immutable.
pub type Settings = Arc<GuildSettings>;

/// `settingsKey(guildId)`.
pub fn settings_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:settings")
}

/// `isModuleEnabled(settings, key)`: a `None` key is on; missing settings are off; otherwise the module's flag must be
/// exactly `true`.
pub fn is_module_enabled(settings: Option<&GuildSettings>, module: Option<ModuleKey>) -> bool {
    bot_config::is_module_key_enabled(module, |col| settings.and_then(|s| s.flag(col)))
}

/// [`is_module_enabled`] by module name (an unknown name is off).
pub fn is_module_enabled_by_name(settings: Option<&GuildSettings>, module: Option<&str>) -> bool {
    bot_config::is_module_enabled(module, |col| settings.and_then(|s| s.flag(col)))
}

/// `isValidCachedSettings(hit, guildId)`: a plain object for this guild written by the current row shape. The marker
/// compares numerically (`2` and `2.0` both count, as `===` does in JS).
pub fn is_valid_cached_settings(hit: &Value, guild_id: &str) -> bool {
    let Some(obj) = hit.as_object() else { return false };
    obj.get("guildId").and_then(Value::as_str) == Some(guild_id)
        && obj.get(CACHE_VERSION_FIELD).and_then(Value::as_f64) == Some(SETTINGS_CACHE_VERSION as f64)
}

/// `reviveSettings(row)`: the row without the marker, dates revived. `None` for a value that is not a readable row
/// (a miss, never an error).
pub fn revive_settings(value: &Value) -> Option<GuildSettings> {
    if !value.is_object() {
        return None;
    }
    GuildSettings::from_json(value).ok()
}

/// `withMarker(row)`: the JSON the JS caches (the Prisma row, then `_cacheV`).
pub fn with_marker(row: &GuildSettings) -> Value {
    let mut v = row.to_json();
    if let Value::Object(map) = &mut v {
        map.insert(CACHE_VERSION_FIELD.to_string(), Value::from(SETTINGS_CACHE_VERSION));
    }
    v
}

/// Fields to write, by Prisma field name (`{ automodEnabled: true, modLogChannelId: "123" }`). Values are JSON as the
/// JS passes them to Prisma; unknown columns and values of the wrong type are refused by the store.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsPatch(pub Map<String, Value>);

impl SettingsPatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets one field.
    pub fn set(mut self, field: &str, value: impl Into<Value>) -> Self {
        self.0.insert(field.to_string(), value.into());
        self
    }

    /// Sets one field in place.
    pub fn insert(&mut self, field: &str, value: impl Into<Value>) {
        self.0.insert(field.to_string(), value.into());
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }
}

impl From<Map<String, Value>> for SettingsPatch {
    fn from(m: Map<String, Value>) -> Self {
        Self(m)
    }
}

/// The pure read-modify-write step of [`SettingsService::update_settings_with`]: gets an owned copy of the locked
/// row and returns the fields to change (an empty patch changes nothing). An `Err` rolls back and reaches the caller
/// unchanged.
pub type SettingsMutator = Box<dyn FnOnce(GuildSettings) -> anyhow::Result<SettingsPatch> + Send>;

/// A settings failure (the database). Cheap to clone, so concurrent readers can share one load.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct SettingsError(pub Arc<DbError>);

impl SettingsError {
    pub fn db(&self) -> &DbError {
        &self.0
    }
}

impl From<DbError> for SettingsError {
    fn from(e: DbError) -> Self {
        Self(Arc::new(e))
    }
}

/// The database side of the settings service.
#[async_trait]
pub trait SettingsStore: Send + Sync + 'static {
    /// `findUnique({ where: { guildId } })`.
    async fn find(&self, guild_id: &str) -> Result<Option<GuildSettings>, DbError>;
    /// `upsert({ where: { guildId }, update: {}, create: { guildId } })`: the row, created when missing.
    async fn create_if_missing(&self, guild_id: &str) -> Result<GuildSettings, DbError>;
    /// `upsert({ where: { guildId }, update: data, create: { guildId, ...data } })`.
    async fn upsert(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError>;
    /// `update({ where: { guildId }, data })`.
    async fn update(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError>;
    /// The locked read-modify-write of `updateSettingsWith` (one transaction: insert if missing, lock, read, mutate,
    /// update). The mutator's error is returned unchanged; database errors are `DbError`s inside the anyhow error.
    async fn update_with(&self, guild_id: &str, mutator: SettingsMutator) -> anyhow::Result<GuildSettings>;
}

type LoadFuture = Shared<BoxFuture<Result<Settings, SettingsError>>>;

#[derive(Default)]
struct MemoState {
    /// guildId -> (row, expires). Insertion order = age.
    memo: IndexMap<String, (Settings, i64)>,
    /// Guilds whose row is known to exist in this process.
    ensured: IndexMap<String, ()>,
    /// guildId -> generation, bumped by every write and invalidation.
    generations: IndexMap<String, u64>,
    /// In-flight cold loads.
    loading: HashMap<String, (u64, LoadFuture)>,
    next_load_id: u64,
}

impl MemoState {
    fn generation(&self, guild_id: &str) -> u64 {
        self.generations.get(guild_id).copied().unwrap_or(0)
    }

    fn bump_generation(&mut self, guild_id: &str) {
        let g = self.generation(guild_id) + 1;
        self.generations.insert(guild_id.to_string(), g);
        if self.generations.len() > MEMO_MAX * 2 {
            self.generations.shift_remove_index(0);
        }
    }

    fn memo_set(&mut self, guild_id: &str, row: Settings, now: i64) {
        self.memo.shift_remove(guild_id);
        self.memo.insert(guild_id.to_string(), (row, now + SETTINGS_MEMO_TTL_MS));
        while self.memo.len() > MEMO_MAX {
            self.memo.shift_remove_index(0);
        }
    }

    fn memo_get(&mut self, guild_id: &str, now: i64) -> Option<Settings> {
        let (row, expires) = self.memo.get(guild_id)?.clone();
        if expires <= now {
            self.memo.shift_remove(guild_id);
            return None;
        }
        Some(row)
    }

    fn mark_ensured(&mut self, guild_id: &str) {
        self.ensured.insert(guild_id.to_string(), ());
        if self.ensured.len() > MEMO_MAX * 2 {
            self.ensured.shift_remove_index(0);
        }
    }
}

struct Inner {
    store: Arc<dyn SettingsStore>,
    cache: Cache,
    clock: SharedClock,
    state: Mutex<MemoState>,
    warned: bot_util::async_util::Throttle,
}

/// The settings service. Cheap to clone (shared state).
#[derive(Clone)]
pub struct SettingsService {
    inner: Arc<Inner>,
}

impl SettingsService {
    pub fn new(store: Arc<dyn SettingsStore>, cache: Cache, clock: SharedClock) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                cache,
                clock,
                state: Mutex::new(MemoState::default()),
                warned: bot_util::async_util::create_throttle(WARN_INTERVAL_MS),
            }),
        }
    }

    /// The database side (tests reach the in-memory store through it).
    pub fn store(&self) -> &Arc<dyn SettingsStore> {
        &self.inner.store
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    async fn load_settings(self, guild_id: String) -> Result<Settings, SettingsError> {
        let key = settings_key(&guild_id);
        let generation0 = self.lock().generation(&guild_id);
        let hit = self.inner.cache.get(&key).await;
        if let Some(hit) = &hit
            && is_valid_cached_settings(hit, &guild_id)
            && let Some(row) = revive_settings(hit)
        {
            let row = Arc::new(row);
            let now = self.now();
            let mut st = self.lock();
            if st.generation(&guild_id) == generation0 {
                st.memo_set(&guild_id, row.clone(), now);
            }
            return Ok(row);
        }
        // Old shape, wrong guild, no marker or unreadable: drop it so the NX write below can replace it.
        if hit.is_some() {
            self.inner.cache.del_one(&key).await;
        }
        let row = match self.inner.store.find(&guild_id).await? {
            Some(r) => r,
            None => self.inner.store.create_if_missing(&guild_id).await?,
        };
        // NX: a reader can never overwrite a newer value written by update_settings.
        self.inner.cache.set_nx(&key, &with_marker(&row), SETTINGS_TTL_SECONDS).await;
        let revived = Arc::new(row);
        let now = self.now();
        let mut st = self.lock();
        if st.generation(&guild_id) == generation0 {
            st.memo_set(&guild_id, revived.clone(), now);
            st.mark_ensured(&guild_id);
        }
        Ok(revived)
    }

    /// `getSettings(guildId)`: memo, then Redis, then Postgres (the row is created on first use). A burst of calls
    /// for one guild shares one load. Fails only when Postgres fails.
    pub async fn get_settings(&self, guild_id: &str) -> Result<Settings, SettingsError> {
        self.get_settings_with(guild_id, false).await
    }

    /// `getSettings(guildId, { fresh })`: `fresh` skips the in-process memo (Redis is still read first).
    pub async fn get_settings_with(&self, guild_id: &str, fresh: bool) -> Result<Settings, SettingsError> {
        if fresh {
            return self.clone().load_settings(guild_id.to_string()).await;
        }
        enum Step {
            Hit(Settings),
            Pending(LoadFuture),
            Started(LoadFuture),
        }
        let step = {
            let now = self.now();
            let mut st = self.lock();
            if let Some(hit) = st.memo_get(guild_id, now) {
                Step::Hit(hit)
            } else if let Some((_, pending)) = st.loading.get(guild_id) {
                Step::Pending(pending.clone())
            } else {
                st.next_load_id += 1;
                let id = st.next_load_id;
                // The load clears its own `loading` entry when it finishes (JS `promise.then(clear, clear)`), whoever
                // polls it to the end: when only the starter cleared it, a starter dropped mid-load (a timeout, a
                // `select!`, a cancelled task) left the finished load in `loading`, and every later read past the
                // memo got that stale row back.
                let (svc, gid) = (self.clone(), guild_id.to_string());
                let fut: LoadFuture = async move {
                    let out = svc.clone().load_settings(gid.clone()).await;
                    {
                        let mut st = svc.lock();
                        if st.loading.get(&gid).is_some_and(|(lid, _)| *lid == id) {
                            st.loading.remove(&gid);
                        }
                    }
                    out
                }
                .boxed()
                .shared();
                st.loading.insert(guild_id.to_string(), (id, fut.clone()));
                Step::Started(fut)
            }
        };
        match step {
            Step::Hit(hit) => Ok(hit),
            Step::Pending(p) => p.await,
            Step::Started(fut) => fut.await,
        }
    }

    /// `getSettingsSafe(guildId)`: like [`SettingsService::get_settings`] but never fails: logs a warning (at most one
    /// per guild per minute) and returns `None`.
    pub async fn get_settings_safe(&self, guild_id: &str) -> Option<Settings> {
        match self.get_settings(guild_id).await {
            Ok(s) => Some(s),
            Err(e) => {
                if self.inner.warned.allow(guild_id) {
                    bot_util::log_warn!(format!("Could not load settings for guild {guild_id}: {e}"));
                }
                None
            }
        }
    }

    /// `peekSettings(guildId)`: memo only, synchronous, no I/O.
    pub fn peek_settings(&self, guild_id: &str) -> Option<Settings> {
        let now = self.now();
        self.lock().memo_get(guild_id, now)
    }

    /// `peekSettings(id) ?? await withTimeout(getSettingsSafe(id), budgetMs).catch(() => null)`: the memo, else a load
    /// bounded by `budget_ms` (the load keeps running after a timeout and fills the memo). `None` on a timeout or an
    /// error.
    pub async fn peek_or_load(&self, guild_id: &str, budget_ms: u64) -> Option<Settings> {
        if let Some(s) = self.peek_settings(guild_id) {
            return Some(s);
        }
        let (svc, gid) = (self.clone(), guild_id.to_string());
        let load = async move { Ok::<_, std::convert::Infallible>(svc.get_settings_safe(&gid).await) };
        bot_util::async_util::with_timeout(load, budget_ms, "settings").await.ok().flatten()
    }

    /// `ensureGuildSettings(guildId)`: makes sure the row exists (every per-guild model has an FK to it). Returns at
    /// once when the guild was ensured in this process. Call it before creating any per-guild row.
    pub async fn ensure_guild_settings(&self, guild_id: &str) -> Result<(), SettingsError> {
        if self.lock().ensured.contains_key(guild_id) {
            return Ok(());
        }
        match self.inner.store.create_if_missing(guild_id).await {
            Ok(_) => {}
            // A concurrent first write for the same guild won the insert: the row exists either way.
            Err(e) if e.is_unique_violation() => {}
            Err(e) => return Err(e.into()),
        }
        self.lock().mark_ensured(guild_id);
        Ok(())
    }

    fn write_through(&self, guild_id: &str, row: GuildSettings) -> Settings {
        let row = Arc::new(row);
        let now = self.now();
        let mut st = self.lock();
        st.bump_generation(guild_id);
        st.memo_set(guild_id, row.clone(), now);
        st.mark_ensured(guild_id);
        row
    }

    /// A failed SET would leave the previous row in Redis, and the read-through would bring it back once the memo
    /// expires; so when the write fails, the key is dropped.
    async fn publish_row(&self, guild_id: &str, row: &GuildSettings) {
        let key = settings_key(guild_id);
        if !self.inner.cache.set(&key, &with_marker(row), SETTINGS_TTL_SECONDS).await {
            self.inner.cache.del_one(&key).await;
        }
    }

    /// `updateSettings(guildId, data)`: upserts scalar fields and writes the row through to the memo and Redis. Use
    /// [`SettingsService::update_settings_with`] for arrays and JSON.
    pub async fn update_settings(&self, guild_id: &str, data: SettingsPatch) -> Result<Settings, SettingsError> {
        let row = match self.inner.store.upsert(guild_id, &data).await {
            Ok(r) => r,
            // Lost the insert race to a concurrent first write: the row exists now.
            Err(e) if e.is_unique_violation() => self.inner.store.update(guild_id, &data).await?,
            Err(e) => return Err(e.into()),
        };
        let published = row.clone();
        let revived = self.write_through(guild_id, row);
        self.publish_row(guild_id, &published).await;
        Ok(revived)
    }

    /// `updateSettingsWith(guildId, mutator)`: a read-modify-write without lost updates. The row is locked
    /// (`FOR UPDATE`) while `mutator` runs on an owned copy; it returns the fields to change (empty: none). The
    /// mutator must be pure (no Discord calls, no slow work). Its error rolls back and is returned unchanged
    /// (downcast it); a database failure is a [`SettingsError`] / `DbError` inside the error.
    pub async fn update_settings_with<F>(&self, guild_id: &str, mutator: F) -> anyhow::Result<Settings>
    where
        F: FnOnce(GuildSettings) -> anyhow::Result<SettingsPatch> + Send + 'static,
    {
        let row = self.inner.store.update_with(guild_id, Box::new(mutator)).await?;
        let published = row.clone();
        let revived = self.write_through(guild_id, row);
        self.publish_row(guild_id, &published).await;
        Ok(revived)
    }

    /// `invalidateSettings(guildId)`: drops the memo entry, the ensured flag and the Redis key (guild join/leave
    /// only). Never fails.
    pub async fn invalidate_settings(&self, guild_id: &str) {
        {
            let mut st = self.lock();
            st.bump_generation(guild_id);
            st.memo.shift_remove(guild_id);
            st.ensured.shift_remove(guild_id);
            st.loading.remove(guild_id);
        }
        self.inner.cache.del_one(&settings_key(guild_id)).await;
    }

    /// Guild ids currently memoised (diagnostics and tests).
    pub fn memoised_guilds(&self) -> HashSet<String> {
        self.lock().memo.keys().cloned().collect()
    }
}
