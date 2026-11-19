//! Port of `src/database/client.js`: the connection pool built from `DATABASE_URL` exactly as Prisma accepts it, query
//! logging (`PRISMA_LOG_QUERIES=1`), and the raw-SQL time helpers `SQL_NOW` / `sqlUtc`.
//!
//! Prisma-specific query parameters are understood so `.env` keeps working unchanged:
//!
//! | parameter | effect here |
//! |---|---|
//! | `schema` | `search_path` of every connection (default `public`) |
//! | `connection_limit` | pool size (default: CPUs * 2 + 1, like Prisma) |
//! | `pool_timeout` | seconds to wait for a free connection (default 10, 0 = no limit) |
//! | `connect_timeout` | seconds to wait for the first connection in [`connect`] (default 5, 0 = no limit) |
//! | `pgbouncer=true` | no prepared-statement cache |
//! | `statement_cache_size` | prepared-statement cache size |
//! | `sslmode` | `disable` / `prefer` / `require` (also libpq's `allow`, `verify-ca`, `verify-full`) |
//! | `sslaccept=strict` | verify the server certificate and host name (`verify-full`) |
//! | `sslcert` | the server's root certificate (Prisma resolves a relative path against `prisma/`) |
//! | `max_connection_lifetime`, `max_idle_connection_lifetime` | pool connection lifetimes (seconds) |
//! | `application_name`, `options`, `host`, `port`, `user`, `password`, `dbname` | passed to the driver |
//!
//! Everything else (`socket_timeout`, `sslidentity`, `sslpassword`, `channel_binding`, ...) is dropped with a warning
//! instead of failing; [`DbConfig::dropped`] lists what was dropped.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::ConnectOptions;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};

use crate::DbError;
use crate::serde_js::format_js_date;

/// UTC "now" as timestamp WITHOUT time zone: matches Prisma `DateTime` columns whatever the session TimeZone is.
pub const SQL_NOW: &str = "(now() AT TIME ZONE 'UTC')";

/// `sqlUtc(date)` with a bind: the SQL fragment for placeholder `$n`; bind [`sql_utc_param`] of the date there.
/// `($n::timestamptz AT TIME ZONE 'UTC')`.
pub fn sql_utc(n: usize) -> String {
    format!("(${n}::timestamptz AT TIME ZONE 'UTC')")
}

/// The value to bind for [`sql_utc`]: `date.toISOString()`.
pub fn sql_utc_param(date: &DateTime<Utc>) -> String {
    format_js_date(date)
}

/// `sqlUtc(date)` inlined as a literal (safe: the text is generated from the date, never from input).
pub fn sql_utc_literal(date: &DateTime<Utc>) -> String {
    format!("('{}'::timestamptz AT TIME ZONE 'UTC')", format_js_date(date))
}

/// The SSL mode in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn to_pg(self) -> PgSslMode {
        match self {
            SslMode::Disable => PgSslMode::Disable,
            SslMode::Allow => PgSslMode::Allow,
            SslMode::Prefer => PgSslMode::Prefer,
            SslMode::Require => PgSslMode::Require,
            SslMode::VerifyCa => PgSslMode::VerifyCa,
            SslMode::VerifyFull => PgSslMode::VerifyFull,
        }
    }
}

/// Everything parsed from `DATABASE_URL`.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Driver options (host, credentials, database, TLS, search_path, logging).
    pub connect_options: PgConnectOptions,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub database: Option<String>,
    /// `schema=` (default `public`).
    pub schema: String,
    pub max_connections: u32,
    /// `pool_timeout`; `None` waits forever.
    pub acquire_timeout: Option<Duration>,
    /// `connect_timeout`; `None` waits forever.
    pub connect_timeout: Option<Duration>,
    pub max_lifetime: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub ssl_mode: SslMode,
    pub ssl_root_cert: Option<PathBuf>,
    /// Prepared-statement cache size (0 with `pgbouncer=true`).
    pub statement_cache_capacity: Option<usize>,
    pub pgbouncer: bool,
    pub application_name: Option<String>,
    /// `PRISMA_LOG_QUERIES=1`.
    pub log_queries: bool,
    /// Query parameters that have no meaning here and were ignored (`name=value`).
    pub dropped: Vec<String>,
}

/// Prisma's default pool size: `num_cpus * 2 + 1`.
pub fn default_connection_limit() -> u32 {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as u32;
    cpus * 2 + 1
}

fn parse_secs(name: &str, value: &str) -> Result<Option<Duration>, DbError> {
    let n: u64 = value.trim().parse().map_err(|_| DbError::Config(format!("{name} must be a number of seconds, got {value:?}")))?;
    Ok(if n == 0 { None } else { Some(Duration::from_secs(n)) })
}

fn quote_schema(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

/// Parses a `DATABASE_URL` (with `PRISMA_LOG_QUERIES` read from the environment).
pub fn parse_database_url(url: &str) -> Result<DbConfig, DbError> {
    parse_database_url_with(url, std::env::var("PRISMA_LOG_QUERIES").is_ok_and(|v| v == "1"), Path::new("prisma"))
}

/// Parses a `DATABASE_URL`. `schema_dir` is the directory Prisma resolves a relative `sslcert` path against
/// (`prisma/`, where `schema.prisma` lives).
pub fn parse_database_url_with(url: &str, log_queries: bool, schema_dir: &Path) -> Result<DbConfig, DbError> {
    let trimmed = url.trim().trim_matches('"').trim_matches('\'');
    let mut parsed = url::Url::parse(trimmed).map_err(|e| DbError::Config(format!("not a URL ({e})")))?;
    match parsed.scheme() {
        "postgres" | "postgresql" => {}
        other => return Err(DbError::Config(format!("unsupported scheme {other:?} (expected postgresql://)"))),
    }

    let pairs: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut keep: Vec<(String, String)> = Vec::new();
    let mut schema = "public".to_string();
    let mut max_connections = default_connection_limit();
    let mut acquire_timeout = Some(Duration::from_secs(10));
    let mut connect_timeout = Some(Duration::from_secs(5));
    let mut max_lifetime = None;
    let mut idle_timeout = None;
    let mut ssl_mode = SslMode::Prefer;
    let mut ssl_strict = false;
    let mut ssl_root_cert: Option<PathBuf> = None;
    let mut statement_cache_capacity: Option<usize> = None;
    let mut pgbouncer = false;
    let mut application_name = None;
    let mut dropped = Vec::new();

    for (k, v) in pairs {
        match k.as_str() {
            "schema" => {
                if !v.is_empty() {
                    schema = v;
                }
            }
            "connection_limit" => {
                let n: u32 = v.trim().parse().map_err(|_| DbError::Config(format!("connection_limit must be a number, got {v:?}")))?;
                max_connections = n.max(1);
            }
            "pool_timeout" => acquire_timeout = parse_secs("pool_timeout", &v)?,
            "connect_timeout" => connect_timeout = parse_secs("connect_timeout", &v)?,
            "max_connection_lifetime" => max_lifetime = parse_secs("max_connection_lifetime", &v)?,
            "max_idle_connection_lifetime" => idle_timeout = parse_secs("max_idle_connection_lifetime", &v)?,
            "pgbouncer" => pgbouncer = v == "true" || v == "1",
            "statement_cache_size" => {
                statement_cache_capacity =
                    Some(v.trim().parse().map_err(|_| DbError::Config(format!("statement_cache_size must be a number, got {v:?}")))?);
            }
            "sslmode" => {
                ssl_mode = match v.as_str() {
                    "disable" => SslMode::Disable,
                    "allow" => SslMode::Allow,
                    "prefer" => SslMode::Prefer,
                    "require" => SslMode::Require,
                    "verify-ca" => SslMode::VerifyCa,
                    "verify-full" => SslMode::VerifyFull,
                    other => return Err(DbError::Config(format!("unknown sslmode {other:?}"))),
                }
            }
            "sslaccept" => match v.as_str() {
                "strict" => ssl_strict = true,
                "accept_invalid_certs" => ssl_strict = false,
                // Prisma: "Unsupported SSL accept mode, defaulting to `accept_invalid_certs`".
                _ => {
                    ssl_strict = false;
                    dropped.push(format!("{k}={v}"));
                }
            },
            "sslcert" | "sslrootcert" => {
                let p = PathBuf::from(&v);
                ssl_root_cert = Some(if p.is_relative() && k == "sslcert" { schema_dir.join(p) } else { p });
            }
            "application_name" => {
                application_name = Some(v.clone());
                keep.push((k, v));
            }
            "host" | "hostaddr" | "port" | "dbname" | "user" | "password" | "options" => keep.push((k, v)),
            _ => dropped.push(format!("{k}={v}")),
        }
    }

    // Hand the driver only what it understands.
    parsed.set_query(None);
    if !keep.is_empty() {
        let mut q = parsed.query_pairs_mut();
        for (k, v) in &keep {
            q.append_pair(k, v);
        }
    }
    let mut opts = PgConnectOptions::from_str(parsed.as_str()).map_err(|e| DbError::Config(e.to_string()))?;
    // The driver keeps the brackets of an IPv6 host ("[::1]"), which a socket address cannot use.
    if let Some(url::Host::Ipv6(ip)) = parsed.host()
        && !keep.iter().any(|(k, _)| k == "host")
    {
        opts = opts.host(&ip.to_string());
    }

    if ssl_strict && matches!(ssl_mode, SslMode::Require | SslMode::Prefer | SslMode::Allow) {
        ssl_mode = SslMode::VerifyFull;
    }
    opts = opts.ssl_mode(ssl_mode.to_pg());
    if let Some(cert) = &ssl_root_cert {
        opts = opts.ssl_root_cert(cert);
    }
    if pgbouncer {
        statement_cache_capacity = Some(0);
    }
    if let Some(n) = statement_cache_capacity {
        opts = opts.statement_cache_capacity(n);
    }
    opts = opts.options([("search_path", quote_schema(&schema))]);
    opts = if log_queries {
        opts.log_statements(log::LevelFilter::Info).log_slow_statements(log::LevelFilter::Warn, Duration::from_secs(1))
    } else {
        // Prisma's default log level is warn/error: no statement logging at all.
        opts.disable_statement_logging()
    };

    for d in &dropped {
        tracing::warn!(parameter = %d, "DATABASE_URL parameter has no meaning for the Rust driver and was ignored");
    }

    Ok(DbConfig {
        host: opts.get_host().to_string(),
        port: opts.get_port(),
        username: opts.get_username().to_string(),
        database: opts.get_database().map(str::to_string),
        connect_options: opts,
        schema,
        max_connections,
        acquire_timeout,
        connect_timeout,
        max_lifetime,
        idle_timeout,
        ssl_mode,
        ssl_root_cert,
        statement_cache_capacity,
        pgbouncer,
        application_name,
        log_queries,
        dropped,
    })
}

impl DbConfig {
    /// Pool options from the parsed parameters.
    pub fn pool_options(&self) -> PgPoolOptions {
        let mut p = PgPoolOptions::new()
            .max_connections(self.max_connections)
            // A very long wait stands in for Prisma's `pool_timeout=0` (no limit).
            .acquire_timeout(self.acquire_timeout.unwrap_or(Duration::from_secs(24 * 3600)));
        if self.max_lifetime.is_some() {
            p = p.max_lifetime(self.max_lifetime);
        }
        if self.idle_timeout.is_some() {
            p = p.idle_timeout(self.idle_timeout);
        }
        p
    }
}

/// Connects eagerly (opens one connection to check the URL), bounded by `connect_timeout`.
pub async fn connect(url: &str) -> Result<PgPool, DbError> {
    connect_with(&parse_database_url(url)?).await
}

/// [`connect`] with an already parsed config.
pub async fn connect_with(cfg: &DbConfig) -> Result<PgPool, DbError> {
    let fut = cfg.pool_options().connect_with(cfg.connect_options.clone());
    match cfg.connect_timeout {
        Some(t) => tokio::time::timeout(t, fut)
            .await
            .map_err(|_| DbError::Timeout(format!("could not connect to the database within {} s", t.as_secs())))?
            .map_err(DbError::from),
        None => fut.await.map_err(DbError::from),
    }
}

/// A pool that connects on first use, like the Prisma client.
pub fn connect_lazy(url: &str) -> Result<PgPool, DbError> {
    Ok(connect_lazy_with(&parse_database_url(url)?))
}

/// [`connect_lazy`] with an already parsed config.
pub fn connect_lazy_with(cfg: &DbConfig) -> PgPool {
    cfg.pool_options().connect_lazy_with(cfg.connect_options.clone())
}

/// A lazy pool from the `DATABASE_URL` environment variable.
pub fn connect_lazy_from_env() -> Result<PgPool, DbError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| DbError::Config("DATABASE_URL is not set".into()))?;
    connect_lazy(&url)
}

/// Closes the pool (`disconnect()` in client.js). Shutdown is coordinated by the binary's signal handler.
pub async fn disconnect(pool: &PgPool) {
    pool.close().await;
}
