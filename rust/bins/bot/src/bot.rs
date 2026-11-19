//! Port of `src/bot.js`: the client, its boot sequence and `client.shutdown()`.
//!
//! bot-core does the heavy lifting: [`bot_core::client::start_bot`] installs the registry (plugins, core components,
//! commands), registers the gateway intake (every `src/events/*.js` file) and logs in with the intents, cache
//! settings and default mentions of bot.js. This module wires the app to PostgreSQL and Redis, keeps the handles the
//! binary needs while the client task runs, and runs the shutdown in the bot.js order.

use std::sync::Arc;
use std::time::Duration;

use bot_core::client::{Bot, BotControl, SCHEDULER_STOP_MS, run_shutdown_flushes};
use bot_core::gateway::GatewayState;
use bot_core::{App, AppConfig};
use serenity::all::ShardManager;
use serenity::gateway::ConnectionStage;

/// `READY_CHECK_TIMEOUT_MS` of index.js (one `/readyz` probe of the shards).
pub const READY_CHECK_TIMEOUT_MS: u64 = 5_000;

/// The app with the production wiring: the PostgreSQL settings and job stores on `pool`, Redis at `redis_url` (the
/// JS client's options, connecting in the background), and `config` (`BOT_OWNER_ID`, `CLIENT_ID`, ...).
/// Call inside a tokio runtime.
pub fn build_app(pool: bot_db::PgPool, redis_url: Option<&str>, config: AppConfig) -> Result<App, bot_core::cache::CacheError> {
    Ok(App::builder().config(config).postgres(pool).redis(redis_url)?.build())
}

/// What the binary keeps of a running [`Bot`] while [`Bot::wait`] owns the client task.
#[derive(Clone)]
pub struct BotHandle {
    pub app: App,
    pub shard_manager: Arc<ShardManager>,
    pub gateway: Arc<GatewayState>,
    pub shards: u32,
}

impl From<BotControl> for BotHandle {
    fn from(c: BotControl) -> Self {
        Self { app: c.app, shard_manager: c.shard_manager, gateway: c.gateway, shards: c.shards }
    }
}

impl BotHandle {
    pub fn of(bot: &Bot) -> Self {
        Self::from(bot.control())
    }

    /// `client.isReady()` of shard `id`: its `ClientReady` fired and its connection is up.
    pub async fn shard_is_ready(&self, id: u32) -> bool {
        if !self.gateway.shard_ready(id) {
            return false;
        }
        let runners = self.shard_manager.runners.lock().await;
        runners.get(&serenity::all::ShardId(id)).is_some_and(|r| r.stage == ConnectionStage::Connected)
    }

    /// `manager.broadcastEval((c) => c.isReady())`: one flag per shard.
    pub async fn ready_states(&self) -> Vec<bool> {
        let mut out = Vec::with_capacity(self.shards as usize);
        for id in 0..self.shards {
            out.push(self.shard_is_ready(id).await);
        }
        out
    }

    /// `client.shutdown()` (see [`shutdown`]).
    pub async fn shutdown(&self) {
        shutdown(&self.app, Some((&self.shard_manager, &self.gateway))).await;
    }
}

/// `client.shutdown()` of bot.js, in its order:
/// 1. stop claiming jobs and let the running ones finish (5 s);
/// 2. the feature flushes (achievement message counts 3 s, then the audit-log queue 4 s), while the database and
///    the client are still up;
/// 3. `client.destroy()`: every shard disconnects (skipped when there is no client, e.g. the start-up test);
/// 4. the database pool and Redis.
pub async fn shutdown(app: &App, client: Option<(&Arc<ShardManager>, &Arc<GatewayState>)>) {
    app.scheduler().stop(SCHEDULER_STOP_MS).await;
    run_shutdown_flushes(app).await;
    if let Some((manager, gateway)) = client {
        gateway.stop();
        manager.shutdown_all().await;
    }
    // Promise.allSettled([disconnectDatabase(), redis.quit() / redis.disconnect()])
    if let Some(r) = app.redis() {
        r.disconnect();
    }
    bot_db::disconnect(app.db()).await;
}

/// `client.readyTimestamp` for the levels voice credit. feat-levels now reads `App::ready_at_ms` itself, so the binary
/// no longer calls this; it pins the levels ready time to the app's once the first shard is ready.
pub async fn sync_levels_ready_at(app: App) {
    loop {
        if let Some(ms) = app.ready_at_ms() {
            feat_levels::xp_service::set_ready_at(&app, ms);
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
