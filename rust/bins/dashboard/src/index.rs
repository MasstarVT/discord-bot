//! Port of `dashboard.js`: the web dashboard entry point (a separate process from the bot, sharing its database and
//! Redis). Missing configuration parks the process instead of crash-looping: /healthz stays 200 and every other path
//! answers 503 naming the keys to set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bot_core::{App, AppConfig};
use bot_util::{log_error, log_info, log_success, log_warn};
use dash_auth::DashboardConfig;
use dash_collections::DashboardDiscord;
use dash_server::{BotLogger, DashLogger, DashboardDeps, DiscordApi, ServerDiscord, ServerOptions, create_dashboard_router, create_parked_router};
use tokio::sync::Notify;

use crate::bot_joins::DbBotJoins;
use crate::records_bridge::{DashboardRecords, build_store_registry};
use crate::settings_bridge::DashboardSettings;
use crate::tasks::TaskClient;

/// `SHUTDOWN_TIMEOUT_MS`.
pub const SHUTDOWN_TIMEOUT_MS: u64 = 10_000;

/// The App the dashboard runs on: PostgreSQL, Redis, settings and the scheduler (for queueing only: the dashboard never
/// claims jobs, so the scheduler is not started), no gateway. Call inside a tokio runtime.
pub fn build_app(database_url: &str, redis_url: Option<&str>) -> anyhow::Result<App> {
    let pool = bot_db::connect_lazy(database_url)?;
    Ok(App::builder().config(AppConfig::from_env()).postgres(pool).redis(redis_url).map_err(|e| anyhow::anyhow!(e.0))?.build())
}

/// What the configured dashboard is made of.
pub struct Dashboard {
    pub router: axum::Router,
    pub app: App,
}

/// The dependencies of `createDashboardServer` for an app and a Discord client: the settings bridge, the records
/// bridge over every store (each collection must have one) with the task runner, and botJoinService.
pub fn dashboard_deps(app: &App, discord: Arc<DiscordApi>, tasks: TaskClient) -> Result<DashboardDeps, String> {
    let registry = build_store_registry(app)?;
    let rest: Arc<dyn DashboardDiscord> = discord.clone();
    let records = DashboardRecords::new(registry, Some(rest), Arc::new(tasks));
    let server_discord: Arc<dyn ServerDiscord> = discord;
    Ok(DashboardDeps {
        discord: server_discord,
        settings: Arc::new(DashboardSettings::new(app.clone())),
        records: Some(Arc::new(records)),
        bot_joins: Some(Arc::new(DbBotJoins::new(app.db().clone()))),
        collections: None,
        now: None,
    })
}

/// The configured branch of dashboard.js: the app, the Discord client, the bridges and the router.
pub fn build_dashboard(config: &DashboardConfig) -> anyhow::Result<Dashboard> {
    let app = build_app(&config.database_url, Some(&config.redis_url))?;
    let logger: Arc<dyn DashLogger> = Arc::new(BotLogger);
    let discord = Arc::new(DiscordApi::from_config(config, Some(logger)));
    let deps = dashboard_deps(&app, discord, TaskClient::new(app.clone())).map_err(anyhow::Error::msg)?;
    let router = create_dashboard_router(ServerOptions::new(config.clone(), deps)).map_err(anyhow::Error::msg)?;
    Ok(Dashboard { router, app })
}

/// The DASHBOARD_URL warning of dashboard.js, when it applies.
pub fn default_url_warning(config: &DashboardConfig) -> Option<String> {
    (config.url_is_default && config.host == "0.0.0.0").then(|| {
        format!(
            "Dashboard: DASHBOARD_URL is not set, so saving only works at {}. Browsers that open the dashboard by another address will be able to read but not save changes until DASHBOARD_URL is set to that address.",
            config.url
        )
    })
}

/// The process: configuration, server, signals. Returns the exit code.
pub async fn run() -> i32 {
    let loaded = dash_auth::config::load_dashboard_config_from_env();
    for warning in &loaded.warnings {
        log_warn!(format!("Dashboard: {warning}"));
    }
    let config = loaded.config.clone();

    let (router, app, headers_timeout) = if !loaded.ok {
        if !loaded.missing.is_empty() {
            log_error!(format!("Dashboard not started: missing {}.", loaded.missing.join(", ")));
        }
        for inv in &loaded.invalid {
            log_error!(format!("Dashboard not started: {} {}.", inv.key, inv.reason));
        }
        log_error!(
            "Set these keys (see the \"Web dashboard\" section of .env.example) and restart. Parked: /healthz answers 200, every other path 503."
        );
        let invalid: Vec<String> = loaded.invalid.iter().map(|i| i.key.clone()).collect();
        (create_parked_router(&loaded.missing, &invalid), None, dash_server::serve::PARKED_HEADERS_TIMEOUT_MS)
    } else {
        match build_dashboard(&config) {
            Ok(d) => {
                if let Some(w) = default_url_warning(&config) {
                    log_warn!(w);
                }
                (d.router, Some(d.app), dash_server::serve::HEADERS_TIMEOUT_MS)
            }
            Err(e) => {
                log_error!("Fatal error during startup", bot_util::logger::LogArg::error(e.as_ref() as &dyn std::error::Error));
                return 1;
            }
        }
    };

    let listener = match tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(l) => l,
        Err(e) => {
            log_error!(format!("Dashboard server on {}:{} failed", config.host, config.port), bot_util::logger::LogArg::error(&e));
            return 1;
        }
    };
    if app.is_some() {
        log_success!(format!("Dashboard listening on {}:{} (public URL {}).", config.host, config.port, config.url));
    } else {
        log_warn!(format!("Dashboard parked on {}:{}.", config.host, config.port));
    }

    let stop = Arc::new(Notify::new());
    install_signal_handlers(stop.clone());
    let wait = stop.clone();
    // server.close() + server.closeAllConnections()
    dash_server::serve_listener(listener, router, headers_timeout, async move { wait.notified().await }).await;

    // closeServices: the database pool and Redis (Promise.allSettled, so nothing here fails the shutdown).
    if let Some(app) = app {
        if let Some(r) = app.redis() {
            r.disconnect();
        }
        bot_db::disconnect(app.db()).await;
    }
    0
}

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// `shutdown(signal)`: a second signal exits at once; otherwise the server closes (bounded at 10 s in all).
pub fn shutdown(stop: &Notify, signal: &str) {
    if SHUTTING_DOWN.swap(true, Ordering::SeqCst) {
        log_warn!(format!("{signal} received again — exiting now."));
        std::process::exit(1);
    }
    log_info!(format!("{signal} received — shutting down the dashboard..."));
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(SHUTDOWN_TIMEOUT_MS));
        log_error!(format!("Dashboard shutdown took longer than {} s — forcing exit.", SHUTDOWN_TIMEOUT_MS / 1000));
        std::process::exit(1);
    });
    stop.notify_one();
}

/// SIGTERM and SIGINT (Ctrl+C on Windows).
pub fn install_signal_handlers(stop: Arc<Notify>) {
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
                shutdown(&stop, name);
            }
        }
        #[cfg(not(unix))]
        {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    log_error!("Could not install the signal handlers");
                    return;
                }
                shutdown(&stop, "SIGINT");
            }
        }
    });
}
