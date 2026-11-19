//! DATABASE_URL parsing (Prisma's parameters) and the raw-SQL time helpers of client.js.

use std::path::Path;
use std::time::Duration;

use bot_db::client::{DbConfig, default_connection_limit, parse_database_url_with};
use bot_db::{SQL_NOW, SslMode, sql_utc, sql_utc_literal, sql_utc_param};
use chrono::TimeZone;

fn parse(url: &str) -> DbConfig {
    parse_database_url_with(url, false, Path::new("prisma")).unwrap_or_else(|e| panic!("{url}: {e}"))
}

#[test]
fn env_example_url_parses_with_prisma_defaults() {
    let c = parse("postgresql://postgres:password@localhost:5432/discordbot?schema=public");
    assert_eq!(c.host, "localhost");
    assert_eq!(c.port, 5432);
    assert_eq!(c.username, "postgres");
    assert_eq!(c.database.as_deref(), Some("discordbot"));
    assert_eq!(c.schema, "public");
    assert_eq!(c.max_connections, default_connection_limit());
    assert_eq!(c.acquire_timeout, Some(Duration::from_secs(10)));
    assert_eq!(c.connect_timeout, Some(Duration::from_secs(5)));
    assert_eq!(c.ssl_mode, SslMode::Prefer);
    assert!(!c.pgbouncer);
    assert!(c.dropped.is_empty());
    assert!(!c.log_queries);
    assert_eq!(c.connect_options.get_options(), Some("-c search_path=\"public\""));
}

#[test]
fn prisma_parameters_are_applied_or_dropped() {
    let c = parse(
        "postgres://u:p%40ss%2Fw@db.example.com:6543/app?schema=Bot&connection_limit=7&pool_timeout=0&connect_timeout=12\
         &sslmode=require&sslaccept=strict&pgbouncer=true&socket_timeout=30&sslidentity=id.p12&sslpassword=x\
         &application_name=bot&max_connection_lifetime=60&max_idle_connection_lifetime=20&channel_binding=prefer&sslcert=certs/root.pem",
    );
    assert_eq!(c.host, "db.example.com");
    assert_eq!(c.port, 6543);
    assert_eq!(c.database.as_deref(), Some("app"));
    assert_eq!(c.schema, "Bot");
    assert_eq!(c.max_connections, 7);
    assert_eq!(c.acquire_timeout, None);
    assert_eq!(c.connect_timeout, Some(Duration::from_secs(12)));
    assert_eq!(c.max_lifetime, Some(Duration::from_secs(60)));
    assert_eq!(c.idle_timeout, Some(Duration::from_secs(20)));
    assert_eq!(c.ssl_mode, SslMode::VerifyFull, "sslaccept=strict verifies the certificate");
    assert_eq!(c.ssl_root_cert.as_deref(), Some(Path::new("prisma").join("certs/root.pem").as_path()));
    assert!(c.pgbouncer);
    assert_eq!(c.statement_cache_capacity, Some(0));
    assert_eq!(c.application_name.as_deref(), Some("bot"));
    assert_eq!(c.connect_options.get_application_name(), Some("bot"));
    assert_eq!(c.connect_options.get_options(), Some("-c search_path=\"Bot\""));
    assert_eq!(c.dropped, vec!["socket_timeout=30", "sslidentity=id.p12", "sslpassword=x", "channel_binding=prefer"]);
    // The pool options build without panicking.
    let _ = c.pool_options();
}

#[test]
fn ssl_modes_follow_prisma() {
    assert_eq!(parse("postgresql://u@h/d?sslmode=disable").ssl_mode, SslMode::Disable);
    // Prisma's `require` does not verify the certificate (sslaccept defaults to accept_invalid_certs).
    assert_eq!(parse("postgresql://u@h/d?sslmode=require").ssl_mode, SslMode::Require);
    assert_eq!(parse("postgresql://u@h/d?sslmode=require&sslaccept=accept_invalid_certs").ssl_mode, SslMode::Require);
    assert_eq!(parse("postgresql://u@h/d?sslmode=verify-ca").ssl_mode, SslMode::VerifyCa);
    // Unknown sslaccept: Prisma defaults to accept_invalid_certs.
    let c = parse("postgresql://u@h/d?sslmode=require&sslaccept=weird");
    assert_eq!(c.ssl_mode, SslMode::Require);
    assert_eq!(c.dropped, vec!["sslaccept=weird"]);
}

#[test]
fn odd_urls() {
    // No port, no database, IPv6 host, quoted value as some .env files have it.
    let c = parse("\"postgresql://postgres@[::1]/\"");
    assert_eq!(c.port, 5432);
    assert_eq!(c.host, "::1");
    let c = parse("postgresql://postgres:pw@localhost/discordbot");
    assert_eq!(c.schema, "public");
    // An empty schema keeps the default; an absolute sslcert path is kept as is.
    let c = parse("postgresql://u@h/d?schema=&sslcert=/etc/ssl/root.pem");
    assert_eq!(c.schema, "public");
    assert_eq!(c.ssl_root_cert.as_deref(), Some(Path::new("/etc/ssl/root.pem")));
    // Errors.
    assert!(parse_database_url_with("mysql://u@h/d", false, Path::new("prisma")).is_err());
    assert!(parse_database_url_with("not a url", false, Path::new("prisma")).is_err());
    assert!(parse_database_url_with("postgresql://u@h/d?connection_limit=x", false, Path::new("prisma")).is_err());
    assert!(parse_database_url_with("postgresql://u@h/d?sslmode=sometimes", false, Path::new("prisma")).is_err());
    // Query logging follows PRISMA_LOG_QUERIES=1.
    assert!(parse_database_url_with("postgresql://u@h/d", true, Path::new("prisma")).unwrap().log_queries);
}

#[test]
fn sql_time_helpers_match_client_js() {
    assert_eq!(SQL_NOW, "(now() AT TIME ZONE 'UTC')");
    let d = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap() + chrono::Duration::milliseconds(6);
    assert_eq!(sql_utc(3), "($3::timestamptz AT TIME ZONE 'UTC')");
    assert_eq!(sql_utc_param(&d), "2026-01-02T03:04:05.006Z");
    assert_eq!(sql_utc_literal(&d), "('2026-01-02T03:04:05.006Z'::timestamptz AT TIME ZONE 'UTC')");
}
