//! Shared helpers of the bot-core integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// `TEST_DATABASE_URL`, or `None` after printing the skip line.
pub fn database_url() -> Option<String> {
    match std::env::var("TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            println!("skipped: TEST_DATABASE_URL not set");
            None
        }
    }
}

/// `TEST_REDIS_URL`, or `None` after printing the skip line.
pub fn redis_url() -> Option<String> {
    match std::env::var("TEST_REDIS_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            println!("skipped: TEST_REDIS_URL not set");
            None
        }
    }
}

/// The repository root.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("..")
}

/// Runs `rust/fixtures/gen/bot-core-js.mjs <args>` (the JS bot's own services) and returns its RESULT value.
pub fn run_js(args: &[&str]) -> Value {
    let root = repo_root();
    let out = Command::new("node")
        .current_dir(&root)
        .arg(root.join("rust").join("fixtures").join("gen").join("bot-core-js.mjs"))
        .args(args)
        .output()
        .expect("node runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().rev().find(|l| l.starts_with("RESULT ")).unwrap_or_else(|| {
        panic!("no RESULT from node {args:?}: {stdout}\n{}", String::from_utf8_lossy(&out.stderr))
    });
    serde_json::from_str(&line["RESULT ".len()..]).expect("RESULT is JSON")
}

/// A pool on the test database.
pub async fn pool(url: &str) -> bot_db::PgPool {
    sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(url).await.expect("test database reachable")
}
