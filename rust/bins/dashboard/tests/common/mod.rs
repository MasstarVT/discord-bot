//! Shared setup of the dashboard tests: the dashboard's own App (PostgreSQL, Redis, settings, scheduler; no gateway,
//! no claim loop), a bot App on the same database and Redis running feat-dashboard-tasks with a fake guild, and the
//! JS scripts.
#![allow(dead_code)]

use std::time::Duration;

use bot_core::testing::{DbTestApp, FakeGuild, FakeRest, TEST_BOT_ID, database_url, redis_url, run_node};
use bot_core::{App, Registry};
use serde_json::Value;
use serenity::all::Permissions;

/// The JS side of these tests.
pub const JS: &str = "rust/fixtures/gen/dashboard-js.mjs";
/// The JS dashboard task runner and the JS bot's task handler (feat-dashboard-tasks' script).
pub const TASKS_JS: &str = "rust/fixtures/gen/feat-dashboard-tasks-js.mjs";

pub const OWNER: &str = "900000000000000099";
pub const BOB: &str = "900000000000000002";
pub const ALICE: &str = "900000000000000003";
pub const ROLE_ADMIN: &str = "910000000000000001";
pub const ROLE_MOD: &str = "910000000000000002";
pub const ROLE_BOT: &str = "910000000000000003";
pub const ROLE_PLAIN: &str = "910000000000000004";
pub const CH_TEXT: &str = "920000000000000001";
pub const CH_LOG: &str = "920000000000000002";

/// The DB tests share the job table and the bot's claim loop: one at a time.
pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Both test variables, or `None` (the test returns early, printing why).
pub fn urls() -> Option<(String, String)> {
    match (database_url(), redis_url()) {
        (Some(d), Some(r)) => Some((d, r)),
        _ => {
            eprintln!("skipped: TEST_DATABASE_URL or TEST_REDIS_URL not set");
            None
        }
    }
}

/// The dashboard process's App, as dashboard.js builds it (bins/dashboard `index::build_app`), with Redis connected.
pub async fn dashboard_app(db: &str, redis: &str) -> App {
    let app = dashboard::index::build_app(db, Some(redis)).expect("the dashboard app builds");
    assert!(app.redis().expect("redis").wait_ready(5_000).await, "the test Redis is reachable");
    app
}

/// The bot's fake guild: Bob is an admin (Manage Server), Alice a moderator.
pub fn fake_guild(g: &str) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .role(ROLE_ADMIN, 3, Permissions::MANAGE_GUILD)
        .role(ROLE_MOD, 2, Permissions::KICK_MEMBERS)
        .role(ROLE_BOT, 5, Permissions::ADMINISTRATOR)
        .member(TEST_BOT_ID, &[ROLE_BOT])
        .member(BOB, &[ROLE_ADMIN])
        .member(ALICE, &[ROLE_MOD])
        .channel(CH_TEXT, 0, "general", &[])
}

/// The bot process: an App on the same database and Redis with feat-dashboard-tasks installed and Discord faked.
pub struct Bot {
    pub t: DbTestApp,
    pub rest: FakeRest,
}

impl Bot {
    pub async fn new(db: &str, redis: &str, guilds: &[&str]) -> Self {
        let cfg = bot_core::AppConfig { bot_owner_id: Some(OWNER.into()), ..Default::default() };
        let t = DbTestApp::build(db, Some(redis), cfg).await.with_guilds(guilds.iter().map(|g| g.to_string()));
        let rest = FakeRest::new();
        let jsons: Vec<Value> = guilds.iter().map(|g| fake_guild(g).json()).collect();
        t.app.set_discord(rest.discord(&jsons));
        let mut reg = Registry::new();
        feat_dashboard_tasks::register(&mut reg);
        reg.install(&t.app);
        Self { t, rest }
    }

    /// One claim round of the bot's interactive loop (dashboard tasks); the number of jobs run.
    pub async fn tick(&self) -> usize {
        self.t.app.scheduler().tick(&self.t.app, true).await
    }

    /// Ticks until nothing is left to claim.
    pub async fn drain(&self) -> usize {
        let mut n = 0;
        loop {
            let k = self.tick().await;
            if k == 0 {
                return n;
            }
            n += k;
        }
    }

    /// Keeps ticking in the background (the bot's 5 s poll, faster) until the handle is aborted.
    pub fn spawn_loop(&self) -> tokio::task::JoinHandle<()> {
        let app = self.t.app.clone();
        tokio::spawn(async move {
            loop {
                app.scheduler().tick(&app, true).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    }

    pub async fn reset(&self, guilds: &[&str]) {
        self.t.reset_guilds(guilds).await;
    }
}

/// `run_node` that fails the test on an `{ error }` answer.
pub fn node(script: &str, args: &[&str]) -> Value {
    let v = run_node(script, args);
    assert!(!v.get("error").is_some_and(Value::is_string), "node {script} {args:?}: {v}");
    v
}

/// [`node`] off the async runtime (for a JS step that must run while Rust code polls).
pub async fn node_async(script: &'static str, args: Vec<String>) -> Value {
    tokio::task::spawn_blocking(move || {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        node(script, &a)
    })
    .await
    .expect("node task")
}

/// Replaces every occurrence of each `(from, to)` string anywhere in `v` (guild ids, generated ids).
pub fn replace_strings(v: &Value, pairs: &[(String, String)]) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.clone();
            for (from, to) in pairs {
                if !from.is_empty() {
                    out = out.replace(from.as_str(), to.as_str());
                }
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| replace_strings(x, pairs)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), replace_strings(x, pairs))).collect()),
        other => other.clone(),
    }
}

/// Removes these keys at every depth.
pub fn drop_keys(v: &Value, keys: &[&str]) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(|x| drop_keys(x, keys)).collect()),
        Value::Object(m) => Value::Object(m.iter().filter(|(k, _)| !keys.contains(&k.as_str())).map(|(k, x)| (k.clone(), drop_keys(x, keys))).collect()),
        other => other.clone(),
    }
}

/// Asserts two JSON values are equal, printing the first difference.
pub fn assert_json_eq(what: &str, got: &Value, want: &Value) {
    if let Some(d) = bot_core::testing::json_diff(got, want, false) {
        panic!("{what}: {d}\n got: {got}\nwant: {want}");
    }
}
