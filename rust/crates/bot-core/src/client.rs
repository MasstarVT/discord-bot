//! Port of `src/bot.js`: the client boot the bot binary calls, and its shutdown (`client.shutdown()`).
//!
//! Boot order (as the JS): plugins (validated, installed: hooks, components, job handlers, reward providers, init),
//! the core components, the commands, then the events, then the login. Every shard runs in this one process
//! (serenity's shard manager); the shard count is Discord's recommendation, read before the shards start, so the
//! scheduler's shard arithmetic (`(guildId >> 22) % count`, the GC on the process holding shard 0) is right from the
//! first claim. On each shard's `ClientReady` the scheduler starts (once per process) and the guild startup work
//! runs ([`crate::gateway`]).

use std::future::Future;
use std::sync::{Arc, Mutex};

use serenity::all::{GatewayIntents, Http, HttpBuilder, ShardManager};
use serenity::gateway::GatewayError;

use crate::context::App;
use crate::discord::{DiscordHandle, default_allowed_mentions};
use crate::error::BoxFuture;
use crate::gateway::{GatewayState, watch_runners};
use crate::registry::{BootReport, Registry};

/// `MESSAGE_CACHE_LIFETIME_S` (3 h) and `MessageManager: 200 per channel` of bot.js.
pub const MESSAGES_PER_CHANNEL: usize = 200;
/// The scheduler gets this long to let running jobs finish at shutdown.
pub const SCHEDULER_STOP_MS: u64 = 5_000;
/// The number of JS event files (`Registered N event listener(s).`).
pub const EVENT_FILE_COUNT: usize = 38;

/// The gateway intents of bot.js; `presences` false is `DISABLE_PRESENCES=1` (no Presence intent).
pub fn intents(presences: bool) -> GatewayIntents {
    let mut i = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_MODERATION
        | GatewayIntents::GUILD_EMOJIS_AND_STICKERS
        | GatewayIntents::GUILD_INTEGRATIONS
        | GatewayIntents::GUILD_WEBHOOKS
        | GatewayIntents::GUILD_INVITES
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_SCHEDULED_EVENTS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    if presences {
        i |= GatewayIntents::GUILD_PRESENCES;
    }
    i
}

/// The cache policy: 200 messages per channel; members, presences, channels and users unlimited.
pub fn cache_settings() -> serenity::cache::Settings {
    let mut s = serenity::cache::Settings::default();
    s.max_messages = MESSAGES_PER_CHANNEL;
    s.cache_guilds = true;
    s.cache_channels = true;
    s.cache_users = true;
    s
}

/// The fatal gateway close code (4004, 4013, 4014) a serenity start error stands for, if any (`fatalCodeFromError`):
/// a bad token or invalid / disallowed intents never fix themselves, so the binary parks instead of retrying.
pub fn fatal_code(err: &serenity::Error) -> Option<u16> {
    match err {
        serenity::Error::Gateway(GatewayError::InvalidAuthentication) => Some(4004),
        serenity::Error::Gateway(GatewayError::InvalidGatewayIntents) => Some(4013),
        serenity::Error::Gateway(GatewayError::DisallowedGatewayIntents) => Some(4014),
        serenity::Error::Http(_) => crate::discord::error_status(err).filter(|s| *s == 401).map(|_| 4004),
        _ => None,
    }
}

/// A work flush that must run at shutdown, while Discord and the database are still up (the achievement message
/// counts, the audit-log queue).
type Flush = Arc<dyn Fn() -> BoxFuture<anyhow::Result<()>> + Send + Sync>;

struct FlushEntry {
    order: u32,
    label: &'static str,
    timeout_ms: u64,
    warn: &'static str,
    run: Flush,
}

/// The shutdown flushes features register (`app.ext::<ShutdownFlushes>()`).
#[derive(Default)]
pub struct ShutdownFlushes(Mutex<Vec<FlushEntry>>);

/// Registers a shutdown flush. They run after the scheduler stops, lowest `order` first (bot.js: achievement counts
/// `10`, 3 s; the audit-log queue `20`, 4 s), each bounded by `timeout_ms`; a failure or timeout logs
/// `<warn>: <error>` (for example `Log queue was not flushed before shutdown`) and the shutdown goes on.
pub fn on_shutdown<F, Fut>(app: &App, order: u32, label: &'static str, timeout_ms: u64, warn: &'static str, f: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let run: Flush = Arc::new(move || Box::pin(f()));
    let flushes = app.ext::<ShutdownFlushes>();
    let mut v = flushes.0.lock().unwrap_or_else(|e| e.into_inner());
    v.push(FlushEntry { order, label, timeout_ms, warn, run });
    v.sort_by_key(|e| e.order);
}

/// Runs the registered flushes in order. Never fails.
pub async fn run_shutdown_flushes(app: &App) {
    let entries: Vec<(u32, &'static str, u64, &'static str, Flush)> =
        app.ext::<ShutdownFlushes>().0.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|e| (e.order, e.label, e.timeout_ms, e.warn, e.run.clone())).collect();
    for (_, label, timeout_ms, warn, run) in entries {
        match bot_util::async_util::with_timeout(run(), timeout_ms, label).await {
            Ok(()) => {}
            Err(e) => bot_util::log_warn!(format!("{warn}: {e}")),
        }
    }
}

/// Why the boot failed.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("Discord login failed: {0}")]
    Discord(Box<serenity::Error>),
}

impl From<serenity::Error> for BootError {
    fn from(e: serenity::Error) -> Self {
        Self::Discord(Box::new(e))
    }
}

/// A running bot: the app, the shard manager and the gateway state.
pub struct Bot {
    pub app: App,
    pub shard_manager: Arc<ShardManager>,
    pub gateway: Arc<GatewayState>,
    pub boot: BootReport,
    pub shards: u32,
    client: tokio::task::JoinHandle<Result<(), serenity::Error>>,
}

/// Builds the HTTP client with the client-wide default mentions (`{ parse: ['users'], repliedUser: false }`).
pub fn http_client(token: &str) -> Http {
    HttpBuilder::new(token).default_allowed_mentions(default_allowed_mentions()).build()
}

/// `start()` of bot.js: installs the registry, connects every shard and returns once the shards are starting.
/// `presences` is `presencesEnabled()`.
pub async fn start_bot(app: App, registry: Registry, token: &str, presences: bool) -> Result<Bot, BootError> {
    // Plugins first: their hooks, persistent components, job handlers and reward providers must exist before any
    // event can fire. Then the core components and the commands.
    bot_util::log_info!("Loading commands...");
    let boot = registry.install(&app);
    bot_util::log_info!("Loading events...");
    crate::event_loader::log_registered(EVENT_FILE_COUNT);

    bot_util::log_info!("Logging in to Discord...");
    let shards = http_client(token).get_bot_gateway().await.map(|g| g.shards.max(1)).map_err(BootError::from)?;
    let client = serenity::all::ClientBuilder::new_with_http(http_client(token), intents(presences)).cache_settings(cache_settings()).await?;
    let ids = if shards > 1 { Some((0..u64::from(shards)).collect()) } else { None };
    app.set_discord(DiscordHandle::new(client.http.clone(), client.cache.clone(), ids, u64::from(shards)));
    let gateway = GatewayState::new(shards);
    let shard_manager = client.shard_manager.clone();
    watch_runners(app.clone(), gateway.clone(), shard_manager.runners.clone());
    let mut client = client;
    let task = tokio::spawn(async move { client.start_shards(shards).await });
    Ok(Bot { app, shard_manager, gateway, boot, shards, client: task })
}

/// The part of a running [`Bot`] that stays usable while its client task is awaited (see [`Bot::split`]).
#[derive(Clone)]
pub struct BotControl {
    pub app: App,
    pub shard_manager: Arc<ShardManager>,
    pub gateway: Arc<GatewayState>,
    pub shards: u32,
}

impl BotControl {
    /// `client.shutdown()`, as [`Bot::shutdown`].
    pub async fn shutdown(&self) {
        shutdown_parts(&self.app, &self.shard_manager, &self.gateway).await;
    }

    /// Whether every shard is connected, as [`Bot::all_shards_ready`].
    pub async fn all_shards_ready(&self) -> bool {
        shards_ready(&self.shard_manager, self.shards).await
    }

    /// The shard count in use.
    pub fn shard_count(&self) -> u32 {
        self.shards
    }
}

/// The client task of a split [`Bot`]: resolves when the shards stop, as [`Bot::wait`].
pub struct BotJoin(tokio::task::JoinHandle<Result<(), serenity::Error>>);

impl std::future::Future for BotJoin {
    type Output = Result<(), serenity::Error>;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx).map(join_result)
    }
}

#[allow(clippy::result_large_err)] // serenity's own error type
fn join_result(r: Result<Result<(), serenity::Error>, tokio::task::JoinError>) -> Result<(), serenity::Error> {
    match r {
        Ok(r) => r,
        Err(e) => Err(serenity::Error::Other(if e.is_panic() { "the client task panicked" } else { "the client task was cancelled" })),
    }
}

async fn shutdown_parts(app: &App, shard_manager: &Arc<ShardManager>, gateway: &Arc<GatewayState>) {
    app.scheduler().stop(SCHEDULER_STOP_MS).await;
    run_shutdown_flushes(app).await;
    gateway.stop();
    shard_manager.shutdown_all().await;
    if let Some(r) = app.redis() {
        r.disconnect();
    }
    bot_db::disconnect(app.db()).await;
}

async fn shards_ready(shard_manager: &Arc<ShardManager>, shards: u32) -> bool {
    let runners = shard_manager.runners.lock().await;
    runners.len() as u32 == shards && runners.values().all(|r| r.stage == serenity::gateway::ConnectionStage::Connected)
}

impl Bot {
    /// `client.shutdown()`: stops the scheduler (5 s for running jobs), runs the feature flushes (achievement counts,
    /// the audit-log queue), disconnects every shard, then closes the database pool and Redis.
    pub async fn shutdown(&self) {
        shutdown_parts(&self.app, &self.shard_manager, &self.gateway).await;
    }

    /// A handle that stays usable (shutdown, readiness) plus the client task to await, so `shutdown` can run while
    /// the client is awaited.
    pub fn split(self) -> (BotControl, BotJoin) {
        let control = BotControl { app: self.app, shard_manager: self.shard_manager, gateway: self.gateway, shards: self.shards };
        (control, BotJoin(self.client))
    }

    /// A clone of the control half without giving up the client task.
    pub fn control(&self) -> BotControl {
        BotControl { app: self.app.clone(), shard_manager: self.shard_manager.clone(), gateway: self.gateway.clone(), shards: self.shards }
    }

    /// Waits until the shards stop. An error is a login or gateway failure (see [`fatal_code`]).
    #[allow(clippy::result_large_err)] // serenity's own error type
    pub async fn wait(self) -> Result<(), serenity::Error> {
        join_result(self.client.await)
    }

    /// The shard count in use.
    pub fn shard_count(&self) -> u32 {
        self.shards
    }

    /// Whether every shard is connected (`/readyz`: every shard `isReady()`).
    pub async fn all_shards_ready(&self) -> bool {
        shards_ready(&self.shard_manager, self.shards).await
    }
}
