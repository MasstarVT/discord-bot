//! Offline test support for bot-core and every feature crate.
//!
//! * [`TestApp`]: an [`App`] with the in-memory cache, settings store and job store, a [`ManualClock`], a
//!   [`RecordingBackend`] for interaction responses and a fixed guild list. No Discord, PostgreSQL or Redis.
//! * [`FakeInteraction`]: builds serenity interactions (slash commands, autocompletes, buttons, selects, modals) from
//!   the JSON Discord sends, for driving the router or a handler.
//! * [`DbTestApp`]: an [`App`] on the test PostgreSQL database (`TEST_DATABASE_URL`) and optionally the test Redis
//!   (`TEST_REDIS_URL`), for SQL and cross-language tests; [`run_node`] runs a JS script of `rust/fixtures/gen/`.
//! * [`assert_commands_match_golden`] and [`assert_plugin_matches_golden`]: the checks against the JS bot's
//!   `rust/fixtures/golden/commands.json` and `plugins.json` that every feature crate runs;
//!   [`assert_registry_matches_golden`] checks a whole bot.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use serenity::all::{Interaction, Permissions};

use crate::cache::MemoryBackend;
use crate::clock::ManualClock;
use crate::context::{App, AppConfig};
use crate::discord::StaticGuilds;
use crate::interaction::RecordingBackend;
use crate::registry::Registry;
use crate::scheduler::MemoryJobStore;
use crate::settings::MemorySettingsStore;

pub mod fake_rest;
pub use fake_rest::{FakeRest, RestCall};

/// An offline app and handles to its in-memory parts. Derefs to [`App`].
pub struct TestApp {
    pub app: App,
    pub clock: ManualClock,
    pub responses: Arc<RecordingBackend>,
    pub cache_backend: Arc<MemoryBackend>,
    pub settings_store: Arc<MemorySettingsStore>,
    pub job_store: Arc<MemoryJobStore>,
}

impl std::ops::Deref for TestApp {
    type Target = App;
    fn deref(&self) -> &App {
        &self.app
    }
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TestApp {
    /// A new offline app (no guilds available, default config).
    pub fn new() -> Self {
        Self::with_config(AppConfig::default())
    }

    /// A new offline app with this config (`bot_owner_id`, ...).
    pub fn with_config(config: AppConfig) -> Self {
        let clock = ManualClock::starting_now();
        let shared: crate::clock::SharedClock = Arc::new(clock.clone());
        let cache_backend = Arc::new(MemoryBackend::new(shared.clone()));
        let settings_store = Arc::new(MemorySettingsStore::new());
        let job_store = Arc::new(MemoryJobStore::new(shared.clone()));
        let app = App::builder()
            .config(config)
            .clock(shared)
            .cache_backend(cache_backend.clone())
            .settings_store(settings_store.clone())
            .job_store(job_store.clone())
            .build();
        let responses = Arc::new(RecordingBackend::new());
        app.set_interaction_backend(responses.clone());
        Self { app, clock, responses, cache_backend, settings_store, job_store }
    }

    /// Makes these guilds available (cached) in this process.
    pub fn with_guilds<S: Into<String>>(self, ids: impl IntoIterator<Item = S>) -> Self {
        self.app.set_guild_directory(Arc::new(StaticGuilds::available(ids)));
        self
    }

    /// Sets the guild directory.
    pub fn set_guilds(&self, guilds: StaticGuilds) {
        self.app.set_guild_directory(Arc::new(guilds));
    }

    /// Installs a registry (as the bot binary does at boot).
    pub fn install(&self, registry: Registry) -> crate::registry::BootReport {
        registry.install(&self.app)
    }
}

/// An interaction as Discord sends it, built field by field; [`FakeInteraction::build`] deserializes it into the
/// serenity type the router takes.
#[derive(Debug, Clone)]
pub struct FakeInteraction {
    json: Value,
    options: Vec<Value>,
    path: Vec<(String, u8)>,
    /// The GUILD_CREATE JSON that USER / ROLE / CHANNEL / MENTIONABLE options are resolved from.
    resolve_guild: Option<Value>,
    /// ROLE options get a default resolved role even without [`FakeInteraction::resolve_from`].
    default_roles: bool,
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A snowflake created now, so `ackBudgetMs` sees a fresh interaction.
fn fresh_snowflake() -> u64 {
    let ms = chrono::Utc::now().timestamp_millis() as u64 - 1_420_070_400_000;
    (ms << 22) | (NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0x3f_ffff)
}

fn base(kind: u8, data: Value) -> Value {
    let id = fresh_snowflake();
    json!({
        "id": id.to_string(),
        "application_id": "100000000000000001",
        "type": kind,
        "data": data,
        "channel_id": "100000000000000003",
        // One token per interaction, so a RecordingBackend can tell one click's calls from another's.
        "token": format!("test-token-{id}"),
        "version": 1,
        "locale": "en-US",
        "user": { "id": "100000000000000010", "username": "member", "discriminator": "0", "avatar": null },
        "entitlements": [],
        "attachment_size_limit": 26214400,
    })
}

/// A bot-authored message as Discord sends it (in channel `100000000000000003`), e.g. a modal's source message.
pub fn message_json(id: &str) -> Value {
    json!({
        "id": id,
        "channel_id": "100000000000000003",
        "author": { "id": "100000000000000002", "username": "bot", "discriminator": "0", "avatar": null, "bot": true },
        "content": "",
        "timestamp": now_timestamp(),
        "edited_timestamp": null,
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "attachments": [],
        "embeds": [],
        "pinned": false,
        "type": 0,
        "components": [],
    })
}

/// Now, as Discord writes a message timestamp.
fn now_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

/// The id string of an option value (a snowflake as a string or a number).
fn id_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string()))
}

fn default_user_json(id: &str) -> Value {
    json!({ "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "global_name": null })
}

fn default_role_json(id: &str) -> Value {
    json!({
        "id": id, "name": format!("role{id}"), "color": 0, "hoist": false, "position": 1, "permissions": "0",
        "managed": false, "mentionable": false, "flags": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    })
}

/// The resolved partial member Discord sends (no `user`, `deaf` / `mute` left out).
fn partial_member_json(member: Option<&Value>) -> Value {
    let mut m = json!({ "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "nick": null, "avatar": null, "premium_since": null, "flags": 0, "pending": false, "permissions": "0" });
    if let Some(src) = member.and_then(Value::as_object) {
        for (k, v) in src {
            if !matches!(k.as_str(), "user" | "deaf" | "mute") {
                m[k] = v.clone();
            }
        }
    }
    m
}

/// Fills `data.resolved` as Discord does. USER (6) options always (from `guild` when it has the member, else a default
/// user, plus a member in a guild); CHANNEL (7), ROLE (8) and MENTIONABLE (9) options only when a `guild` (a
/// GUILD_CREATE JSON) is given, from it or with defaults. An id already present in any resolved map (written by hand
/// through [`FakeInteraction::json_mut`]) is left alone.
fn fill_resolved(json: &mut Value, options: &[Value], guild: Option<&Value>, default_roles: bool) {
    let leaf: Vec<(u64, String)> = options
        .iter()
        .filter_map(|o| Some((o.get("type")?.as_u64()?, id_of(o.get("value")?)?)))
        .filter(|(k, _)| matches!(k, 6..=9))
        .collect();
    if leaf.is_empty() {
        return;
    }
    let in_guild = json.get("guild_id").is_some_and(|g| !g.is_null());
    let find = |list: &str, id: &str| -> Option<Value> {
        guild?.get(list)?.as_array()?.iter().find(|x| x.get("id").and_then(id_of).as_deref() == Some(id)).cloned()
    };
    let find_member = |id: &str| -> Option<Value> {
        guild?.get("members")?.as_array()?.iter().find(|m| m["user"].get("id").and_then(id_of).as_deref() == Some(id)).cloned()
    };
    if !json["data"].get("resolved").is_some_and(Value::is_object) {
        json["data"]["resolved"] = json!({});
    }
    let resolved = &mut json["data"]["resolved"];
    let known = |r: &Value, id: &str| ["users", "members", "roles", "channels"].iter().any(|k| r.get(*k).and_then(|m| m.get(id)).is_some());
    for (kind, id) in leaf {
        if known(resolved, &id) {
            continue;
        }
        // Without a guild to resolve from, only users get defaults: a made-up role or channel (name, type) would
        // shadow what the serenity cache says about it.
        if guild.is_none() && kind != 6 && !(default_roles && kind == 8) {
            continue;
        }
        let as_role = kind == 8 || (kind == 9 && find("roles", &id).is_some());
        if as_role {
            let role = find("roles", &id).unwrap_or_else(|| default_role_json(&id));
            resolved["roles"][id.as_str()] = role;
        } else if kind == 7 {
            let ch = find("channels", &id).or_else(|| find("threads", &id));
            let mut c = json!({ "id": id, "name": format!("channel{id}"), "type": 0, "permissions": "0" });
            if let Some(src) = ch {
                for k in ["name", "type", "parent_id", "thread_metadata", "nsfw"] {
                    if let Some(v) = src.get(k) {
                        c[k] = v.clone();
                    }
                }
            }
            resolved["channels"][id.as_str()] = c;
        } else {
            let member = find_member(&id);
            let user = member.as_ref().map(|m| m["user"].clone()).unwrap_or_else(|| default_user_json(&id));
            resolved["users"][id.as_str()] = user;
            // A guild member only when the interaction is in a guild and the user is a member (or no guild was given).
            if in_guild && (member.is_some() || guild.is_none()) {
                resolved["members"][id.as_str()] = partial_member_json(member.as_ref());
            }
        }
    }
}

impl FakeInteraction {
    /// `/name` (type 2).
    pub fn slash(name: &str) -> Self {
        Self { json: base(2, json!({ "id": "100000000000000004", "name": name, "type": 1 })), options: Vec::new(), path: Vec::new(), resolve_guild: None, default_roles: false }
    }

    /// The autocomplete of `/name` (type 4).
    pub fn autocomplete(name: &str) -> Self {
        Self { json: base(4, json!({ "id": "100000000000000004", "name": name, "type": 1 })), options: Vec::new(), path: Vec::new(), resolve_guild: None, default_roles: false }
    }

    /// A button click (type 3) on message `100000000000000005`.
    pub fn button(custom_id: &str) -> Self {
        let mut j = base(3, json!({ "custom_id": custom_id, "component_type": 2 }));
        j["message"] = message_json("100000000000000005");
        Self { json: j, options: Vec::new(), path: Vec::new(), resolve_guild: None, default_roles: false }
    }

    /// A string select (type 3) with `values`.
    pub fn select(custom_id: &str, values: &[&str]) -> Self {
        let mut j = base(3, json!({ "custom_id": custom_id, "component_type": 3, "values": values }));
        j["message"] = message_json("100000000000000005");
        Self { json: j, options: Vec::new(), path: Vec::new(), resolve_guild: None, default_roles: false }
    }

    /// A user select (type 3, component 5) with user ids.
    pub fn user_select(custom_id: &str, values: &[&str]) -> Self {
        let mut j = base(3, json!({ "custom_id": custom_id, "component_type": 5, "values": values, "resolved": {} }));
        j["message"] = message_json("100000000000000005");
        Self { json: j, options: Vec::new(), path: Vec::new(), resolve_guild: None, default_roles: false }
    }

    /// A modal submit (type 5) with text inputs `(id, value)`.
    pub fn modal(custom_id: &str, fields: &[(&str, &str)]) -> Self {
        let rows: Vec<Value> = fields
            .iter()
            .map(|(id, v)| json!({ "type": 1, "components": [{ "type": 4, "custom_id": id, "value": v }] }))
            .collect();
        Self { json: base(5, json!({ "custom_id": custom_id, "components": rows })), options: Vec::new(), path: Vec::new(), resolve_guild: None, default_roles: false }
    }

    /// In guild `id`, by a member with no roles and no permissions.
    pub fn guild(mut self, id: &str) -> Self {
        self.json["guild_id"] = json!(id);
        let user = self.json["user"].clone();
        self.json["member"] = json!({ "user": user, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "permissions": "0" });
        self.json.as_object_mut().map(|o| o.remove("user"));
        self
    }

    /// The invoking user's id.
    pub fn user(mut self, id: &str) -> Self {
        if self.json.get("member").is_some() {
            self.json["member"]["user"]["id"] = json!(id);
        } else {
            self.json["user"]["id"] = json!(id);
        }
        self
    }

    /// The channel the interaction comes from (default `100000000000000003`).
    pub fn channel(mut self, id: &str) -> Self {
        self.json["channel_id"] = json!(id);
        if self.json.get("message").is_some() {
            self.json["message"]["channel_id"] = json!(id);
        }
        self
    }

    /// The id of the message a component is on (default `100000000000000005`): match it to the message a handler
    /// collects on.
    pub fn message_id(mut self, id: &str) -> Self {
        if self.json.get("message").is_some() {
            self.json["message"]["id"] = json!(id);
        }
        self
    }

    /// The member's permissions (what Discord sends with the interaction; used when the guild is not in a serenity
    /// cache).
    pub fn permissions(mut self, permissions: Permissions) -> Self {
        if self.json.get("member").is_some() {
            self.json["member"]["permissions"] = json!(permissions.bits().to_string());
        }
        self
    }

    /// The member's role ids.
    pub fn roles(mut self, roles: &[&str]) -> Self {
        if self.json.get("member").is_some() {
            self.json["member"]["roles"] = json!(roles);
        }
        self
    }

    /// Runs under subcommand `name`.
    pub fn subcommand(mut self, name: &str) -> Self {
        self.path = vec![(name.to_string(), 1)];
        self
    }

    /// Runs under `group name`.
    pub fn group(mut self, group: &str, sub: &str) -> Self {
        self.path = vec![(group.to_string(), 2), (sub.to_string(), 1)];
        self
    }

    /// A leaf option (`kind` from [`crate::command::option_type`]).
    pub fn option(mut self, name: &str, kind: u8, value: Value) -> Self {
        self.options.push(json!({ "name": name, "type": kind, "value": value }));
        self
    }

    /// The focused option of an autocomplete.
    pub fn focused(mut self, name: &str, value: &str) -> Self {
        self.options.push(json!({ "name": name, "type": 3, "value": value, "focused": true }));
        self
    }

    /// Resolves USER / ROLE / CHANNEL / MENTIONABLE options from this fake guild (names, permissions, member roles),
    /// as Discord fills `data.resolved` from the guild. Without it, [`Self::build`] fills only USER options (with a
    /// default user, and a member in a guild).
    pub fn resolve_from(mut self, guild: &FakeGuild) -> Self {
        self.resolve_guild = Some(guild.json());
        self
    }

    /// ROLE options get a default resolved role (`role{id}`, position 1, no permissions) even without
    /// [`Self::resolve_from`], as Discord always sends the resolved role. Off by default: a made-up role would shadow
    /// what the serenity cache says about it.
    pub fn resolve_roles_by_default(mut self) -> Self {
        self.default_roles = true;
        self
    }

    /// The timestamp of the message a component is on (default: now), e.g. `"2026-01-01T00:00:00.000000+00:00"`.
    pub fn message_timestamp(mut self, timestamp: &str) -> Self {
        if self.json.get("message").is_some() {
            self.json["message"]["timestamp"] = json!(timestamp);
        }
        self
    }

    /// The raw JSON (edit anything else).
    pub fn json_mut(&mut self) -> &mut Value {
        &mut self.json
    }

    /// The serenity interaction.
    pub fn build(self) -> Interaction {
        let mut json = self.json;
        if json.get("type").and_then(Value::as_u64).is_some_and(|t| t == 2 || t == 4) {
            let mut opts = Value::Array(self.options);
            if let Value::Array(leaf) = &opts {
                fill_resolved(&mut json, leaf, self.resolve_guild.as_ref(), self.default_roles);
            }
            for (name, kind) in self.path.iter().rev() {
                opts = json!([{ "name": name, "type": kind, "options": opts }]);
            }
            json["data"]["options"] = opts;
        }
        serde_json::from_value(json).unwrap_or_else(|e| panic!("FakeInteraction does not deserialize: {e}"))
    }
}

// ── PostgreSQL, Redis and the JS side ─────────────────────────────────────────

/// `TEST_DATABASE_URL`, or `None` after printing `skipped: TEST_DATABASE_URL not set` (a test then returns early).
pub fn database_url() -> Option<String> {
    env_or_skip("TEST_DATABASE_URL")
}

/// `TEST_REDIS_URL`, or `None` after printing `skipped: TEST_REDIS_URL not set`.
pub fn redis_url() -> Option<String> {
    env_or_skip("TEST_REDIS_URL")
}

fn env_or_skip(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            println!("skipped: {key} not set");
            None
        }
    }
}

/// A pool on the test database (eager: fails the test when the server is down).
pub async fn connect_test_pool(url: &str) -> bot_db::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await
        .unwrap_or_else(|e| panic!("the test database is not reachable: {e}"))
}

/// The repository root.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("..")
}

/// Runs a Node script (path relative to the repository root, e.g. `rust/fixtures/gen/feat-x-seed.mjs`) with `args`,
/// from the repository root, with the test environment variables inherited, and returns the JSON after the last
/// stdout line starting with `RESULT ` (the protocol of `rust/fixtures/gen/bot-core-js.mjs`; the JS logger may print
/// other lines). Panics with the output when there is none.
pub fn run_node(script: &str, args: &[&str]) -> Value {
    let root = repo_root();
    let out = std::process::Command::new("node").current_dir(&root).arg(root.join(script)).args(args).output().expect("node runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().rev().find(|l| l.starts_with("RESULT ")).unwrap_or_else(|| {
        panic!("no RESULT line from node {script} {args:?}:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr))
    });
    serde_json::from_str(&line["RESULT ".len()..]).expect("the RESULT line is JSON")
}

/// An app on the test database: the PostgreSQL settings and job stores, the system clock (SQL time and app time must
/// agree for due-time claims), an in-memory cache or the test Redis, a [`RecordingBackend`] for interactions.
/// Derefs to [`App`].
pub struct DbTestApp {
    pub app: App,
    pub pool: bot_db::PgPool,
    pub responses: Arc<RecordingBackend>,
    /// The Redis backend when built with [`DbTestApp::with_redis`].
    pub redis: Option<crate::cache::RedisBackend>,
}

impl std::ops::Deref for DbTestApp {
    type Target = App;
    fn deref(&self) -> &App {
        &self.app
    }
}

impl DbTestApp {
    /// PostgreSQL at `database_url`, the in-memory cache.
    pub async fn new(database_url: &str) -> Self {
        Self::build(database_url, None, AppConfig::default()).await
    }

    /// PostgreSQL and Redis (waits up to 5 s for the connection), for cache cross-language tests.
    pub async fn with_redis(database_url: &str, redis_url: &str) -> Self {
        Self::build(database_url, Some(redis_url), AppConfig::default()).await
    }

    /// Any combination, with a config.
    pub async fn build(database_url: &str, redis_url: Option<&str>, config: AppConfig) -> Self {
        let pool = connect_test_pool(database_url).await;
        let mut builder = App::builder().config(config).postgres(pool.clone());
        let mut redis = None;
        if let Some(url) = redis_url {
            let backend = crate::cache::RedisBackend::connect(crate::cache::RedisOptions::new(Some(url))).expect("TEST_REDIS_URL parses");
            assert!(backend.wait_ready(5_000).await, "the test Redis is not reachable");
            builder = builder.cache_backend(Arc::new(backend.clone()));
            redis = Some(backend);
        }
        let app = builder.build();
        let responses = Arc::new(RecordingBackend::new());
        app.set_interaction_backend(responses.clone());
        Self { app, pool, responses, redis }
    }

    /// Makes these guilds available (cached) in this process.
    pub fn with_guilds<S: Into<String>>(self, ids: impl IntoIterator<Item = S>) -> Self {
        self.app.set_guild_directory(Arc::new(StaticGuilds::available(ids)));
        self
    }

    /// Deletes these guilds' settings rows (and, by the cascades, every per-guild row) and forgets them in the
    /// settings service: call it before and after a test.
    pub async fn reset_guilds(&self, guild_ids: &[&str]) {
        let ids: Vec<String> = guild_ids.iter().map(|s| s.to_string()).collect();
        sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = ANY($1)"#).bind(&ids).execute(&self.pool).await.expect("cleanup runs");
        for id in &ids {
            self.app.settings().invalidate_settings(id).await;
        }
    }
}

// ── Golden files ──────────────────────────────────────────────────────────────

/// `rust/fixtures/golden` (resolved from bot-core's manifest directory, whatever crate calls it).
pub fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join("golden")
}

/// A golden JSON file (key order preserved).
pub fn load_golden(name: &str) -> Value {
    let path = golden_dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

/// The first difference between two JSON values (path, left, right), comparing key order too when `ordered`.
pub fn json_diff(a: &Value, b: &Value, ordered: bool) -> Option<String> {
    fn go(a: &Value, b: &Value, path: &str, ordered: bool) -> Option<String> {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                if ordered {
                    let kx: Vec<&String> = x.keys().collect();
                    let ky: Vec<&String> = y.keys().collect();
                    if kx != ky {
                        return Some(format!("{path}: keys {kx:?} vs {ky:?}"));
                    }
                }
                for (k, v) in x {
                    match y.get(k) {
                        Some(w) => {
                            if let Some(d) = go(v, w, &format!("{path}.{k}"), ordered) {
                                return Some(d);
                            }
                        }
                        None => return Some(format!("{path}.{k}: present only on the left ({v})")),
                    }
                }
                y.keys().find(|k| !x.contains_key(*k)).map(|k| format!("{path}.{k}: present only on the right ({})", y[k]))
            }
            (Value::Array(x), Value::Array(y)) => {
                if x.len() != y.len() {
                    return Some(format!("{path}: {} vs {} items", x.len(), y.len()));
                }
                x.iter().zip(y).enumerate().find_map(|(i, (v, w))| go(v, w, &format!("{path}[{i}]"), ordered))
            }
            (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => {
                if ordered && x.to_string() != y.to_string() { Some(format!("{path}: {x} vs {y} (formatting)")) } else { None }
            }
            _ if a == b => None,
            _ => Some(format!("{path}: {a} vs {b}")),
        }
    }
    go(a, b, "$", ordered)
}

/// Checks that the registry declares exactly the commands `names`, and that each one's Discord payload is
/// byte-identical to the JS payload in `commands.json` (the deploy hash depends on it) and its routing metadata
/// (`category`, `module`, `gated`, `permissionLevel`, `subcommandPermissions`, `allowInDMs`, `hasAutocomplete`)
/// matches. Also runs the command validation. Panics with the first difference.
pub fn assert_commands_match_golden(registry: &Registry, names: &[&str]) {
    let golden = load_golden("commands.json");
    let entries = golden.as_array().expect("commands.json is an array");
    let mut problems = Vec::new();

    let declared: Vec<&str> = registry.commands().iter().map(|c| c.name()).collect();
    for n in names {
        if !declared.contains(n) {
            problems.push(format!("/{n}: not declared in the registry"));
        }
    }
    for d in &declared {
        if !names.contains(d) {
            problems.push(format!("/{d}: declared but not in the expected list"));
        }
    }

    for name in names {
        let Some(def) = registry.command_named(name) else { continue };
        let Some(g) = entries.iter().find(|e| e["name"] == json!(name)) else {
            problems.push(format!("/{name}: not in commands.json"));
            continue;
        };
        let payload = def.payload();
        if let Some(d) = json_diff(&payload, &g["payload"], false) {
            problems.push(format!("/{name}: payload differs: {d}"));
        } else {
            let ours = serde_json::to_string(&payload).unwrap_or_default();
            let theirs = serde_json::to_string(&g["payload"]).unwrap_or_default();
            if ours != theirs {
                let d = json_diff(&payload, &g["payload"], true).unwrap_or_else(|| "serialization".into());
                problems.push(format!("/{name}: payload is JSON-equal but not byte-identical (key order changes the deploy hash): {d}"));
            }
        }
        let meta = def.metadata();
        for key in ["category", "module", "gated", "permissionLevel", "subcommandPermissions", "allowInDMs", "hasAutocomplete"] {
            if let Some(d) = json_diff(&meta[key], &g[key], false) {
                problems.push(format!("/{name}: {key} differs: {d}"));
            }
        }
        for e in def.validate() {
            problems.push(format!("/{name}: {e}"));
        }
    }
    assert!(problems.is_empty(), "commands do not match the JS golden file:\n{}", problems.join("\n"));
}

fn module_ref_json(m: crate::hooks::ModuleRef) -> Option<Value> {
    match m {
        crate::hooks::ModuleRef::Inherit => None,
        crate::hooks::ModuleRef::Ungated => Some(Value::Null),
        crate::hooks::ModuleRef::Module(k) => Some(json!(k.as_str())),
    }
}

/// The plugin's manifest in the `plugins.json` shape (`module` left out when inherited, `null` when ungated).
pub fn plugin_manifest(plugin: &crate::plugin::Plugin) -> Value {
    let hooks: Vec<Value> = plugin
        .hooks
        .iter()
        .map(|h| {
            let mut m = Map::new();
            m.insert("event".into(), json!(h.event().as_str()));
            m.insert("id".into(), json!(h.id()));
            if let Some(v) = module_ref_json(h.module()) {
                m.insert("module".into(), v);
            }
            m.insert("always".into(), json!(h.always()));
            m.insert("timeoutMs".into(), json!(h.timeout_ms()));
            m.insert("hasFilter".into(), json!(h.has_filter()));
            Value::Object(m)
        })
        .collect();
    let components: Vec<Value> = plugin
        .components
        .iter()
        .map(|c| {
            let mut m = Map::new();
            m.insert("type".into(), json!(c.kind.as_str()));
            m.insert("prefix".into(), json!(c.prefix));
            if let Some(v) = module_ref_json(c.module) {
                m.insert("module".into(), v);
            }
            Value::Object(m)
        })
        .collect();
    json!({
        "id": plugin.id,
        "module": plugin.module.map(|m| m.as_str()),
        "hooks": hooks,
        "components": components,
        "jobs": plugin.jobs.iter().map(|j| j.0.as_str()).collect::<Vec<_>>(),
        "provides": plugin.provides.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
        "hasInit": plugin.init.is_some(),
    })
}

/// Checks that the registry's plugin `id` has the JS plugin's module, hooks (event, id, module, always, timeoutMs,
/// hasFilter, in order), components (type, prefix, module), job types, providers and init, as `plugins.json` lists
/// them, and that it passes the plugin validation with no error. Panics with the first difference.
pub fn assert_plugin_matches_golden(registry: &Registry, id: &str) {
    let golden = load_golden("plugins.json");
    let entry = golden.as_array().and_then(|a| a.iter().find(|p| p["id"] == json!(id))).unwrap_or_else(|| panic!("plugin {id} is not in plugins.json"));
    let plugin = registry.plugin_named(id).unwrap_or_else(|| panic!("plugin {id} is not declared in the registry"));
    let mut ours = plugin_manifest(plugin);
    let mut theirs = entry.clone();
    if let Some(o) = theirs.as_object_mut() {
        o.remove("file");
    }
    if let Some(o) = ours.as_object_mut() {
        o.remove("file");
    }
    if let Some(d) = json_diff(&ours, &theirs, false) {
        panic!("plugin {id} does not match plugins.json: {d}");
    }
    // The JS keeps `module` absent (inherit) apart from `null` (ungated): JSON-equality above treats an absent key
    // as a difference, so both are checked.
    let check = crate::plugin::collect_plugins(vec![clone_manifest_only(plugin)]);
    assert!(check.errors.is_empty(), "plugin {id} fails validation: {:?}", check.errors);
}

/// A copy of the plugin's declarations (handlers shared) for validation.
fn clone_manifest_only(p: &crate::plugin::Plugin) -> crate::plugin::Plugin {
    crate::plugin::Plugin {
        id: p.id,
        module: p.module,
        hooks: p.hooks.iter().map(|h| h.clone_box()).collect(),
        components: p.components.clone(),
        jobs: p.jobs.clone(),
        provides: p.provides.clone(),
        init: p.init.clone(),
    }
}

/// The whole-bot check for `bins/bot`: every command and plugin of the JS bot is declared and matches its golden
/// entry, every persistent customId prefix has exactly one handler (a plugin's or the core `noop`), and every job type
/// has a handler. Panics with every problem.
pub fn assert_registry_matches_golden(registry: &Registry) {
    let commands = load_golden("commands.json");
    let names: Vec<String> = commands.as_array().map(|a| a.iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_commands_match_golden(registry, &refs);
    let plugins = load_golden("plugins.json");
    let mut problems = Vec::new();
    let golden_ids: Vec<&str> = plugins.as_array().map(|a| a.iter().filter_map(|p| p["id"].as_str()).collect()).unwrap_or_default();
    for id in &golden_ids {
        if registry.plugin_named(id).is_none() {
            problems.push(format!("plugin {id} is not declared"));
        } else if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_plugin_matches_golden(registry, id))) {
            problems.push(crate::error::panic_message(e.as_ref()));
        }
    }
    for p in registry.plugins() {
        if !golden_ids.contains(&p.id) {
            problems.push(format!("plugin {} is not a JS plugin", p.id));
        }
    }
    problems.extend(persistent_handler_problems(registry));
    assert!(problems.is_empty(), "the registry does not match the JS bot:\n{}", problems.join("\n"));
}

/// Every persistent customId prefix must have exactly one handler per listed component type (a plugin's, or the core
/// `noop`), and every job type a handler (`test/registry.test.js`, "persistent prefixes without exactly one handler").
pub fn persistent_handler_problems(registry: &Registry) -> Vec<String> {
    let mut problems = Vec::new();
    for info in bot_config::CUSTOM_ID_PREFIXES.iter().filter(|i| i.kind == bot_config::CustomIdKind::Persistent) {
        for ty in info.types {
            let handlers = registry
                .plugins()
                .iter()
                .flat_map(|p| p.components.iter())
                .filter(|c| c.prefix == info.prefix && c.kind.component_type() == *ty)
                .count()
                + usize::from(info.prefix == "noop" && *ty == bot_config::ComponentType::Button);
            if handlers != 1 {
                problems.push(format!("persistent {} prefix \"{}\" has {handlers} handler(s)", ty.as_str(), info.prefix));
            }
        }
    }
    for job in bot_config::JobType::ALL {
        if !registry.plugins().iter().any(|p| p.jobs.iter().any(|j| j.0 == job)) {
            problems.push(format!("job type {job} has no handler"));
        }
    }
    problems
}

/// [`persistent_handler_problems`] limited to what these plugins own: the customId prefixes and job types the JS
/// plugins `plugin_ids` declare in `plugins.json` (so a feature crate checks only its own handlers).
pub fn persistent_handler_problems_for(registry: &Registry, plugin_ids: &[&str]) -> Vec<String> {
    let golden = load_golden("plugins.json");
    let mut prefixes = Vec::new();
    let mut jobs = Vec::new();
    for p in golden.as_array().into_iter().flatten().filter(|p| p["id"].as_str().is_some_and(|id| plugin_ids.contains(&id))) {
        for c in p["components"].as_array().into_iter().flatten() {
            if let Some(prefix) = c["prefix"].as_str() {
                prefixes.push(format!("prefix \"{prefix}\" has"));
            }
        }
        for j in p["jobs"].as_array().into_iter().flatten() {
            if let Some(j) = j.as_str() {
                jobs.push(format!("job type {j} has"));
            }
        }
    }
    persistent_handler_problems(registry)
        .into_iter()
        .filter(|line| prefixes.iter().any(|p| line.contains(p.as_str())) || jobs.iter().any(|j| line.starts_with(j.as_str())))
        .collect()
}

/// A [`crate::scheduler::JobStore`] that fails on demand (a connection error), wrapping a real store: the next `n`
/// schedule writes (inserts and dedupe upserts, i.e. `scheduleJob`), or every call while
/// [`FailingJobStore::set_failing`] is on.
pub struct FailingJobStore {
    inner: Arc<dyn crate::scheduler::JobStore>,
    fail_schedules: std::sync::atomic::AtomicUsize,
    fail_all: std::sync::atomic::AtomicBool,
    failures: std::sync::atomic::AtomicUsize,
}

impl FailingJobStore {
    pub fn new(inner: Arc<dyn crate::scheduler::JobStore>) -> Self {
        Self {
            inner,
            fail_schedules: std::sync::atomic::AtomicUsize::new(0),
            fail_all: std::sync::atomic::AtomicBool::new(false),
            failures: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The next `n` schedule writes fail.
    pub fn fail_next_schedules(&self, n: usize) {
        self.fail_schedules.store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Every call fails while on.
    pub fn set_failing(&self, on: bool) {
        self.fail_all.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many calls failed so far.
    pub fn failures(&self) -> usize {
        self.failures.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn check(&self, schedule: bool) -> Result<(), bot_db::DbError> {
        use std::sync::atomic::Ordering;
        let fail = self.fail_all.load(Ordering::SeqCst)
            || (schedule && self.fail_schedules.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok());
        if fail {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(bot_db::DbError::Sqlx(sqlx::Error::PoolTimedOut));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::scheduler::JobStore for FailingJobStore {
    async fn claim(&self, req: &crate::scheduler::ClaimRequest) -> Result<Vec<bot_db::ScheduledJob>, bot_db::DbError> {
        self.check(false)?;
        self.inner.claim(req).await
    }
    async fn insert(&self, job: &crate::scheduler::NewJob) -> Result<crate::scheduler::Scheduled, bot_db::DbError> {
        self.check(true)?;
        self.inner.insert(job).await
    }
    async fn upsert_dedupe(&self, job: &crate::scheduler::NewJob, on_conflict: &crate::scheduler::OnConflict) -> Result<crate::scheduler::Scheduled, bot_db::DbError> {
        self.check(true)?;
        self.inner.upsert_dedupe(job, on_conflict).await
    }
    async fn settle(&self, id: &str, version: i32, update: &crate::scheduler::JobUpdate) -> Result<u64, bot_db::DbError> {
        self.check(false)?;
        self.inner.settle(id, version, update).await
    }
    async fn delete_version(&self, id: &str, version: i32) -> Result<u64, bot_db::DbError> {
        self.check(false)?;
        self.inner.delete_version(id, version).await
    }
    async fn cancel(&self, filter: &crate::scheduler::CancelWhere) -> Result<u64, bot_db::DbError> {
        self.check(false)?;
        self.inner.cancel(filter).await
    }
    async fn get(&self, dedupe_key: &str) -> Result<Option<bot_db::ScheduledJob>, bot_db::DbError> {
        self.check(false)?;
        self.inner.get(dedupe_key).await
    }
    async fn get_many(&self, dedupe_keys: &[String]) -> Result<Vec<bot_db::ScheduledJob>, bot_db::DbError> {
        self.check(false)?;
        self.inner.get_many(dedupe_keys).await
    }
    async fn settle_merged(
        &self,
        id: &str,
        version: i32,
        next: crate::scheduler::SettleNextFn,
        run_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::scheduler::SettleOutcome, bot_db::DbError> {
        self.check(false)?;
        self.inner.settle_merged(id, version, next, run_at).await
    }
    async fn gc(&self) -> Result<u64, bot_db::DbError> {
        self.check(false)?;
        self.inner.gc().await
    }
}

impl DbTestApp {
    /// [`DbTestApp::new`] whose PostgreSQL job store fails on demand (see [`FailingJobStore`]).
    pub async fn with_failing_jobs(database_url: &str) -> (Self, Arc<FailingJobStore>) {
        let pool = connect_test_pool(database_url).await;
        let failing = Arc::new(FailingJobStore::new(Arc::new(crate::scheduler::PgJobStore::new(pool.clone()))));
        let app = App::builder().postgres(pool.clone()).job_store(failing.clone()).build();
        let responses = Arc::new(RecordingBackend::new());
        app.set_interaction_backend(responses.clone());
        (Self { app, pool, responses, redis: None }, failing)
    }
}

// ── An offline serenity cache ──────────────────────────────────────────────────

/// A guild as Discord sends it in GUILD_CREATE, with the fields serenity requires. `roles` are `(id, permission bits)`
/// (the `@everyone` role, id = guild id, is added with no permissions); `members` are `(user id, role ids)`.
pub fn guild_create_json(guild_id: &str, owner_id: &str, roles: &[(&str, u64)], members: &[(&str, &[&str])]) -> Value {
    let mut role_list = vec![json!({
        "id": guild_id, "name": "@everyone", "color": 0, "hoist": false, "position": 0, "permissions": "0",
        "managed": false, "mentionable": false, "flags": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    })];
    for (i, (id, bits)) in roles.iter().enumerate() {
        role_list.push(json!({
            "id": id, "name": format!("role{i}"), "color": 0, "hoist": false, "position": i + 1,
            "permissions": bits.to_string(), "managed": false, "mentionable": false, "flags": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
        }));
    }
    let member_list: Vec<Value> = members
        .iter()
        .map(|(id, roles)| {
            json!({
                "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null },
                "roles": roles, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
            })
        })
        .collect();
    json!({
        "id": guild_id, "name": "Test Guild", "icon": null, "splash": null, "discovery_splash": null,
        "owner_id": owner_id, "afk_channel_id": null, "afk_timeout": 300, "verification_level": 0,
        "default_message_notifications": 0, "explicit_content_filter": 0, "roles": role_list, "emojis": [],
        "features": [], "mfa_level": 0, "application_id": null, "system_channel_id": null, "system_channel_flags": 0,
        "rules_channel_id": null, "max_members": 500000, "vanity_url_code": null, "description": null, "banner": null,
        "premium_tier": 0, "premium_subscription_count": 0, "preferred_locale": "en-US", "public_updates_channel_id": null,
        "nsfw_level": 0, "stickers": [], "premium_progress_bar_enabled": false,
        "joined_at": "2025-01-01T00:00:00.000000+00:00", "large": false, "unavailable": false,
        "member_count": member_list.len(), "voice_states": [], "members": member_list, "channels": [], "threads": [],
        "presences": [], "stage_instances": [], "guild_scheduled_events": []
    })
}

/// A serenity HTTP client that never reaches Discord: every request goes to an unreachable local proxy (connection
/// refused at once) with the rate limiter off, so offline tests make no network calls.
pub fn offline_http() -> serenity::all::Http {
    serenity::all::HttpBuilder::new("offline-test-token").proxy("http://127.0.0.1:1").ratelimiter_disabled(true).build()
}

/// A serenity cache holding these guilds (GUILD_CREATE payloads, see [`guild_create_json`]), and a Discord handle on
/// it whose HTTP client has no valid token (any REST call fails, as an unreachable Discord would).
pub fn offline_discord(guilds: &[Value]) -> crate::discord::DiscordHandle {
    offline_discord_with(guilds, offline_http())
}

/// [`offline_discord`] with this HTTP client (a [`FakeRest::http`], for example).
pub fn offline_discord_with(guilds: &[Value], http: serenity::all::Http) -> crate::discord::DiscordHandle {
    let cache = Arc::new(serenity::all::Cache::new());
    for g in guilds {
        let mut event: serenity::all::GuildCreateEvent =
            serde_json::from_value(g.clone()).unwrap_or_else(|e| panic!("GUILD_CREATE does not deserialize: {e}"));
        cache.update(&mut event);
    }
    crate::discord::DiscordHandle::new(Arc::new(http), cache, None, 1)
}

impl TestApp {
    /// Installs an offline Discord connection with these guilds cached (they become the available guilds unless a
    /// guild directory is set): permission checks then use guild-level permissions from the cache.
    /// The bot user ([`TEST_BOT_ID`]) is registered too, so bot permission checks see the bot.
    pub fn with_discord_guilds(self, guilds: &[Value]) -> Self {
        self.app.set_discord(offline_discord_as(guilds));
        self
    }

    /// Installs a Discord connection whose REST calls go to `rest` (no guilds cached; the bot user known).
    pub fn with_rest(self, rest: &FakeRest) -> Self {
        self.app.set_discord(rest.discord(&[]));
        self
    }

    /// [`Self::with_fake_guilds`] with REST going to `rest`.
    pub fn with_fake_guilds_rest(self, guilds: &[FakeGuild], rest: &FakeRest) -> Self {
        let json: Vec<Value> = guilds.iter().map(FakeGuild::json).collect();
        self.app.set_discord(rest.discord(&json));
        self
    }

    /// An offline app whose job store fails on demand (wrapping the in-memory store, which `job_store` still is):
    /// see [`FailingJobStore`].
    pub fn with_failing_jobs() -> (Self, Arc<FailingJobStore>) {
        let clock = ManualClock::starting_now();
        let shared: crate::clock::SharedClock = Arc::new(clock.clone());
        let cache_backend = Arc::new(MemoryBackend::new(shared.clone()));
        let settings_store = Arc::new(MemorySettingsStore::new());
        let job_store = Arc::new(MemoryJobStore::new(shared.clone()));
        let failing = Arc::new(FailingJobStore::new(job_store.clone()));
        let app = App::builder()
            .clock(shared)
            .cache_backend(cache_backend.clone())
            .settings_store(settings_store.clone())
            .job_store(failing.clone())
            .build();
        let responses = Arc::new(RecordingBackend::new());
        app.set_interaction_backend(responses.clone());
        (Self { app, clock, responses, cache_backend, settings_store, job_store }, failing)
    }
}

// ── Richer offline guilds ──────────────────────────────────────────────────────

/// The bot's user id in offline Discord handles built by [`FakeGuild`] / [`offline_discord_as`].
pub const TEST_BOT_ID: &str = "100000000000000002";

/// A GUILD_CREATE payload built piece by piece: roles (with positions, flags and bot tags), members, channels with
/// permission overwrites, and threads. Feed it to [`offline_discord_as`] or [`TestApp::with_fake_guilds`].
#[derive(Debug, Clone)]
pub struct FakeGuild {
    id: String,
    owner: String,
    everyone: u64,
    roles: Vec<Value>,
    members: Vec<Value>,
    channels: Vec<Value>,
    threads: Vec<Value>,
    voice_states: Vec<Value>,
    presences: Vec<Value>,
    /// Top-level GUILD_CREATE fields set by the setters (`name`, `afk_channel_id`, `premium_tier`, ...).
    fields: Vec<(String, Value)>,
}

impl FakeGuild {
    /// A guild owned by `owner` with only `@everyone` (no permissions).
    pub fn new(id: &str, owner: &str) -> Self {
        Self {
            id: id.into(),
            owner: owner.into(),
            everyone: 0,
            roles: Vec::new(),
            members: Vec::new(),
            channels: Vec::new(),
            threads: Vec::new(),
            voice_states: Vec::new(),
            presences: Vec::new(),
            fields: Vec::new(),
        }
    }

    /// `@everyone`'s permissions.
    pub fn everyone(mut self, permissions: Permissions) -> Self {
        self.everyone = permissions.bits();
        self
    }

    /// A role at `position` with `permissions`.
    pub fn role(self, id: &str, position: u16, permissions: Permissions) -> Self {
        self.role_with(id, position, permissions, |_| {})
    }

    /// A role with extra JSON (`name`, `managed`, `mentionable`, `tags`, ...).
    pub fn role_with(mut self, id: &str, position: u16, permissions: Permissions, edit: impl FnOnce(&mut Value)) -> Self {
        let mut r = json!({
            "id": id, "name": format!("role{id}"), "color": 0, "hoist": false, "position": position,
            "permissions": permissions.bits().to_string(), "managed": false, "mentionable": false, "flags": 0,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
        });
        edit(&mut r);
        self.roles.push(r);
        self
    }

    /// A member with role ids.
    pub fn member(mut self, id: &str, roles: &[&str]) -> Self {
        self.members.push(json!({
            "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": id == TEST_BOT_ID },
            "roles": roles, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
        }));
        self
    }

    /// A channel of Discord type `kind` (0 text, 2 voice, 4 category, 5 announcement, 15 forum) with overwrites
    /// `(id, is_member, allow, deny)`.
    pub fn channel(mut self, id: &str, kind: u8, name: &str, overwrites: &[(&str, bool, Permissions, Permissions)]) -> Self {
        let ows: Vec<Value> = overwrites
            .iter()
            .map(|(oid, member, allow, deny)| json!({ "id": oid, "type": u8::from(*member), "allow": allow.bits().to_string(), "deny": deny.bits().to_string() }))
            .collect();
        self.channels.push(json!({
            "id": id, "type": kind, "name": name, "position": self.channels.len(), "permission_overwrites": ows,
            "nsfw": false, "parent_id": null, "topic": null, "last_message_id": null, "rate_limit_per_user": 0
        }));
        self
    }

    /// A thread of Discord type `kind` (11 public, 12 private) under `parent`.
    pub fn thread(mut self, id: &str, kind: u8, parent: &str) -> Self {
        self.threads.push(json!({
            "id": id, "type": kind, "name": format!("thread{id}"), "parent_id": parent, "owner_id": self.owner, "guild_id": self.id,
            "message_count": 0, "member_count": 1, "rate_limit_per_user": 0, "nsfw": false, "permission_overwrites": [],
            "thread_metadata": { "archived": false, "auto_archive_duration": 1440, "archive_timestamp": "2025-01-01T00:00:00.000000+00:00", "locked": false }
        }));
        self
    }

    /// Any top-level GUILD_CREATE field (`"system_channel_id"`, `"icon"`, ...); the last write wins.
    pub fn field(mut self, key: &str, value: Value) -> Self {
        self.fields.retain(|(k, _)| k != key);
        self.fields.push((key.to_string(), value));
        self
    }

    /// The guild name (default `Test Guild`).
    pub fn name(self, name: &str) -> Self {
        self.field("name", json!(name))
    }

    /// The AFK channel and timeout (seconds).
    pub fn afk(self, channel: &str, timeout_secs: u64) -> Self {
        self.field("afk_channel_id", json!(channel)).field("afk_timeout", json!(timeout_secs))
    }

    /// `premium_tier` (0-3).
    pub fn premium_tier(self, tier: u8) -> Self {
        self.field("premium_tier", json!(tier))
    }

    /// `premium_subscription_count` (boosts).
    pub fn premium_count(self, count: u64) -> Self {
        self.field("premium_subscription_count", json!(count))
    }

    /// The guild features (`"ROLE_ICONS"`, `"COMMUNITY"`, ...).
    pub fn features(self, features: &[&str]) -> Self {
        self.field("features", json!(features))
    }

    /// `vanity_url_code`.
    pub fn vanity_code(self, code: &str) -> Self {
        self.field("vanity_url_code", json!(code))
    }

    /// A member that is a bot (other than [`TEST_BOT_ID`], which [`Self::member`] already marks).
    pub fn bot_member(self, id: &str, roles: &[&str]) -> Self {
        self.member_with(id, roles, |m| m["user"]["bot"] = json!(true))
    }

    /// A member with extra JSON (`nick`, `avatar`, `premium_since`, `communication_disabled_until`, `user.*`, ...).
    pub fn member_with(mut self, id: &str, roles: &[&str], edit: impl FnOnce(&mut Value)) -> Self {
        self = self.member(id, roles);
        if let Some(m) = self.members.last_mut() {
            edit(m);
        }
        self
    }

    /// A channel with extra JSON (`parent_id`, `nsfw`, `user_limit`, `bitrate`, `topic`, ...).
    pub fn channel_with(
        mut self,
        id: &str,
        kind: u8,
        name: &str,
        overwrites: &[(&str, bool, Permissions, Permissions)],
        edit: impl FnOnce(&mut Value),
    ) -> Self {
        self = self.channel(id, kind, name, overwrites);
        if let Some(c) = self.channels.last_mut() {
            edit(c);
        }
        self
    }

    /// Edits an already added channel or thread (no-op when the id is unknown).
    pub fn edit_channel(mut self, id: &str, edit: impl FnOnce(&mut Value)) -> Self {
        if let Some(c) = self.channels.iter_mut().chain(self.threads.iter_mut()).find(|c| c["id"] == json!(id)) {
            edit(c);
        }
        self
    }

    /// A channel's category (`parent_id`).
    pub fn channel_parent(self, id: &str, parent: &str) -> Self {
        let parent = parent.to_string();
        self.edit_channel(id, move |c| c["parent_id"] = json!(parent))
    }

    /// A channel's `nsfw` flag.
    pub fn channel_nsfw(self, id: &str, nsfw: bool) -> Self {
        self.edit_channel(id, move |c| c["nsfw"] = json!(nsfw))
    }

    /// A voice channel's `user_limit`.
    pub fn channel_user_limit(self, id: &str, limit: u32) -> Self {
        self.edit_channel(id, move |c| c["user_limit"] = json!(limit))
    }

    /// A voice channel's `bitrate`.
    pub fn channel_bitrate(self, id: &str, bitrate: u32) -> Self {
        self.edit_channel(id, move |c| c["bitrate"] = json!(bitrate))
    }

    /// A thread's `owner_id` (default: the guild owner).
    pub fn thread_owner(self, id: &str, owner: &str) -> Self {
        let owner = owner.to_string();
        self.edit_channel(id, move |c| c["owner_id"] = json!(owner))
    }

    /// `user` is in voice channel `channel`.
    pub fn voice_state(self, user: &str, channel: &str) -> Self {
        self.voice_state_with(user, channel, |_| {})
    }

    /// A voice state with extra JSON (`self_mute`, `self_deaf`, `self_stream`, ...).
    pub fn voice_state_with(mut self, user: &str, channel: &str, edit: impl FnOnce(&mut Value)) -> Self {
        let mut v = json!({
            "channel_id": channel, "user_id": user, "session_id": format!("session{user}"), "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null
        });
        edit(&mut v);
        self.voice_states.push(v);
        self
    }

    /// `user`'s presence: `status` (`online`, `idle`, `dnd`, `offline`) and activities as Discord sends them
    /// (`{ "name": "...", "type": 0 }`; `created_at` is filled when missing).
    pub fn presence(mut self, user: &str, status: &str, activities: Vec<Value>) -> Self {
        let activities: Vec<Value> = activities
            .into_iter()
            .map(|mut a| {
                if a.get("created_at").is_none() {
                    a["created_at"] = json!(1_735_689_600_000_u64);
                }
                a
            })
            .collect();
        self.presences.push(json!({ "user": { "id": user }, "status": status, "activities": activities, "client_status": { "desktop": status } }));
        self
    }

    /// The GUILD_CREATE JSON.
    pub fn json(&self) -> Value {
        let mut g = guild_create_json(&self.id, &self.owner, &[], &[]);
        let mut roles = vec![json!({
            "id": self.id, "name": "@everyone", "color": 0, "hoist": false, "position": 0, "permissions": self.everyone.to_string(),
            "managed": false, "mentionable": false, "flags": 0,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
        })];
        roles.extend(self.roles.iter().cloned());
        g["roles"] = Value::Array(roles);
        g["members"] = Value::Array(self.members.clone());
        g["member_count"] = json!(self.members.len());
        g["channels"] = Value::Array(self.channels.clone());
        g["threads"] = Value::Array(self.threads.clone());
        g["voice_states"] = Value::Array(self.voice_states.clone());
        g["presences"] = Value::Array(self.presences.clone());
        for (k, v) in &self.fields {
            g[k.as_str()] = v.clone();
        }
        g
    }
}

/// [`offline_discord`] whose cache also knows the bot's own user ([`TEST_BOT_ID`], from a READY payload), so bot
/// permission checks work offline.
pub fn offline_discord_as(guilds: &[Value]) -> crate::discord::DiscordHandle {
    offline_discord_as_with(guilds, offline_http())
}

/// [`offline_discord_as`] with this HTTP client (a [`FakeRest::http`], for example).
pub fn offline_discord_as_with(guilds: &[Value], http: serenity::all::Http) -> crate::discord::DiscordHandle {
    let handle = offline_discord_with(guilds, http);
    let ready = json!({
        "v": 10,
        "user": { "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true, "verified": true, "mfa_enabled": false, "flags": 0 },
        "guilds": [],
        "session_id": "offline",
        "resume_gateway_url": "wss://offline.invalid",
        "shard": [0, 1],
        "application": { "id": "100000000000000001", "flags": 0 }
    });
    let mut event: serenity::all::ReadyEvent = serde_json::from_value(json!(ready)).unwrap_or_else(|e| panic!("READY does not deserialize: {e}"));
    handle.cache.update(&mut event);
    handle
}

impl TestApp {
    /// Installs an offline Discord connection with these fake guilds cached and the bot user known.
    pub fn with_fake_guilds(self, guilds: &[FakeGuild]) -> Self {
        let json: Vec<Value> = guilds.iter().map(FakeGuild::json).collect();
        self.app.set_discord(offline_discord_as(&json));
        self
    }
}

/// A guild message as the gateway delivers it (`MESSAGE_CREATE`), by `author_id` (a user, not a bot) in `channel_id`
/// of `guild_id`, with `content`.
pub fn fake_message(id: &str, guild_id: &str, channel_id: &str, author_id: &str, content: &str) -> serenity::all::Message {
    let json = json!({
        "id": id, "channel_id": channel_id, "guild_id": guild_id,
        "author": { "id": author_id, "username": format!("user{author_id}"), "discriminator": "0", "avatar": null },
        "member": { "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 },
        "content": content, "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
        "type": 0, "components": []
    });
    serde_json::from_value(json).unwrap_or_else(|e| panic!("MESSAGE_CREATE does not deserialize: {e}"))
}

impl DbTestApp {
    /// Installs a registry (as the bot binary does at boot).
    pub fn install(&self, registry: Registry) -> crate::registry::BootReport {
        registry.install(&self.app)
    }
}
