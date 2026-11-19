//! The application context: one shared object passed to every command, autocomplete, component, hook and job.
//!
//! It holds what the JS kept in module-level singletons: the database pool, the cache, the settings service, the
//! scheduler, the rewards bus, the hook / component / command registries, the live collectors, and (once connected)
//! the Discord HTTP client and cache. Nothing is global, so tests build as many independent apps as they like
//! ([`crate::testing::TestApp`]), offline: in-memory cache, settings and job stores, a lazily connected pool.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use bot_db::PgPool;

use crate::cache::{Cache, CacheBackend, MemoryBackend, RedisBackend, RedisOptions};
use crate::clock::{SharedClock, SystemClock};
use crate::collectors::CollectorHub;
use crate::command::CommandRegistry;
use crate::component_registry::ComponentRegistry;
use crate::discord::{DiscordHandle, GuildDirectory, StaticGuilds};
use crate::hooks::{HookCtx, HookEvent, HookRegistry, ReadyGate};
use crate::interaction::{DisconnectedBackend, InteractionBackend, SerenityBackend};
use crate::rewards::RewardsBus;
use crate::scheduler::{JobStore, MemoryJobStore, PgJobStore, Scheduler};
use crate::settings::{MemorySettingsStore, PgSettingsStore, SettingsService, SettingsStore};

/// The environment keys the framework reads.
#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    /// `BOT_OWNER_ID`: the user with the OWNER level.
    pub bot_owner_id: Option<String>,
    /// `CLIENT_ID`.
    pub client_id: Option<String>,
    /// `DEPLOY_GUILD_ID`.
    pub deploy_guild_id: Option<String>,
    /// `NODE_ENV`.
    pub node_env: Option<String>,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self { bot_owner_id: get("BOT_OWNER_ID"), client_id: get("CLIENT_ID"), deploy_guild_id: get("DEPLOY_GUILD_ID"), node_env: get("NODE_ENV") }
    }
}

struct AppInner {
    config: AppConfig,
    clock: SharedClock,
    db: PgPool,
    cache: Cache,
    settings: SettingsService,
    scheduler: Scheduler,
    rewards: RewardsBus,
    hooks: HookRegistry,
    components: ComponentRegistry,
    commands: CommandRegistry,
    collectors: CollectorHub,
    ready_gate: Arc<ReadyGate>,
    discord: OnceLock<DiscordHandle>,
    directory: RwLock<Option<Arc<dyn GuildDirectory>>>,
    interaction_backend: RwLock<Option<Arc<dyn InteractionBackend>>>,
    extensions: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
    redis: Option<RedisBackend>,
    ready_at: OnceLock<i64>,
}

/// The application context. Cheap to clone (an `Arc`).
#[derive(Clone)]
pub struct App(Arc<AppInner>);

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("discord", &self.0.discord.get().is_some()).finish()
    }
}

/// Builds an [`App`]. Every part has an offline default (in-memory cache and stores, a pool that never connects,
/// the system clock); [`AppBuilder::production`] wires PostgreSQL and Redis.
pub struct AppBuilder {
    config: AppConfig,
    clock: SharedClock,
    db: Option<PgPool>,
    cache_backend: Option<Arc<dyn CacheBackend>>,
    redis: Option<RedisBackend>,
    settings_store: Option<Arc<dyn SettingsStore>>,
    job_store: Option<Arc<dyn JobStore>>,
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self { config: AppConfig::default(), clock: Arc::new(SystemClock), db: None, cache_backend: None, redis: None, settings_store: None, job_store: None }
    }
}

/// A pool that never connects (acquire fails within 500 ms): what offline tests run against.
pub fn unreachable_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .max_connections(1)
        .connect_lazy("postgresql://offline:offline@127.0.0.1:1/offline")
        .expect("static URL parses")
}

impl AppBuilder {
    pub fn config(mut self, config: AppConfig) -> Self {
        self.config = config;
        self
    }

    pub fn clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// The pool raw SQL runs on (and, unless other stores are set, the PostgreSQL settings and job stores).
    pub fn pool(mut self, pool: PgPool) -> Self {
        self.db = Some(pool);
        self
    }

    pub fn cache_backend(mut self, backend: Arc<dyn CacheBackend>) -> Self {
        self.cache_backend = Some(backend);
        self
    }

    /// Redis with the JS client's options (connects in the background; call inside a tokio runtime).
    pub fn redis(mut self, url: Option<&str>) -> Result<Self, crate::cache::CacheError> {
        let backend = RedisBackend::connect(RedisOptions::new(url))?;
        self.cache_backend = Some(Arc::new(backend.clone()));
        self.redis = Some(backend);
        Ok(self)
    }

    pub fn settings_store(mut self, store: Arc<dyn SettingsStore>) -> Self {
        self.settings_store = Some(store);
        self
    }

    pub fn job_store(mut self, store: Arc<dyn JobStore>) -> Self {
        self.job_store = Some(store);
        self
    }

    /// PostgreSQL stores on `pool` (settings and jobs) and the pool itself.
    pub fn postgres(self, pool: PgPool) -> Self {
        let settings: Arc<dyn SettingsStore> = Arc::new(PgSettingsStore::new(pool.clone()));
        let jobs: Arc<dyn JobStore> = Arc::new(PgJobStore::new(pool.clone()));
        self.pool(pool).settings_store(settings).job_store(jobs)
    }

    /// The production wiring: PostgreSQL stores on `pool`, Redis at `redis_url`, config from the environment.
    pub fn production(pool: PgPool, redis_url: Option<&str>) -> Result<Self, crate::cache::CacheError> {
        Self::default().config(AppConfig::from_env()).postgres(pool).redis(redis_url)
    }

    pub fn build(self) -> App {
        let clock = self.clock;
        let db = self.db.unwrap_or_else(unreachable_pool);
        let backend = self.cache_backend.unwrap_or_else(|| Arc::new(MemoryBackend::new(clock.clone())));
        let cache = Cache::new(backend, clock.clone());
        let settings_store = self.settings_store.unwrap_or_else(|| Arc::new(MemorySettingsStore::new()));
        let job_store = self.job_store.unwrap_or_else(|| Arc::new(MemoryJobStore::new(clock.clone())));
        let settings = SettingsService::new(settings_store, cache.clone(), clock.clone());
        let scheduler = Scheduler::new(job_store, settings.clone(), clock.clone());
        App(Arc::new(AppInner {
            config: self.config,
            clock,
            db,
            cache,
            settings,
            scheduler,
            rewards: RewardsBus::new(),
            hooks: HookRegistry::new(),
            components: ComponentRegistry::new(),
            commands: CommandRegistry::new(),
            collectors: CollectorHub::new(),
            ready_gate: Arc::new(ReadyGate::default()),
            discord: OnceLock::new(),
            directory: RwLock::new(None),
            interaction_backend: RwLock::new(None),
            extensions: Mutex::new(HashMap::new()),
            redis: self.redis,
            ready_at: OnceLock::new(),
        }))
    }
}

impl App {
    pub fn builder() -> AppBuilder {
        AppBuilder::default()
    }

    pub fn config(&self) -> &AppConfig {
        &self.0.config
    }

    /// The shared clock (`Date.now()`).
    pub fn clock(&self) -> &SharedClock {
        &self.0.clock
    }

    /// `prisma`: the PostgreSQL pool for feature SQL (through bot-db).
    pub fn db(&self) -> &PgPool {
        &self.0.db
    }

    /// `cache` (redis.js).
    pub fn cache(&self) -> &Cache {
        &self.0.cache
    }

    /// The settings service (settingsService.js).
    pub fn settings(&self) -> &SettingsService {
        &self.0.settings
    }

    /// The scheduler (scheduler.js).
    pub fn scheduler(&self) -> &Scheduler {
        &self.0.scheduler
    }

    /// The rewards bus (rewards.js).
    pub fn rewards(&self) -> &RewardsBus {
        &self.0.rewards
    }

    /// The hook registry (hookRegistry.js).
    pub fn hooks(&self) -> &HookRegistry {
        &self.0.hooks
    }

    /// `client.buttonHandlers` / `selectHandlers` / `modalHandlers`.
    pub fn components(&self) -> &ComponentRegistry {
        &self.0.components
    }

    /// `client.commands`.
    pub fn commands(&self) -> &CommandRegistry {
        &self.0.commands
    }

    /// The live collectors.
    pub fn collectors(&self) -> &CollectorHub {
        &self.0.collectors
    }

    /// The guildAvailable gate.
    pub fn ready_gate(&self) -> Arc<ReadyGate> {
        self.0.ready_gate.clone()
    }

    /// `runHooks(event, fields)`: see [`HookRegistry::run`].
    pub fn run_hooks<E: HookEvent>(&self, data: E) -> impl Future<Output = Arc<HookCtx<E>>> + Send + 'static {
        self.0.hooks.run(self, data)
    }

    /// The Discord connection, once part B's boot installed it.
    pub fn discord(&self) -> Option<&DiscordHandle> {
        self.0.discord.get()
    }

    /// Installs the Discord connection (once; a second call is ignored and returns `false`).
    pub fn set_discord(&self, handle: DiscordHandle) -> bool {
        self.0.discord.set(handle).is_ok()
    }

    /// The serenity HTTP client, when connected.
    pub fn http(&self) -> Option<Arc<serenity::all::Http>> {
        self.discord().map(|d| d.http.clone())
    }

    /// The guilds this process holds: an override (tests), else the Discord cache, else none.
    pub fn guild_directory(&self) -> Arc<dyn GuildDirectory> {
        if let Some(d) = self.0.directory.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return d;
        }
        match self.discord() {
            Some(h) => Arc::new(h.clone()),
            None => Arc::new(StaticGuilds::default()),
        }
    }

    /// Overrides the guild directory (tests: which guilds are available, sharding).
    pub fn set_guild_directory(&self, directory: Arc<dyn GuildDirectory>) {
        *self.0.directory.write().unwrap_or_else(|e| e.into_inner()) = Some(directory);
    }

    /// Where interaction responses go: an override (tests), else serenity's HTTP client, else nowhere.
    pub fn interaction_backend(&self) -> Arc<dyn InteractionBackend> {
        if let Some(b) = self.0.interaction_backend.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return b;
        }
        match self.discord() {
            Some(h) => Arc::new(SerenityBackend { http: h.http.clone() }),
            None => Arc::new(DisconnectedBackend),
        }
    }

    /// Overrides the interaction backend (tests: a [`crate::interaction::RecordingBackend`]).
    pub fn set_interaction_backend(&self, backend: Arc<dyn InteractionBackend>) {
        *self.0.interaction_backend.write().unwrap_or_else(|e| e.into_inner()) = Some(backend);
    }

    /// Per-app state of a feature (what the JS kept in module-level variables: memos, indexes, throttles). Created
    /// with `T::default()` on first use; one instance per app and type.
    pub fn ext<T: Any + Send + Sync + Default>(&self) -> Arc<T> {
        let mut map = self.0.extensions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(TypeId::of::<T>()).or_insert_with(|| Arc::new(T::default()) as Arc<dyn Any + Send + Sync>);
        entry.clone().downcast::<T>().unwrap_or_else(|_| unreachable!("extension type keyed by its TypeId"))
    }

    /// `client.readyTimestamp`: when this process became ready (the first shard's `ClientReady`, epoch ms on the app
    /// clock); `None` before that.
    pub fn ready_at_ms(&self) -> Option<i64> {
        self.0.ready_at.get().copied()
    }

    /// Records the ready time (the gateway does it once; tests may call it). Later calls are ignored (`false`).
    pub fn set_ready_at_ms(&self, ms: i64) -> bool {
        self.0.ready_at.set(ms).is_ok()
    }

    /// The Redis backend, when this app uses Redis (shutdown closes it).
    pub fn redis(&self) -> Option<&RedisBackend> {
        self.0.redis.as_ref()
    }

    /// `client.shutdown()`'s framework part: stops the scheduler (waiting up to `timeout_ms` for running jobs), then
    /// closes Redis and the pool.
    pub async fn shutdown(&self, timeout_ms: u64) {
        self.scheduler().stop(timeout_ms).await;
        if let Some(r) = self.redis() {
            r.disconnect();
        }
        bot_db::disconnect(self.db()).await;
    }
}
