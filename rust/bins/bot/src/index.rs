//! Port of `index.js`: the process around the client.
//!
//! * `--deploy` (`npm run deploy`): push every slash command and exit ([`deploy_mode`]).
//! * Otherwise: the health endpoints, the signal handlers (the only ones in the app), the missing-config park, the
//!   restart brake, the auto-deploy, then the client (`ShardingManager` in the JS; here every shard runs in this
//!   process through serenity's shard manager), the 60 s ready wait, and the exits and parks of the shard events.
//! * Before all of that, when `DATABASE_URL` is set, the pending Prisma migrations are applied ([`migrate`]): the
//!   Docker image of the JS bot runs `prisma migrate deploy` before `node index.js`, and this binary replaces both.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_core::client::{fatal_code, start_bot};
use bot_core::command::{AUTO_DEPLOY_TIMEOUT_MS, AutoDeployOutcome, DeployConfig, auto_deploy_commands, auto_deploy_with_timeout, deploy_commands};
use bot_core::{App, AppConfig};
use bot_util::fatal_gateway::fatal_close_code;
use bot_util::health_server::{HealthServer, Readiness, ReadinessCheck, start_health_server};
use bot_util::{log_error, log_info, log_success, log_warn};
use serde_json::{Map, Value, json};

use crate::bot::{BotHandle, READY_CHECK_TIMEOUT_MS, build_app};
use crate::registry::build_registry;

/// `HEALTH_PORT` default.
pub const DEFAULT_HEALTH_PORT: u16 = 8081;
/// `SPAWN_READY_TIMEOUT_MS`: how long the start waits for every shard to be ready.
pub const SPAWN_READY_TIMEOUT_MS: u64 = 60_000;
/// `SHUTDOWN_TIMEOUT_MS`: Docker's stop_grace_period is 30 s.
pub const SHUTDOWN_TIMEOUT_MS: u64 = 25_000;
/// The shard shutdown bound (`SHUTDOWN_TIMEOUT_MS - 5_000`).
pub const SHARD_SHUTDOWN_TIMEOUT_MS: u64 = SHUTDOWN_TIMEOUT_MS - 5_000;
/// `PARK_REMINDER_MS`.
pub const PARK_REMINDER_MS: u64 = 10 * 60_000;
/// The variables the bot cannot start without.
pub const REQUIRED_ENV: [&str; 4] = ["DISCORD_TOKEN", "CLIENT_ID", "DATABASE_URL", "REDIS_URL"];

/// The environment index.js and bot.js read, captured once in `main` (after `.env` is loaded).
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub discord_token: Option<String>,
    pub client_id: Option<String>,
    pub database_url: Option<String>,
    pub redis_url: Option<String>,
    pub deploy_guild_id: Option<String>,
    /// `DEPLOY_HASH_FILE` (unset: `<cwd>/.deploy-hash`).
    pub deploy_hash_file: Option<String>,
    /// `HEALTH_PORT` as written.
    pub health_port: Option<String>,
    /// `STATE_DIR` as written.
    pub state_dir: Option<String>,
    /// `presencesEnabled()` (`DISABLE_PRESENCES`).
    pub presences: bool,
    /// The directory paths are resolved against: the repository root in the JS (`__dirname` of index.js), the
    /// working directory here (run the binary from the repository root).
    pub root: PathBuf,
    pub app_config: AppConfig,
}

impl Env {
    pub fn from_process() -> Self {
        // `process.env.X` is truthy-checked everywhere in index.js: an empty value counts as unset.
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            discord_token: get("DISCORD_TOKEN"),
            client_id: get("CLIENT_ID"),
            database_url: get("DATABASE_URL"),
            redis_url: get("REDIS_URL"),
            deploy_guild_id: get("DEPLOY_GUILD_ID"),
            deploy_hash_file: get("DEPLOY_HASH_FILE"),
            health_port: get("HEALTH_PORT"),
            state_dir: get("STATE_DIR"),
            presences: bot_util::gateway_features::presences_enabled(),
            root: std::env::current_dir().unwrap_or_default(),
            app_config: AppConfig::from_env(),
        }
    }

    /// `required.filter((k) => !process.env[k])`.
    pub fn missing(&self) -> Vec<&'static str> {
        let present = [&self.discord_token, &self.client_id, &self.database_url, &self.redis_url];
        REQUIRED_ENV.iter().zip(present).filter(|(_, v)| v.is_none()).map(|(k, _)| *k).collect()
    }

    /// `resolve(__dirname, process.env.STATE_DIR || 'state')`.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(self.state_dir.as_deref().unwrap_or("state"))
    }

    /// Where and as whom commands are deployed. `DEPLOY_HASH_FILE` is used as written (relative to the working
    /// directory, like `writeFileSync`); the default is `.deploy-hash` in the root.
    pub fn deploy_config(&self) -> DeployConfig {
        DeployConfig {
            token: self.discord_token.clone(),
            client_id: self.client_id.clone(),
            guild_id: self.deploy_guild_id.clone(),
            hash_file: self.deploy_hash_file.as_ref().map(PathBuf::from).unwrap_or_else(|| self.root.join(".deploy-hash")),
        }
    }
}

/// `Number(process.env.HEALTH_PORT || 8081)` as a port: a non-integer or out-of-range value is the error Node's
/// `server.listen` reports (it then goes to the health server's error handler).
pub fn parse_health_port(raw: Option<&str>) -> Result<u16, String> {
    let n = health_port_number(raw);
    if n.fract() == 0.0 && (0.0..65536.0).contains(&n) {
        Ok(n as u16)
    } else {
        Err(format!("options.port should be >= 0 and < 65536. Received {}.", bot_util::js::number_to_string(n)))
    }
}

/// `Number(process.env.HEALTH_PORT || 8081)` (decimal, `0x` / `0o` / `0b`, surrounding white space; NaN otherwise).
pub fn health_port_number(raw: Option<&str>) -> f64 {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else { return f64::from(DEFAULT_HEALTH_PORT) };
    let s = bot_util::js::trim(raw);
    if s.is_empty() {
        0.0
    } else if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN)
    } else if let Some(o) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        u64::from_str_radix(o, 8).map(|v| v as f64).unwrap_or(f64::NAN)
    } else if let Some(b) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        u64::from_str_radix(b, 2).map(|v| v as f64).unwrap_or(f64::NAN)
    } else if s.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        s.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// The start phase `/readyz` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    BrakeDelay,
    Spawned,
}

#[derive(Default)]
struct State {
    phase: Option<Phase>,
    brake_until_ms: f64,
    parked: Option<String>,
    app: Option<App>,
    bot: Option<BotHandle>,
    health: Option<HealthServer>,
}

/// The lifecycle state of index.js (`phase`, `brakeUntil`, `parked`, `shuttingDown`, `manager`).
#[derive(Default)]
pub struct Lifecycle {
    state: Mutex<State>,
    shutting_down: AtomicBool,
    /// The client task is running (the JS `shard.worker` is alive).
    client_running: AtomicBool,
}

impl Lifecycle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn phase(&self) -> Phase {
        self.st().phase.unwrap_or(Phase::Starting)
    }

    pub fn set_phase(&self, phase: Phase) {
        self.st().phase = Some(phase);
    }

    pub fn parked(&self) -> Option<String> {
        self.st().parked.clone()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn set_app(&self, app: App) {
        self.st().app = Some(app);
    }

    pub fn set_bot(&self, bot: BotHandle) {
        self.client_running.store(true, Ordering::SeqCst);
        self.st().bot = Some(bot);
    }

    pub fn set_brake_until(&self, ms: f64) {
        self.st().brake_until_ms = ms;
    }

    /// `getReadiness()`.
    pub async fn readiness(&self) -> Result<Readiness, String> {
        let (parked, phase, until, bot) = {
            let s = self.st();
            (s.parked.clone(), s.phase.unwrap_or(Phase::Starting), s.brake_until_ms, s.bot.clone())
        };
        if let Some(code) = parked {
            return Ok(Readiness::not_ready(&format!("parked:{code}")));
        }
        if phase == Phase::BrakeDelay {
            let mut r = Readiness::not_ready("brake-delay");
            r.details.insert("until".into(), json!(bot_util::js::to_iso_string(until)));
            return Ok(r);
        }
        let Some(bot) = bot.filter(|_| phase == Phase::Spawned) else { return Ok(Readiness::not_ready("starting")) };
        let states = match tokio::time::timeout(Duration::from_millis(READY_CHECK_TIMEOUT_MS), bot.ready_states()).await {
            Ok(s) => s,
            Err(_) => return Err(format!("Shard ready check timed out after {READY_CHECK_TIMEOUT_MS} ms")),
        };
        Ok(readiness_of(&states))
    }

    /// `park(code, message)`: stops for good without exiting (no retry, no restart by Docker): `/healthz` stays 200,
    /// `/readyz` reports `parked:<code>`, and the reason is repeated every 10 minutes.
    pub fn park(self: &Arc<Self>, code: &str, message: Option<String>) {
        {
            let mut s = self.st();
            if s.parked.is_some() {
                return;
            }
            s.parked = Some(code.to_string());
        }
        let text = message.unwrap_or_else(|| park_text(code));
        log_error!(format!("PARKED: {text}"));
        log_error!(format!("Not retrying. /healthz stays 200 and /readyz returns 503 \"parked:{code}\" until the bot is restarted."));
        let code = code.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(PARK_REMINDER_MS)).await;
                log_error!(format!("Still parked ({code}): {text}"));
            }
        });
    }
}

/// The default park text for a fatal close code.
pub fn park_text(code: &str) -> String {
    match code.parse::<u16>().ok().and_then(fatal_close_code) {
        Some(c) => format!("Discord closed the gateway with {} ({}). {}", c.code, c.name, c.fix),
        None => format!("Discord closed the gateway with {code}."),
    }
}

/// The `/readyz` answer for the shards' `isReady()` flags.
pub fn readiness_of(states: &[bool]) -> Readiness {
    let mut details = Map::new();
    if !states.is_empty() && states.iter().all(|s| *s) {
        details.insert("shards".into(), json!(states.len()));
        return Readiness { ready: true, details };
    }
    details.insert("reason".into(), Value::String("shard-not-ready".into()));
    details.insert("shards".into(), json!(states.len()));
    Readiness { ready: false, details }
}

/// `deployCommands()` for `node index.js --deploy`: the exit code (0 deployed, 1 failed).
pub async fn deploy_mode(cfg: &DeployConfig) -> i32 {
    log_info!("Deploy mode — registering slash commands...");
    let registry = build_registry();
    match deploy_commands(cfg, registry.payloads()).await {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

/// `prisma migrate deploy` (the Docker CMD of the JS bot, run before `node index.js` when `DATABASE_URL` is set).
pub async fn migrate(database_url: &str) -> anyhow::Result<bot_db::migrate::MigrateReport> {
    let cfg = bot_db::parse_database_url(database_url)?;
    let pool = bot_db::connect_with(&cfg).await?;
    let report = bot_db::migrate::migrate_deploy(&pool, &cfg.schema).await;
    pool.close().await;
    Ok(report?)
}

/// [`migrate`] with the log lines; false when the start must stop (`|| exit 1`).
pub async fn migrate_logged(database_url: &str) -> bool {
    match migrate(database_url).await {
        Ok(r) => {
            if r.applied.is_empty() {
                log_info!("No pending migrations to apply.");
            } else {
                log_info!(format!("Applied {} migration(s): {}", r.applied.len(), r.applied.join(", ")));
                log_success!("All migrations have been successfully applied.");
            }
            for name in &r.missing_locally {
                log_warn!(format!("Migration {name} is applied in the database but not part of this build."));
            }
            for name in &r.modified {
                log_warn!(bot_db::migrate::modified_warning(name));
            }
            true
        }
        Err(e) => {
            log_error!("Database migration failed", bot_core::error::log_arg(&e));
            false
        }
    }
}

/// Starts the health endpoints on `HEALTH_PORT` (before anything slow, so Docker's HEALTHCHECK passes during the
/// restart brake and while parked).
pub async fn start_health(lc: &Arc<Lifecycle>, raw_port: Option<&str>) {
    // The JS logs HEALTH_PORT after Number(): `0x1F91` prints as 8081, `abc` as NaN.
    let label = bot_util::js::number_to_string(health_port_number(raw_port));
    let on_error = {
        let label = label.clone();
        move |e: std::io::Error| {
            log_error!(format!("Health server on port {label} failed — /healthz and /readyz are unavailable"), bot_util::logger::LogArg::error(&e));
        }
    };
    let port = match parse_health_port(raw_port) {
        Ok(p) => p,
        Err(msg) => {
            on_error(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg));
            return;
        }
    };
    let weak = Arc::downgrade(lc);
    let check: ReadinessCheck = Arc::new(move || {
        let lc = weak.upgrade();
        Box::pin(async move {
            match lc {
                Some(lc) => lc.readiness().await,
                None => Ok(Readiness::not_ready("starting")),
            }
        })
    });
    if let Some(server) = start_health_server(port, check, on_error).await {
        log_info!(format!("Health endpoints listening on :{label} (/healthz, /readyz)"));
        lc.st().health = Some(server);
    }
}

/// `shutdown(signal)`: the second signal exits at once; otherwise the shards disconnect (bounded at 20 s, the whole
/// shutdown at 25 s) and the process exits 0.
pub async fn shutdown(lc: Arc<Lifecycle>, signal: &str) {
    if lc.shutting_down.swap(true, Ordering::SeqCst) {
        log_warn!(format!("{signal} received again — exiting now."));
        std::process::exit(1);
    }
    log_info!(format!("{signal} received — shutting down..."));
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(SHUTDOWN_TIMEOUT_MS));
        log_error!(format!("Shutdown took longer than {} s — forcing exit.", SHUTDOWN_TIMEOUT_MS / 1000));
        std::process::exit(1);
    });

    let (health, bot, app) = {
        let mut s = lc.st();
        (s.health.take(), s.bot.clone(), s.app.clone())
    };
    if let Some(h) = health {
        h.close();
    }

    let live = lc.client_running.load(Ordering::SeqCst);
    match (bot, app) {
        (Some(bot), _) if live => {
            let n = bot.shards;
            match tokio::time::timeout(Duration::from_millis(SHARD_SHUTDOWN_TIMEOUT_MS), bot.shutdown()).await {
                Ok(()) => log_info!(format!("Disconnected {n} shard(s) from Discord.")),
                Err(_) => log_warn!(format!("Shard shutdown incomplete: Shard shutdown timed out after {SHARD_SHUTDOWN_TIMEOUT_MS} ms")),
            }
        }
        // No live client (still starting, parked or dead): close the database and Redis quietly.
        (_, Some(app)) => {
            let _ = tokio::time::timeout(Duration::from_millis(SHARD_SHUTDOWN_TIMEOUT_MS), crate::bot::shutdown(&app, None)).await;
        }
        _ => {}
    }
    std::process::exit(0);
}

/// Listens for SIGTERM and SIGINT (Ctrl+C on Windows) and runs [`shutdown`] for each.
pub fn install_signal_handlers(lc: &Arc<Lifecycle>) {
    let lc = lc.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
                log_error!("Could not install the signal handlers");
                return;
            };
            loop {
                let name = tokio::select! {
                    _ = term.recv() => "SIGTERM",
                    _ = int.recv() => "SIGINT",
                };
                tokio::spawn(shutdown(lc.clone(), name));
            }
        }
        #[cfg(not(unix))]
        {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    log_error!("Could not install the signal handlers");
                    return;
                }
                tokio::spawn(shutdown(lc.clone(), "SIGINT"));
            }
        }
    });
}

/// The restart brake step of `boot()`: records the start and sleeps when crash-looping.
pub async fn restart_brake(lc: &Arc<Lifecycle>, state_dir: &Path) {
    let now = bot_util::js::now_ms();
    let brake = bot_util::restart_brake::record_start(state_dir, now);
    if let Some(w) = &brake.warning {
        log_warn!(format!("Restart brake: {w}"));
    }
    if brake.delay_ms > 0 {
        let until = now + brake.delay_ms as f64;
        lc.set_brake_until(until);
        lc.set_phase(Phase::BrakeDelay);
        log_warn!(format!(
            "Restart brake: {} starts in the last hour — waiting {} min before connecting to Discord (until {}).",
            brake.starts,
            bot_util::js::number_to_string(brake.delay_ms as f64 / 60_000.0),
            bot_util::js::to_iso_string(until).unwrap_or_default()
        ));
        tokio::time::sleep(Duration::from_millis(brake.delay_ms)).await;
        lc.set_phase(Phase::Starting);
    } else {
        log_info!(format!("Restart brake: start {} in the last hour (more than 5 are delayed).", brake.starts));
    }
}

/// Waits until every shard is ready (logging `Shard N is ready.` as each one gets there), at most `timeout_ms`;
/// whether all were ready in time. Keeps logging late shards in the background after a timeout.
async fn wait_ready(bot: &BotHandle, timeout_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut seen = vec![false; bot.shards as usize];
    loop {
        for id in 0..bot.shards {
            if !seen[id as usize] && bot.gateway.shard_ready(id) {
                seen[id as usize] = true;
                log_success!(format!("Shard {id} is ready."));
            }
        }
        if seen.iter().all(|s| *s) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            let (b, mut seen) = (bot.clone(), seen);
            tokio::spawn(async move {
                while !seen.iter().all(|s| *s) {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    for id in 0..b.shards {
                        if !seen[id as usize] && b.gateway.shard_ready(id) {
                            seen[id as usize] = true;
                            log_success!(format!("Shard {id} is ready."));
                        }
                    }
                }
            });
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// What happens when the client task ends (the JS `disconnect` and `death` shard events).
fn on_client_exit(lc: &Arc<Lifecycle>, joined: Result<Result<(), serenity::Error>, tokio::task::JoinError>) {
    lc.client_running.store(false, Ordering::SeqCst);
    if lc.is_shutting_down() {
        return;
    }
    let result = match joined {
        Ok(r) => r,
        Err(e) => Err(serenity::Error::Other(if e.is_panic() { "the client task panicked" } else { "the client task was cancelled" })),
    };
    if let Err(e) = &result
        && let Some(code) = fatal_code(e)
    {
        lc.park(&code.to_string(), None);
        return;
    }
    if let Some(p) = lc.parked() {
        log_warn!(format!("Shard 0 exited (parked: {p})."));
        return;
    }
    match result {
        Err(e) => log_error!(
            "Shard 0 died — exiting so Docker restarts the bot (the restart brake paces retries).",
            bot_util::logger::LogArg::error(&e)
        ),
        Ok(()) => log_error!("Shard 0 disconnected and won't reconnect — exiting so Docker restarts the bot (the restart brake paces retries)."),
    }
    std::process::exit(1);
}

/// `boot()`: returns when the start is over (parked, or the client stopped while shutting down); exits the process
/// where the JS does.
pub async fn boot(lc: Arc<Lifecycle>, env: Env) -> anyhow::Result<()> {
    // Missing config can't fix itself, so park rather than crash-loop.
    let missing = env.missing();
    if !missing.is_empty() {
        lc.park(
            "missing-env",
            Some(format!(
                "Missing required environment variables: {}. Copy .env.example to .env (or set them on the container) and restart.",
                missing.join(", ")
            )),
        );
        return Ok(());
    }

    // Restart brake: pace crash loops so they can't use up the daily login limit.
    restart_brake(&lc, &env.state_dir()).await;

    // Auto-deploy slash commands whenever the command set has changed.
    let registry = build_registry();
    let cfg = env.deploy_config();
    let payloads = registry.payloads();
    // Never fails: a failed or stuck deploy is logged and the bot starts with the commands Discord already has.
    let _: AutoDeployOutcome = auto_deploy_with_timeout(async move { auto_deploy_commands(&cfg, payloads).await }, AUTO_DEPLOY_TIMEOUT_MS).await;

    // ── The client (the ShardingManager and the shard's start() in the JS) ──────
    let database_url = env.database_url.clone().unwrap_or_default();
    let pool = bot_db::connect_lazy(&database_url)?;
    let app = build_app(pool, env.redis_url.as_deref(), env.app_config.clone()).map_err(|e| anyhow::anyhow!(e.0))?;
    lc.set_app(app.clone());

    log_info!("Starting ShardingManager...");
    let token = env.discord_token.clone().unwrap_or_default();
    let bot = match start_bot(app.clone(), registry, &token, env.presences).await {
        Ok(b) => b,
        Err(bot_core::client::BootError::Discord(err)) => {
            if lc.parked().is_some() || lc.is_shutting_down() {
                return Ok(());
            }
            if let Some(code) = fatal_code(&err) {
                if let Some(c) = fatal_close_code(code) {
                    log_error!(format!("Discord login failed with {} ({}). {}", c.code, c.name, c.fix));
                }
                lc.park(&code.to_string(), None);
                return Ok(());
            }
            log_error!(
                "Failed to spawn shards — exiting so Docker restarts the bot (the restart brake paces retries).",
                bot_util::logger::LogArg::error(err.as_ref())
            );
            std::process::exit(1);
        }
    };
    let (control, join) = bot.split();
    let handle = BotHandle::from(control);
    for id in 0..handle.shards {
        log_info!(format!("Spawning Shard {id}..."));
    }
    lc.set_bot(handle.clone());

    let mut client = tokio::spawn(join);
    tokio::select! {
        r = &mut client => {
            on_client_exit(&lc, r);
            return Ok(());
        }
        all = wait_ready(&handle, SPAWN_READY_TIMEOUT_MS) => {
            if !all && lc.parked().is_none() && !lc.is_shutting_down() {
                // Every shard is running but slow to get ready (e.g. a Discord incident): keep going and let /readyz
                // report it instead of forcing another login.
                log_warn!(format!(
                    "Shards not ready after {} s — still waiting; /readyz returns 503 until they are.",
                    SPAWN_READY_TIMEOUT_MS / 1000
                ));
            }
        }
    }
    lc.set_phase(Phase::Spawned);
    let r = client.await;
    on_client_exit(&lc, r);
    Ok(())
}

/// `node index.js`: everything after `.env`. Runs until a signal or a fatal exit; returns an exit code only when the
/// start fails before the client exists.
pub async fn run(env: Env) -> i32 {
    // The Docker CMD: `if [ -n "$DATABASE_URL" ]; then prisma migrate deploy || exit 1; fi`.
    if let Some(url) = env.database_url.as_deref()
        && !migrate_logged(url).await
    {
        return 1;
    }

    let lc = Lifecycle::new();
    start_health(&lc, env.health_port.as_deref()).await;
    install_signal_handlers(&lc);

    if let Err(err) = boot(lc.clone(), env).await {
        log_error!("Fatal error during startup", bot_core::error::log_arg(&err));
        return 1;
    }
    // Parked, or the client stopped during a shutdown: stay up; the signal handlers end the process.
    std::future::pending::<()>().await;
    0
}
