#![recursion_limit = "512"]
//! Database tests of the eleven stores, on the mock PostgreSQL and Redis (TEST_DATABASE_URL, TEST_REDIS_URL), in both
//! cross-language directions:
//! - the same steps run through the JS stores (rust/fixtures/gen/dash-records-b-js.mjs) on one guild and through the
//!   Rust stores on another; every answer, task run, Discord call, row and cache value is compared;
//! - then each side lists what the other side wrote (Rust writes, JS reads; JS writes, Rust reads).
//!
//! The scenarios port the store cases of test/dashboard-collections.test.js, test/dashboard-collections-more.test.js,
//! test/dashboard-panels.test.js and test/sec-dashboard.test.js.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bot_core::settings::SettingsPatch;
use bot_core::testing::{DbTestApp, database_url, redis_url, run_node};
use dash_collections::testing::{FakeDiscord, MemoryTasks};
use dash_collections::{
    Collections, DashboardMember, DashboardUser, DiscordApiError, DiscordResources, ListOpts, Mode, RecordStore, StoreCtx, StoreError,
    TaskResult,
};
use serde_json::{Map, Value, json};

// ── The runner ─────────────────────────────────────────────────────────────────────────────────────────────────────

/// "$G" -> the guild, "@id<n>" -> the id step n answered, anywhere in a string.
fn fill(v: &Value, guild: &str, ids: &[Option<String>]) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.replace("$G", guild);
            while let Some(pos) = out.find("@id") {
                let digits: String = out[pos + 3..].chars().take_while(char::is_ascii_digit).collect();
                let Some(id) = digits.parse::<usize>().ok().and_then(|n| ids.get(n).cloned().flatten()) else { break };
                out.replace_range(pos..pos + 3 + digits.len(), &id);
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, guild, ids)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), fill(x, guild, ids))).collect()),
        other => other.clone(),
    }
}

struct Rust {
    t: DbTestApp,
    stores: HashMap<&'static str, Arc<dyn RecordStore>>,
}

async fn rust_side() -> Option<Rust> {
    let db = database_url()?;
    let redis = redis_url()?;
    let t = DbTestApp::with_redis(&db, &redis).await;
    let stores = dash_records_b::stores(&t.app).into_iter().collect();
    Some(Rust { t, stores })
}

/// The StoreCtx of a ctx spec, with its doubles.
fn ctx_of(spec: &Value, guild: &str) -> (StoreCtx, Option<Arc<FakeDiscord>>, Option<Arc<MemoryTasks>>) {
    let user_id = spec.get("userId").and_then(Value::as_str).unwrap_or("0").to_string();
    let mut ctx = StoreCtx { guild_id: guild.into(), user_id, ..Default::default() };
    ctx.user = spec.get("user").filter(|v| !v.is_null()).map(|u| serde_json::from_value::<DashboardUser>(u.clone()).expect("user"));
    ctx.member = spec.get("member").filter(|v| !v.is_null()).map(|m| serde_json::from_value::<DashboardMember>(m.clone()).expect("member"));
    let mut discord = None;
    if let Some(res) = spec.get("resources") {
        let d = Arc::new(FakeDiscord::new());
        if !res.is_null() {
            d.set_resources(guild, serde_json::from_value::<DiscordResources>(res.clone()).expect("resources"));
        }
        if let Some(fails) = spec.get("discordFail").and_then(Value::as_object) {
            for (method, f) in fails {
                let mut e = DiscordApiError::new(
                    f["message"].as_str().unwrap_or(""),
                    f.get("status").and_then(Value::as_u64).unwrap_or(0) as u16,
                    f["code"].as_str().unwrap_or(""),
                );
                if let Some(c) = f.get("discordCode").and_then(Value::as_i64) {
                    e = e.with_discord_code(c);
                }
                d.fail_next(method, e);
            }
        }
        ctx.discord = Some(d.clone());
        discord = Some(d);
    }
    let mut tasks = None;
    if let Some(answers) = spec.get("tasks").filter(|v| !v.is_null()) {
        let m = Arc::new(MemoryTasks::new());
        for (kind, r) in answers.as_object().into_iter().flatten() {
            m.answer(kind, serde_json::from_value::<TaskResult>(r.clone()).expect("task result"));
        }
        ctx.tasks = Some(m.clone());
        tasks = Some(m);
    }
    (ctx, discord, tasks)
}

fn error_json(e: StoreError) -> Value {
    match e {
        StoreError::Record(r) => json!({ "error": r.to_json() }),
        StoreError::Field(f) => json!({ "field": { "path": f.path, "message": f.message } }),
        StoreError::Discord(d) => json!({ "discord": d.message }),
        StoreError::Internal(e) => json!({ "internal": e.to_string() }),
    }
}

impl Rust {
    fn store(&self, key: &str) -> &Arc<dyn RecordStore> {
        self.stores.get(key).unwrap_or_else(|| panic!("no store {key}"))
    }

    async fn sql(&self, sql: &str, params: &[Value]) -> Result<u64, String> {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        for p in params {
            q = q.bind(p.as_str().map(str::to_string));
        }
        q.execute(&self.t.pool).await.map(|r| r.rows_affected()).map_err(|e| e.to_string())
    }

    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, String> {
        let wrapped = format!("SELECT row_to_json(t) AS r FROM ({sql}) t");
        let mut q = sqlx::query_scalar::<_, Value>(sqlx::AssertSqlSafe(wrapped));
        for p in params {
            q = q.bind(p.as_str().map(str::to_string));
        }
        q.fetch_all(&self.t.pool).await.map_err(|e| e.to_string())
    }

    async fn step(&self, step: &Value, ctxs: &Value, guild: &str) -> Value {
        let params = step.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
        match step.get("op").and_then(Value::as_str) {
            Some("sql") => {
                return match self.sql(step["sql"].as_str().unwrap_or(""), &params).await {
                    Ok(n) => json!({ "out": n }),
                    Err(e) => json!({ "internal": e }),
                };
            }
            Some("query") => {
                return match self.query(step["sql"].as_str().unwrap_or(""), &params).await {
                    Ok(rows) => json!({ "out": rows }),
                    Err(e) => json!({ "internal": e }),
                };
            }
            Some("cache") => return json!({ "out": self.t.app.cache().get(step["key"].as_str().unwrap_or("")).await }),
            Some("cacheset") => {
                let ttl = step.get("ttl").and_then(Value::as_u64).unwrap_or(300);
                return json!({ "out": self.t.app.cache().set(step["key"].as_str().unwrap_or(""), &step["value"], ttl).await });
            }
            Some("settings") => {
                let patch = SettingsPatch(step["patch"].as_object().cloned().unwrap_or_default());
                self.t.app.settings().update_settings(guild, patch).await.expect("settings update");
                return json!({ "out": null });
            }
            Some(other) => panic!("unknown op {other}"),
            None => {}
        }
        let key = step["store"].as_str().expect("store");
        let store = self.store(key);
        let ctx_name = step.get("ctx").and_then(Value::as_str).unwrap_or("default");
        let (ctx, discord, tasks) = ctx_of(ctxs.get(ctx_name).unwrap_or(&json!({})), guild);
        let id = step.get("id").and_then(Value::as_str).unwrap_or("");
        let method = step["method"].as_str().expect("method");
        let action = step.get("action").and_then(Value::as_str);
        let read = ctx.read_ctx();
        let mut data = step.get("data").cloned().unwrap_or(json!({}));
        let out: Result<Value, StoreError> = async {
            if step.get("validate") == Some(&Value::Bool(true)) {
                let now = ctx_now(&self.t);
                let (mode, current) = match method {
                    "create" => (Mode::Create, None),
                    "update" => (Mode::Update, store.get(guild, id, &read).await?),
                    _ => (Mode::Action, store.get(guild, id, &read).await?),
                };
                data = Value::Object(Collections::builtin().validate_record_input(key, &data, mode, action, current.as_ref(), now)?.data);
            }
            let data: Map<String, Value> = data.as_object().cloned().unwrap_or_default();
            Ok(match method {
                "list" => json!({ "out": store.list(guild, &ListOpts::default(), &read).await?.items }),
                "get" => json!({ "out": store.get(guild, id, &read).await? }),
                "create" => {
                    let s = store.create(guild, data, &ctx).await?;
                    json!({ "saved": { "item": s.item, "message": s.message } })
                }
                "update" => {
                    let s = store.update(guild, id, data, &ctx).await?;
                    json!({ "saved": { "item": s.item, "message": s.message } })
                }
                "remove" => {
                    store.remove(guild, id, &ctx).await?;
                    json!({ "out": null })
                }
                "action" => {
                    let a = action.unwrap_or("");
                    let r = if store.action_ids().contains(&a) {
                        store.action(guild, id, a, data, &ctx).await?
                    } else {
                        return Err(dash_collections::RecordError::not_found("Unknown action.").into());
                    };
                    json!({ "action": { "item": r.item, "message": r.message } })
                }
                other => panic!("unknown method {other}"),
            })
        }
        .await;
        let mut r = match out {
            Ok(v) => v,
            Err(e) => error_json(e),
        };
        let task_calls: Vec<Value> =
            tasks.map(|t| t.calls().into_iter().map(|(_, kind, payload, _)| json!({ "kind": kind, "payload": payload })).collect()).unwrap_or_default();
        let discord_calls: Vec<Value> =
            discord.map(|d| d.calls().into_iter().map(|c| json!({ "method": c.method, "args": c.args })).collect()).unwrap_or_default();
        r["tasks"] = Value::Array(task_calls);
        r["discord"] = Value::Array(discord_calls);
        r
    }

    /// Runs a spec on `guild`: one answer per step.
    async fn run(&self, spec: &Value, guild: &str) -> Vec<Value> {
        let mut ids: Vec<Option<String>> = Vec::new();
        let mut out = Vec::new();
        let ctxs = spec.get("ctxs").cloned().unwrap_or(json!({}));
        for raw in spec["steps"].as_array().expect("steps") {
            let ctxs = fill(&ctxs, guild, &ids);
            let step = fill(raw, guild, &ids);
            let mut r = self.step(&step, &ctxs, guild).await;
            for k in ["tasks", "discord"] {
                if r.get(k).is_none() {
                    r[k] = json!([]);
                }
            }
            let item = r.get("saved").map(|s| &s["item"]).or_else(|| r.get("out"));
            ids.push(item.and_then(|i| i.get("id")).and_then(Value::as_str).map(str::to_string));
            out.push(r);
        }
        out
    }
}

fn ctx_now(t: &DbTestApp) -> f64 {
    t.app.clock().now_ms() as f64
}

static SPEC_N: AtomicU64 = AtomicU64::new(0);

/// Runs a spec through the JS stores on `guild`.
fn run_js(spec: &Value, guild: &str) -> Vec<Value> {
    let mut s = spec.clone();
    s["guild"] = json!(guild);
    let dir = std::env::temp_dir().join("dash-records-b");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("spec-{}-{}-{guild}.json", std::process::id(), SPEC_N.fetch_add(1, Ordering::SeqCst)));
    std::fs::write(&path, serde_json::to_string(&s).expect("spec json")).expect("spec written");
    let out = run_node("rust/fixtures/gen/dash-records-b-js.mjs", &[path.to_str().expect("utf-8 path")]);
    let _ = std::fs::remove_file(&path);
    if let Some(f) = out.get("fatal") {
        panic!("the JS side failed: {f}");
    }
    out["results"].as_array().cloned().expect("results")
}

// ── Comparison ─────────────────────────────────────────────────────────────────────────────────────────────────────

/// Keys whose values are times of the run (they differ between the two runs by the seconds between them).
const TIME_KEYS: [&str; 4] = ["createdAt", "updatedAt", "lastRenamedAt", "messageMissingAt"];

fn is_generated_id(s: &str) -> bool {
    let cuid = s.len() == 25 && s.starts_with('c') && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let uuid = s.len() == 36 && s.bytes().enumerate().all(|(i, b)| if [8, 13, 18, 23].contains(&i) { b == b'-' } else { b.is_ascii_hexdigit() });
    cuid || uuid
}

/// Replaces the guild id with "$G", generated ids with "<id n>" (n in order of first appearance, also inside strings
/// such as customIds and message URLs), run times with "<time>" and internal error texts with "<internal>".
fn normalize(v: &Value, guild: &str, ids: &mut Vec<String>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.replace(guild, "$G");
            // ids anywhere in the text (rr:<groupId>:<roleId>, ...)
            let mut i = 0;
            while i < out.len() {
                let mut hit = None;
                for len in [36, 25] {
                    if let Some(sub) = out.get(i..i + len)
                        && is_generated_id(sub)
                        && (i == 0 || !out.as_bytes()[i - 1].is_ascii_alphanumeric())
                        && out.as_bytes().get(i + len).is_none_or(|b| !b.is_ascii_alphanumeric())
                    {
                        hit = Some(len);
                        break;
                    }
                }
                if let Some(len) = hit {
                    let id = out[i..i + len].to_string();
                    let n = ids.iter().position(|x| *x == id).unwrap_or_else(|| {
                        ids.push(id.clone());
                        ids.len() - 1
                    });
                    let token = format!("<id {n}>");
                    out.replace_range(i..i + len, &token);
                    i += token.len();
                } else {
                    i += out[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| normalize(x, guild, ids)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let v = if TIME_KEYS.contains(&k.as_str()) && x.is_string() {
                        json!("<time>")
                    } else if k == "internal" {
                        json!("<internal>")
                    } else {
                        normalize(x, guild, ids)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn norm_all(results: &[Value], guild: &str) -> Vec<Value> {
    let mut ids = Vec::new();
    results.iter().map(|r| normalize(r, guild, &mut ids)).collect()
}

/// Asserts the two runs answered the same, step by step (key order included, as JSON.stringify writes it).
fn assert_same(spec: &Value, js: &[Value], js_guild: &str, rs: &[Value], rs_guild: &str) {
    let (a, b) = (norm_all(js, js_guild), norm_all(rs, rs_guild));
    if std::env::var("DASH_DUMP").is_ok() {
        for (i, y) in b.iter().enumerate() {
            println!("step {i}: {}", serde_json::to_string(y).unwrap_or_default());
        }
    }
    assert_eq!(a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if let Some(d) = bot_core::testing::json_diff(x, y, true) {
            panic!(
                "step {i} differs ({d})\nstep: {}\nJS:   {}\nRust: {}",
                spec["steps"][i],
                serde_json::to_string(x).unwrap_or_default(),
                serde_json::to_string(y).unwrap_or_default()
            );
        }
    }
}

/// Runs a scenario on both sides, compares every step, then lets each side read what the other wrote: `reads` (steps,
/// usually lists and cache reads) run on both guilds through both languages and must answer the same.
async fn scenario(r: &Rust, js_guild: &str, rs_guild: &str, spec: &Value, reads: Value) -> (Vec<Value>, Vec<Value>) {
    r.t.reset_guilds(&[js_guild, rs_guild]).await;
    let js = run_js(spec, js_guild);
    let rs = r.run(spec, rs_guild).await;
    assert_same(spec, &js, js_guild, &rs, rs_guild);
    if reads.as_array().is_some_and(|a| !a.is_empty()) {
        let reads = json!({ "ctxs": spec.get("ctxs").cloned().unwrap_or(json!({})), "steps": reads });
        // Rust writes, JS reads; JS writes, Rust reads: each read equals the writer's own read of its data.
        let js_reads_rust = run_js(&reads, rs_guild);
        let rust_reads_rust = r.run(&reads, rs_guild).await;
        assert_eq!(strip_calls(&js_reads_rust), strip_calls(&rust_reads_rust), "JS reads what Rust wrote");
        let js_reads_js = run_js(&reads, js_guild);
        let rust_reads_js = r.run(&reads, js_guild).await;
        assert_eq!(strip_calls(&js_reads_js), strip_calls(&rust_reads_js), "Rust reads what JS wrote");
    }
    r.t.reset_guilds(&[js_guild, rs_guild]).await;
    (js, rs)
}

/// `list` steps of these stores.
fn lists(keys: &[&str]) -> Value {
    Value::Array(keys.iter().map(|k| json!({ "store": k, "method": "list" })).collect())
}

fn strip_calls(v: &[Value]) -> Vec<Value> {
    v.iter().map(|r| r.get("out").cloned().unwrap_or_else(|| r.clone())).collect()
}

// ── Fixtures (test/dashboard-collections.test.js) ───────────────────────────────────────────────────────────────────

const USER: &str = "955017000000000010";
const OWNER: &str = "955017000000000011";
const CH_TEXT: &str = "$G20";
const CH_NEWS: &str = "$G21";
const CH_VOICE: &str = "$G22";
const CH_NSFW: &str = "$G23";
const CAT: &str = "$G24";
const ROLE_PING: &str = "955017000000000030";
const ROLE_QUIET: &str = "955017000000000031";
const ROLE_MOD: &str = "955017000000000032";
const ROLE_HIGH: &str = "955017000000000033";
const ROLE_FLAGGED: &str = "955017000000000035";
const EMOJI_ID: &str = "955017000000000040";

fn role(id: &str, name: &str, position: i64, extra: Value) -> Value {
    let mut r = json!({ "id": id, "name": name, "color": null, "position": position, "managed": false, "everyone": false,
        "assignable": true, "mentionable": false, "dangerous": false, "permissions": "0" });
    for (k, v) in extra.as_object().into_iter().flatten() {
        r[k] = v.clone();
    }
    r
}

fn resources() -> Value {
    json!({
        "guild": { "id": "$G", "name": "Test Server", "icon": null, "memberCount": 1234, "ownerId": OWNER, "premiumTier": 0,
            "premiumSubscriptionCount": 7, "verificationLevel": 0 },
        "channels": [
            { "id": CH_TEXT, "name": "general", "type": 0, "kind": "text", "parentId": null, "position": 0, "nsfw": false },
            { "id": CH_NEWS, "name": "news", "type": 5, "kind": "announcement", "parentId": null, "position": 1, "nsfw": false },
            { "id": CH_VOICE, "name": "Lounge", "type": 2, "kind": "voice", "parentId": null, "position": 2, "nsfw": false },
            { "id": CH_NSFW, "name": "spicy", "type": 0, "kind": "text", "parentId": null, "position": 3, "nsfw": true },
            { "id": CAT, "name": "Support", "type": 4, "kind": "category", "parentId": null, "position": 4, "nsfw": false },
        ],
        "roles": [
            role(ROLE_HIGH, "admins", 10, json!({ "assignable": false })),
            role(ROLE_MOD, "mods", 4, json!({ "permissions": "2", "dangerous": true })),
            role(ROLE_PING, "pings", 2, json!({ "mentionable": true })),
            role(ROLE_QUIET, "quiet", 1, json!({})),
            role(ROLE_FLAGGED, "helpers", 1, json!({ "dangerous": true })),
            role("$G", "@everyone", 0, json!({ "everyone": true, "assignable": false })),
        ],
        "emojis": [{ "id": EMOJI_ID, "name": "blob", "animated": false }],
        "bot": { "userId": "955017000000000099", "topRolePosition": 5, "manageRoles": true, "createInvite": true },
    })
}

/// Channel ids are "<guild id>NN" (unique columns such as StatChannel.channelId hold one row per channel in the
/// whole database, so the JS guild and the Rust guild need their own).
///
/// The ctxs most scenarios use: `default` (a member with Manage Server, the bot's access check passing), `owner`,
/// `nodiscord` (no Discord client, no task runner).
fn ctxs(tasks: Value) -> Value {
    let user = json!({ "id": USER, "username": "alice", "globalName": "Alice", "avatar": null });
    let mut tasks = tasks;
    tasks["access.channels"] = json!({ "status": "done", "message": "ok", "data": { "problems": [] } });
    json!({
        "default": { "userId": USER, "user": user, "member": { "owner": false, "permissions": "32" }, "resources": resources(), "tasks": tasks },
        "owner": { "userId": OWNER, "user": user, "member": { "owner": true, "permissions": "0" }, "resources": resources(), "tasks": tasks },
        "plain": { "userId": USER, "user": user, "member": { "owner": false, "permissions": "0", "highestRolePosition": 2 }, "resources": resources(), "tasks": tasks },
        "nodiscord": { "userId": USER, "member": { "owner": false, "permissions": "32" } },
        "notasks": { "userId": USER, "member": { "owner": false, "permissions": "32" }, "resources": resources() },
    })
}

fn guilds(n: u64) -> (String, String) {
    (format!("9620170000000{:05}", n * 2), format!("9620170000000{:05}", n * 2 + 1))
}

macro_rules! side {
    () => {
        match rust_side().await {
            Some(r) => r,
            None => return,
        }
    };
}

// ── triggers ────────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn triggers_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(1);
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "store": "triggers", "method": "create", "validate": true, "data": { "trigger": "(a+)+", "response": "Hi {user}",
                "channelIds": [CH_TEXT], "exemptRoleIds": [ROLE_QUIET], "cooldown": 10 } },
            // Turning an unsafe pattern into a regex is refused by the store too (not only by the form check).
            { "store": "triggers", "method": "update", "id": "@id0", "data": { "matchType": "REGEX" } },
            { "op": "cacheset", "key": "guild:$G:triggers", "value": [{ "id": "x" }] },
            { "store": "triggers", "method": "update", "id": "@id0", "data": { "enabled": false, "response": "Bye" } },
            { "op": "cache", "key": "guild:$G:triggers" },
            { "store": "triggers", "method": "update", "id": "@id0", "data": { "enabled": true } },
            { "store": "triggers", "method": "update", "id": "@id0", "data": {} },
            { "store": "triggers", "method": "create", "validate": true, "data": { "trigger": "^hello$", "matchType": "REGEX", "response": "Hey", "caseSensitive": true } },
            { "store": "triggers", "method": "create", "data": { "trigger": "(x+)+y", "matchType": "REGEX", "response": "x" } },
            { "store": "triggers", "method": "list" },
            { "store": "triggers", "method": "get", "id": "@id0" },
            { "store": "triggers", "method": "get", "id": "cnotthere000000000000000" },
            { "store": "triggers", "method": "update", "id": "cnotthere000000000000000", "data": { "enabled": false } },
            { "store": "triggers", "method": "remove", "id": "@id7" },
            { "store": "triggers", "method": "remove", "id": "@id7" },
            { "op": "query", "sql": "SELECT \"trigger\", \"matchType\", \"createdBy\", \"enabled\", \"cooldown\" FROM \"AutoTrigger\" WHERE \"guildId\" = $1 ORDER BY \"createdAt\"", "params": ["$G"] },
            // The cap: 99 more rows, then the 101st is refused.
            { "op": "sql", "sql": "INSERT INTO \"AutoTrigger\" (\"id\", \"guildId\", \"trigger\", \"response\", \"createdBy\", \"updatedAt\") SELECT 'cap' || $1 || g, $1, 't' || g, 'x', '1', (now() AT TIME ZONE 'UTC') FROM generate_series(1, 99) g", "params": ["$G"] },
            { "store": "triggers", "method": "create", "validate": true, "data": { "trigger": "more", "response": "x" } },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["triggers"])).await;
}

// ── translateChannels / translateStats ──────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translate_channels_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(2);
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "store": "translateChannels", "method": "create", "validate": true, "data": { "channelId": CH_TEXT, "targetLangs": ["es", "zh-tw"] } },
            { "op": "query", "sql": "SELECT \"translationEnabled\" FROM \"GuildSettings\" WHERE \"guildId\" = $1", "params": ["$G"] },
            { "op": "cache", "key": "guild:$G:translateChannels" },
            { "store": "translateChannels", "method": "create", "data": { "channelId": CH_TEXT, "targetLangs": ["fr"] } },
            { "store": "translateChannels", "method": "create", "data": { "channelId": CH_NEWS, "targetLangs": ["klingon"] } },
            { "store": "translateChannels", "method": "create", "data": { "channelId": CH_NEWS, "targetLangs": [] } },
            { "store": "translateChannels", "method": "create", "data": { "channelId": CH_NEWS, "targetLangs": ["Spanish", "de", "es"] } },
            { "store": "translateChannels", "method": "update", "id": "@id0", "data": { "targetLangs": ["ja", "ko"] } },
            { "op": "cache", "key": "guild:$G:translateChannels" },
            { "store": "translateChannels", "method": "update", "id": "@id0", "data": {} },
            { "store": "translateChannels", "method": "list" },
            { "store": "translateChannels", "method": "remove", "id": "@id6" },
            { "store": "translateChannels", "method": "remove", "id": "@id6" },
            { "store": "translateChannels", "method": "update", "id": "@id6", "data": { "targetLangs": ["es"] } },
            { "op": "cache", "key": "guild:$G:translateChannels" },
            // translateStats reads the counters the bot writes and the configured channels.
            { "op": "sql", "sql": "INSERT INTO \"TranslateStat\" (\"guildId\", \"channelId\", \"via\", \"direction\", \"lang\", \"count\", \"firstAt\", \"lastAt\") VALUES ($1, $2, 'auto', 'from', 'de', 5, '2026-01-02T03:04:05.678', '2026-02-02T03:04:05.678'), ($1, $2, 'auto', 'to', 'ja', 5, '2026-01-02T03:04:05.678', '2026-02-02T03:04:05.678'), ($1, $2, 'command', 'from', 'fr', 2, '2026-01-01T00:00:00', '2026-03-01T00:00:00'), ($1, $3, 'command', 'to', 'es', 1, '2026-01-05T00:00:00', '2026-01-05T00:00:00')", "params": ["$G", CH_TEXT, CH_VOICE] },
            { "store": "translateStats", "method": "list" },
            { "store": "translateStats", "method": "get", "id": CH_TEXT },
            { "store": "translateStats", "method": "get", "id": "955017000000000999" },
            { "store": "translateStats", "method": "create", "data": {} },
            { "store": "translateStats", "method": "update", "id": CH_TEXT, "data": {} },
            { "store": "translateStats", "method": "remove", "id": CH_TEXT },
        ]
    });
    let mut reads = lists(&["translateChannels", "translateStats"]);
    reads.as_array_mut().expect("steps").push(json!({ "op": "cache", "key": "guild:$G:translateChannels" }));
    scenario(&r, &a, &b, &spec, reads).await;
}

// ── shopItems ───────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shop_items_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(3);
    let gem = json!({ "name": "Gem", "price": 5, "type": "ROLE", "roleId": ROLE_PING, "description": "Shiny", "imageUrl": "https://example.com/g.png",
        "useOnBuy": true, "destroyOnUse": false, "canBuyMultiple": false, "maxPerUser": 1, "stock": 4, "buyMessage": "Bought {item}", "useMessage": null, "enabled": true });
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "store": "shopItems", "method": "create", "validate": true, "data": gem },
            { "store": "shopItems", "method": "create", "validate": true, "data": { "name": "  gem ", "price": 1 } },
            { "store": "shopItems", "method": "create", "validate": true, "data": { "name": "Cookie", "price": 1500 } },
            { "store": "shopItems", "method": "create", "data": { "name": "Bad\nName", "price": 1 } },
            // Changing away from Role drops the role, as /economy-admin item edit does.
            { "store": "shopItems", "method": "update", "id": "@id0", "data": { "type": "USABLE" } },
            { "store": "shopItems", "method": "update", "id": "@id0", "data": { "useOnBuy": true, "type": "STATIC" } },
            { "store": "shopItems", "method": "update", "id": "@id0", "data": { "stock": 0, "enabled": false } },
            { "store": "shopItems", "method": "update", "id": "@id2", "data": { "name": "GEM" } },
            { "store": "shopItems", "method": "update", "id": "@id2", "data": { "name": "Biscuit" } },
            { "store": "shopItems", "method": "update", "id": "@id2", "data": {} },
            { "store": "shopItems", "method": "update", "id": "cnotthere000000000000000", "data": { "price": 3 } },
            { "store": "shopItems", "method": "update", "id": "@id0", "validate": true, "data": { "name": "Gem", "price": 5, "type": "STATIC", "roleId": null,
                "description": "Shiny", "imageUrl": "https://example.com/g.png", "useOnBuy": false, "destroyOnUse": false, "canBuyMultiple": false,
                "maxPerUser": 1, "stock": 0, "buyMessage": "Bought {item}", "useMessage": null, "enabled": false } },
            { "store": "shopItems", "method": "list" },
            { "store": "shopItems", "method": "get", "id": "@id2" },
            { "store": "shopItems", "method": "remove", "id": "@id2" },
            { "store": "shopItems", "method": "remove", "id": "@id2" },
            { "op": "query", "sql": "SELECT \"name\", \"price\", \"type\", \"roleId\", \"stock\", \"enabled\", \"useOnBuy\" FROM \"ShopItem\" WHERE \"guildId\" = $1 ORDER BY \"name\"", "params": ["$G"] },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["shopItems"])).await;
}

// ── tempVoiceHubs ───────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temp_voice_hubs_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(4);
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "store": "tempVoiceHubs", "method": "create", "validate": true, "data": { "channelId": CH_VOICE, "categoryId": CAT, "nameFormat": "Room {index}",
                "userLimit": 4, "bitrate": 128, "keepAliveMinutes": 5, "ownershipLockMinutes": 2, "syncPermissions": false, "createTextChannel": true,
                "textChannelMembersOnly": false, "ignoredRoleIds": [ROLE_QUIET], "moderatorRoleIds": [ROLE_PING] } },
            { "store": "tempVoiceHubs", "method": "create", "validate": true, "data": { "channelId": CH_VOICE } },
            { "store": "tempVoiceHubs", "method": "create", "data": { "channelId": CH_TEXT, "nameFormat": "{user}'s room" } },
            { "store": "tempVoiceHubs", "method": "create", "data": { "channelId": CH_TEXT, "nameFormat": "   " } },
            { "op": "sql", "sql": "INSERT INTO \"TempVoiceChannel\" (\"id\", \"channelId\", \"guildId\", \"hubId\", \"ownerId\", \"index\", \"updatedAt\") VALUES ('tv' || $1, $2, $1, $3, $4, 1, (now() AT TIME ZONE 'UTC'))", "params": ["$G", CH_NEWS, "@id0", USER] },
            { "store": "tempVoiceHubs", "method": "create", "ctx": "nodiscord", "data": { "channelId": CH_NEWS } },
            { "store": "tempVoiceHubs", "method": "get", "id": "@id0" },
            { "store": "tempVoiceHubs", "method": "update", "id": "@id0", "data": { "bitrate": 64, "nameFormat": " #{index} {username} ", "userLimit": 0, "categoryId": null } },
            { "store": "tempVoiceHubs", "method": "update", "id": "@id0", "data": { "bitrate": null } },
            { "store": "tempVoiceHubs", "method": "update", "id": "@id0", "data": {} },
            { "store": "tempVoiceHubs", "method": "update", "id": "cnotthere000000000000000", "data": { "userLimit": 3 } },
            { "store": "tempVoiceHubs", "method": "list" },
            { "store": "tempVoiceHubs", "method": "remove", "id": "@id0" },
            { "op": "sql", "sql": "DELETE FROM \"TempVoiceChannel\" WHERE \"guildId\" = $1", "params": ["$G"] },
            { "store": "tempVoiceHubs", "method": "remove", "id": "@id0" },
            { "store": "tempVoiceHubs", "method": "remove", "id": "@id0" },
            { "store": "tempVoiceHubs", "method": "create", "ctx": "nodiscord", "data": { "channelId": CH_VOICE } },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["tempVoiceHubs"])).await;
}

// ── starboards ──────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starboards_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(5);
    let ch = |i: u64| format!("95501700000000{:04}", 5000 + i);
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "store": "starboards", "method": "create", "validate": true, "data": { "name": "best", "channelId": CH_TEXT,
                "emojis": ["⭐", format!("<:blob:{EMOJI_ID}>")], "threshold": 4, "selfStar": false, "removeSelfStars": true, "ignoreBots": false,
                "removeBelowThreshold": false, "autoReact": true, "maxMessageAgeDays": 14, "allowNsfw": false, "ignoredChannelIds": [ch(1), ch(2)], "enabled": true } },
            { "store": "starboards", "method": "create", "validate": true, "data": { "name": "best", "channelId": CH_NEWS } },
            { "store": "starboards", "method": "create", "data": { "name": "Bad Name!", "channelId": CH_NEWS, "emojis": [] } },
            { "store": "starboards", "method": "create", "data": { "name": "x", "channelId": CH_NEWS, "emojis": ["nope"] } },
            // Age-restricted sources need an age-restricted repost channel.
            { "store": "starboards", "method": "create", "validate": true, "data": { "name": "nsfw", "channelId": CH_NEWS, "allowNsfw": true } },
            { "store": "starboards", "method": "create", "validate": true, "data": { "name": "nsfw", "channelId": CH_NSFW, "allowNsfw": true } },
            { "store": "starboards", "method": "create", "validate": true, "ctx": "nodiscord", "data": { "name": "nsfw2", "channelId": CH_NEWS, "allowNsfw": true } },
            { "op": "cacheset", "key": "guild:$G:starboards", "value": [] },
            { "store": "starboards", "method": "update", "id": "@id0", "data": { "ignoredChannelIds": [ch(2), ch(3), ch(4)], "threshold": 2, "name": "Top Posts" } },
            { "op": "cache", "key": "guild:$G:starboards" },
            { "store": "starboards", "method": "update", "id": "@id0", "data": { "selfStar": true } },
            { "store": "starboards", "method": "update", "id": "@id0", "data": { "allowNsfw": true } },
            { "store": "starboards", "method": "update", "id": "@id0", "data": { "name": "nsfw" } },
            { "store": "starboards", "method": "update", "id": "@id0", "data": { "emojis": [] } },
            { "store": "starboards", "method": "update", "id": "@id0", "data": {} },
            { "store": "starboards", "method": "update", "id": "cnotthere000000000000000", "data": { "threshold": 3 } },
            { "op": "sql", "sql": "INSERT INTO \"StarboardEntry\" (\"id\", \"starboardId\", \"sourceChannelId\", \"sourceMessageId\", \"authorId\", \"boardMessageId\", \"count\", \"updatedAt\") VALUES ('e1' || $1, $1, $2, '1', $3, '2', 5, (now() AT TIME ZONE 'UTC')), ('e2' || $1, $1, $2, '3', $3, NULL, 1, (now() AT TIME ZONE 'UTC'))", "params": ["@id0", CH_NEWS, USER] },
            { "store": "starboards", "method": "get", "id": "@id0" },
            { "store": "starboards", "method": "list" },
            { "store": "starboards", "method": "remove", "id": "@id5" },
            { "store": "starboards", "method": "remove", "id": "@id5" },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["starboards"])).await;
}

// ── statChannels ────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stat_channels_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(6);
    let jobs = json!({ "op": "query", "sql": "SELECT \"type\", \"dedupeKey\", \"payload\", \"status\" FROM \"ScheduledJob\" WHERE \"guildId\" = $1 ORDER BY \"dedupeKey\"", "params": ["$G"] });
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "store": "statChannels", "method": "create", "validate": true, "data": { "kind": "MEMBERS", "channelId": CH_VOICE } },
            jobs,
            { "store": "statChannels", "method": "create", "validate": true, "data": { "kind": "MEMBERS", "channelId": CH_NEWS } },
            { "store": "statChannels", "method": "create", "validate": true, "data": { "kind": "BOTS", "channelId": CH_VOICE } },
            { "store": "statChannels", "method": "create", "data": { "kind": "ROLE_MEMBERS", "channelId": CH_NEWS, "roleId": null } },
            { "store": "statChannels", "method": "create", "data": { "kind": "ROLE_MEMBERS", "channelId": CH_NEWS, "roleId": "$G" } },
            { "store": "statChannels", "method": "create", "validate": true, "data": { "kind": "ROLE_MEMBERS", "channelId": CH_NEWS, "roleId": ROLE_PING, "template": null } },
            { "store": "statChannels", "method": "create", "validate": true, "data": { "kind": "ROLE_MEMBERS", "channelId": CH_TEXT, "roleId": ROLE_PING } },
            { "store": "statChannels", "method": "create", "data": { "kind": "ROLES", "channelId": CH_TEXT, "template": "No count here" } },
            { "store": "statChannels", "method": "create", "ctx": "nodiscord", "data": { "kind": "ROLES", "channelId": CH_TEXT, "template": "{server} roles: {count}" } },
            { "store": "statChannels", "method": "update", "id": "@id0", "data": { "template": "People:   {count}" } },
            jobs,
            { "store": "statChannels", "method": "update", "id": "@id0", "data": { "template": null } },
            { "store": "statChannels", "method": "update", "id": "@id0", "data": {} },
            { "store": "statChannels", "method": "update", "id": "@id0", "data": { "template": "{role}: {count}" } },
            { "store": "statChannels", "method": "update", "id": "cnotthere000000000000000", "data": { "template": "A {count}" } },
            { "store": "statChannels", "method": "action", "action": "refresh", "id": "@id0", "data": {} },
            { "op": "settings", "patch": { "statsEnabled": false } },
            { "store": "statChannels", "method": "action", "action": "refresh", "id": "@id0", "data": {} },
            { "store": "statChannels", "method": "action", "action": "refresh", "id": "cnotthere000000000000000", "data": {} },
            { "store": "statChannels", "method": "action", "action": "nope", "id": "@id0", "data": {} },
            { "store": "statChannels", "method": "list" },
            { "store": "statChannels", "method": "remove", "id": "@id0" },
            { "store": "statChannels", "method": "remove", "id": "@id6" },
            { "store": "statChannels", "method": "remove", "id": "@id9" },
            jobs,
            { "store": "statChannels", "method": "remove", "id": "@id9" },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["statChannels"])).await;
}

// ── tags ────────────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tags_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(7);
    let hello_actions = json!([
        { "type": "reply", "responses": [{ "text": "Hi {user}" }, { "text": "Hey {1:you}" }], "pingRoleIds": [ROLE_PING] },
        { "type": "send", "channelId": CH_NEWS, "embed": true, "responses": [{ "text": "logged" }] },
    ]);
    let reply = json!({ "type": "reply", "responses": [{ "text": "Hi {user}" }, { "text": "Hey {1:you}" }], "embed": false, "channelId": null, "roleId": null, "pingRoleIds": [ROLE_PING] });
    let send = json!({ "type": "send", "responses": [{ "text": "logged" }], "embed": true, "channelId": CH_NEWS, "roleId": null, "pingRoleIds": [] });
    let quiet = json!({ "type": "reply", "responses": [{ "text": "Hi {user}" }, { "text": "Hey {1:you}" }], "embed": false, "channelId": null, "roleId": null, "pingRoleIds": [ROLE_QUIET] });
    let give = |role: &str| json!([reply, send, { "type": "addRole", "responses": [], "embed": false, "channelId": null, "roleId": role, "pingRoleIds": [] }]);
    let mut c = ctxs(json!({}));
    c["refuse"] = c["default"].clone();
    c["refuse"]["tasks"]["access.channels"] = json!({ "status": "done", "message": "ok", "data": { "problems": [{ "index": 0, "text": "You need View Channel, Send Messages in #news." }] } });
    let spec = json!({
        "ctxs": c,
        "steps": [
            { "store": "tags", "method": "create", "validate": true, "data": { "name": "hello", "description": "Says hi", "actions": hello_actions,
                "cooldownScope": "USER", "cooldownSeconds": 30, "allowRoleIds": [ROLE_PING] } },
            { "op": "query", "sql": "SELECT \"content\", \"useEmbed\", \"cooldownSeconds\", \"createdBy\", \"actions\" FROM \"CustomTag\" WHERE \"guildId\" = $1", "params": ["$G"] },
            { "store": "tags", "method": "create", "validate": true, "data": { "name": "hello", "actions": hello_actions } },
            { "store": "tags", "method": "create", "data": { "name": "Bad Name", "actions": [] } },
            // Ping roles: a non-mentionable one needs Mention @everyone.
            { "store": "tags", "method": "update", "id": "@id0", "data": { "actions": [quiet, send] } },
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "owner", "data": { "actions": [quiet, send] } },
            { "store": "tags", "method": "update", "id": "@id0", "data": { "actions": [reply, send] } },
            // Role actions: never a role with moderator permissions, never one above the member (when known).
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "plain", "data": { "actions": give(ROLE_PING) } },
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "owner", "data": { "actions": give(ROLE_MOD) } },
            { "store": "tags", "method": "update", "id": "@id0", "data": { "actions": give(ROLE_FLAGGED) } },
            { "store": "tags", "method": "update", "id": "@id0", "data": { "actions": give(ROLE_PING) } },
            // Unchanged roles are never re-checked.
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "plain", "data": { "actions": give(ROLE_PING), "description": "x" } },
            // A new or changed send action needs the user's access to the channel (the bot checks it).
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "refuse", "data": { "actions": [reply, { "type": "send", "responses": [{ "text": "changed" }], "embed": true, "channelId": CH_NEWS, "roleId": null, "pingRoleIds": [] }] } },
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "refuse", "data": { "actions": [reply, send], "enabled": false } },
            { "store": "tags", "method": "update", "id": "@id0", "ctx": "nodiscord", "data": { "actions": [reply, send, send] } },
            // Cooldown: NONE stores 0 and clears the running cooldowns.
            { "op": "cacheset", "key": "tag:cd:$G:@id0", "value": 1 },
            { "op": "cacheset", "key": "tag:cd:$G:@id0:u1", "value": 1 },
            { "store": "tags", "method": "update", "id": "@id0", "data": { "cooldownScope": "NONE" } },
            { "op": "cache", "key": "tag:cd:$G:@id0" },
            { "op": "cache", "key": "tag:cd:$G:@id0:u1" },
            { "store": "tags", "method": "update", "id": "@id0", "data": { "cooldownSeconds": 12 } },
            { "store": "tags", "method": "update", "id": "@id0", "data": { "cooldownScope": "SERVER", "cooldownSeconds": 12 } },
            // A legacy command (actions null) shows its content; unrelated edits keep it legacy.
            { "op": "sql", "sql": "INSERT INTO \"CustomTag\" (\"id\", \"guildId\", \"name\", \"content\", \"useEmbed\", \"createdBy\", \"updatedAt\") VALUES ('legacy' || $1, $1, 'legacy', 'old text', true, $2, (now() AT TIME ZONE 'UTC'))", "params": ["$G", USER] },
            { "store": "tags", "method": "get", "id": "legacy$G" },
            { "store": "tags", "method": "update", "id": "legacy$G", "data": { "enabled": false } },
            { "op": "query", "sql": "SELECT \"actions\", \"content\", \"useEmbed\", \"enabled\" FROM \"CustomTag\" WHERE \"id\" = $1", "params": ["legacy$G"] },
            { "store": "tags", "method": "update", "id": "legacy$G", "data": { "actions": [{ "type": "reply", "responses": [{ "text": "new text" }], "embed": true, "channelId": null, "roleId": null, "pingRoleIds": [] }] } },
            { "op": "query", "sql": "SELECT \"actions\", \"content\", \"useEmbed\", \"enabled\" FROM \"CustomTag\" WHERE \"id\" = $1", "params": ["legacy$G"] },
            { "store": "tags", "method": "update", "id": "legacy$G", "data": { "actions": [{ "type": "addRole", "responses": [], "embed": false, "channelId": null, "roleId": ROLE_PING, "pingRoleIds": [] }] } },
            { "op": "query", "sql": "SELECT \"actions\", \"content\", \"useEmbed\" FROM \"CustomTag\" WHERE \"id\" = $1", "params": ["legacy$G"] },
            { "store": "tags", "method": "list" },
            // What a store returns is accepted back as an update and changes nothing.
            { "store": "tags", "method": "update", "id": "@id0", "validate": true, "data": { "description": "x", "enabled": true, "actions": give(ROLE_PING),
                "allowRoleIds": [ROLE_PING], "denyRoleIds": [], "allowChannelIds": [], "denyChannelIds": [], "cooldownScope": "SERVER", "cooldownSeconds": 12, "deleteInvocation": false } },
            { "store": "tags", "method": "remove", "id": "legacy$G" },
            { "store": "tags", "method": "remove", "id": "legacy$G" },
            { "store": "tags", "method": "update", "id": "legacy$G", "data": { "enabled": true } },
            { "store": "tags", "method": "create", "ctx": "nodiscord", "data": { "name": "pings", "actions": [{ "type": "reply", "responses": [{ "text": "x" }], "pingRoleIds": [ROLE_PING] }] } },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["tags"])).await;
}

// ── reminders ───────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reminders_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(8);
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| bot_db::format_js_date(&chrono::DateTime::from_timestamp_millis(ms).expect("time"));
    let run_at = iso((now + 3 * 3_600_000) / 60_000 * 60_000);
    let past = iso(now - 60_000);
    let jobs = json!({ "op": "query", "sql": "SELECT \"type\", \"dedupeKey\", \"payload\", \"status\", to_char(\"runAt\", 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS \"runAt\" FROM \"ScheduledJob\" WHERE \"guildId\" = $1 ORDER BY \"dedupeKey\"", "params": ["$G"] });
    let spec = json!({
        "ctxs": ctxs(json!({})),
        "steps": [
            { "op": "settings", "patch": { "timezone": "Europe/Berlin" } },
            { "store": "reminders", "method": "create", "validate": true, "data": { "name": "standup", "channelId": CH_TEXT, "timeOfDay": "09:30", "pingRoleId": ROLE_PING } },
            jobs,
            { "store": "reminders", "method": "create", "validate": true, "data": { "name": "standup", "channelId": CH_TEXT } },
            { "store": "reminders", "method": "create", "validate": true, "data": { "name": "quiet", "channelId": CH_TEXT, "pingRoleId": ROLE_QUIET } },
            { "store": "reminders", "method": "create", "data": { "name": "Bad Name", "channelId": CH_TEXT, "mode": "TIME_OF_DAY", "timeOfDay": "10:00" } },
            // Pause cancels the job; resume brings it back.
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "enabled": false } },
            jobs,
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "enabled": true } },
            jobs,
            // Interval with a daily window; then once.
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "INTERVAL", "intervalMinutes": 30, "windowStart": "08:00", "windowEnd": "18:00" } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "INTERVAL", "intervalMinutes": 1440, "windowStart": "08:00" } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "INTERVAL", "intervalMinutes": 2, "windowStart": null, "windowEnd": null } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "ONE_SHOT", "runAt": run_at } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "runAt": past } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "TIME_OF_DAY", "timeOfDay": "07:00", "weekdays": [] } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "TIME_OF_DAY", "timeOfDay": "07:00", "weekdays": ["sun", "mon"] } },
            // A finished one-off: saving the whole form with only a new channel works; turning it back on needs a new time.
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "ONE_SHOT", "runAt": run_at } },
            { "op": "sql", "sql": "UPDATE \"Reminder\" SET \"enabled\" = false, \"runAt\" = $2::timestamp, \"lastSentAt\" = $3::timestamp WHERE \"id\" = $1", "params": ["@id1", iso(now - 86_400_000).trim_end_matches('Z'), iso(now).trim_end_matches('Z')] },
            { "store": "reminders", "method": "get", "id": "@id1" },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "channelId": CH_NEWS, "template": { "content": "Hey, I'm a reminder!", "embed": null, "buttons": [] },
                "mode": "ONE_SHOT", "timeOfDay": null, "weekdays": ["sun", "mon"], "intervalMinutes": 15, "windowStart": null, "windowEnd": null,
                "runAt": iso(now - 86_400_000), "requireActivity": false, "pingRoleId": ROLE_PING, "enabled": false } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "enabled": true } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "mode": "TIME_OF_DAY", "timeOfDay": "07:00", "enabled": true } },
            jobs,
            // Test post: in the reminder's channel, the ping role shown but not pinged; the schedule is untouched.
            { "store": "reminders", "method": "action", "action": "test", "id": "@id1", "data": { "channelId": null } },
            { "store": "reminders", "method": "action", "action": "test", "id": "@id1", "data": { "channelId": "955017000000000999" } },
            { "store": "reminders", "method": "action", "action": "test", "id": "@id1", "ctx": "nodiscord", "data": {} },
            { "store": "reminders", "method": "action", "action": "test", "id": "@id1", "ctx": "send403", "data": { "channelId": CH_TEXT } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "pingRoleId": ROLE_QUIET } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "pingRoleId": null } },
            // A deleted channel keeps a paused reminder paused.
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "enabled": false, "channelId": "955017000000000999" } },
            { "store": "reminders", "method": "update", "id": "@id1", "data": { "enabled": true } },
            { "store": "reminders", "method": "list" },
            { "store": "reminders", "method": "remove", "id": "@id1" },
            jobs,
            { "store": "reminders", "method": "remove", "id": "@id1" },
            { "store": "reminders", "method": "create", "validate": true, "data": { "name": "every", "channelId": CH_TEXT, "mode": "INTERVAL", "intervalMinutes": 60, "enabled": false } },
            jobs,
        ]
    });
    let mut spec = spec;
    spec["ctxs"]["send403"] = spec["ctxs"]["default"].clone();
    spec["ctxs"]["send403"]["discordFail"] = json!({ "sendMessage": { "message": "Missing Permissions", "status": 403, "code": "forbidden", "discordCode": 50013 } });
    let (_, rs) = scenario(&r, &a, &b, &spec, lists(&["reminders"])).await;
    // The job of an active reminder runs when the item says it runs next, at 09:30 in the server's time zone.
    let next = rs[1]["saved"]["item"]["nextRunAt"].as_str().expect("nextRunAt").to_string();
    assert_eq!(rs[2]["out"][0]["runAt"].as_str(), Some(next.as_str()));
    let at = chrono::DateTime::parse_from_rfc3339(&next).expect("ISO").with_timezone(&chrono_tz::Europe::Berlin);
    assert_eq!(at.format("%H:%M").to_string(), "09:30");
    assert_eq!(rs[23]["out"][0]["runAt"], rs[22]["saved"]["item"]["nextRunAt"], "the job follows the resumed schedule");
}

// ── reactionRoles ───────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaction_roles_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(9);
    let item = |role: &str, label: &str, emoji: Value| json!({ "roleId": role, "label": label, "emoji": emoji, "style": 1, "description": null });
    let mut c = ctxs(json!({ "rr.publish": { "status": "done", "message": "Posted in #general.", "data": null },
        "rr.repost": { "status": "done", "message": "Reposted in #news", "data": null },
        "rr.roleTemplate": { "status": "done", "message": "Created 3 roles and posted the panel.", "data": null } }));
    c["failing"] = c["default"].clone();
    c["failing"]["tasks"]["rr.publish"] = json!({ "status": "failed", "message": "I can't post in #general: I need Send Messages there.", "data": null });
    c["failing"]["tasks"]["rr.roleTemplate"] = json!({ "status": "offline", "message": "The bot is offline", "data": null });
    c["pending"] = c["default"].clone();
    c["pending"]["tasks"]["rr.roleTemplate"] = json!({ "status": "pending", "message": "Still working", "data": null });
    let count = json!({ "op": "query", "sql": "SELECT \"title\", \"messageId\", \"type\", \"layout\", \"maxRoles\", \"placeholder\", (SELECT COUNT(*) FROM \"ReactionRoleItem\" i WHERE i.\"groupId\" = g.\"id\") AS items FROM \"ReactionRoleGroup\" g WHERE \"guildId\" = $1 ORDER BY \"createdAt\"", "params": ["$G"] });
    let spec = json!({
        "ctxs": c,
        "steps": [
            { "store": "reactionRoles", "method": "create", "validate": true, "data": { "channelId": CH_TEXT, "title": "  Pick roles ", "type": "NORMAL", "layout": "BUTTONS", "maxRoles": 2,
                "items": [item(ROLE_PING, "", json!("⭐")), item(ROLE_QUIET, "Quiet", json!(format!("<:blob:{EMOJI_ID}>")))] } },
            count,
            { "op": "cacheset", "key": "guild:$G:rrpanels", "value": {} },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "data": { "title": "New title", "type": "UNIQUE" } },
            { "op": "cache", "key": "guild:$G:rrpanels" },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "data": { "items": [item(ROLE_QUIET, "Quiet", json!(null)), item(ROLE_FLAGGED, "Helpers", json!("🔥"))] } },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "data": { "title": "New title" } },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "data": { "items": [item(ROLE_QUIET, "Quiet", json!("not an emoji"))] } },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "data": { "items": [item(ROLE_QUIET, "Quiet", json!("<:gone:955017000000000041>"))] } },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "ctx": "nodiscord", "data": { "title": "x" } },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "ctx": "notasks", "data": { "title": "x" } },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "ctx": "failing", "data": { "description": "Now with text" } },
            { "store": "reactionRoles", "method": "action", "action": "repost", "id": "@id0", "data": { "channelId": CH_NEWS } },
            { "store": "reactionRoles", "method": "action", "action": "repost", "id": "cnotthere000000000000000", "data": {} },
            // A dropdown panel stores the placeholder and descriptions; other layouts drop what they do not use.
            { "store": "reactionRoles", "method": "create", "validate": true, "data": { "channelId": CH_TEXT, "title": "Menu", "layout": "SELECT", "type": "NORMAL", "placeholder": "Choose",
                "items": [{ "roleId": ROLE_PING, "label": "Pings", "emoji": null, "description": "  get pinged  " }] } },
            { "store": "reactionRoles", "method": "update", "id": "@id14", "data": { "placeholder": "Pick one", "maxRoles": 1 } },
            { "store": "reactionRoles", "method": "create", "data": { "channelId": CH_TEXT, "title": "  ", "layout": "REACTIONS", "type": "NORMAL",
                "items": [item(ROLE_PING, "Pings", json!("⭐")), item(ROLE_QUIET, "Quiet", json!("🔥"))] } },
            { "store": "reactionRoles", "method": "update", "id": "@id16", "data": { "items": [item(ROLE_PING, "Pings", json!("✅"))] } },
            // A panel the bot cannot post stays "Not posted yet".
            { "store": "reactionRoles", "method": "create", "validate": true, "ctx": "failing", "data": { "channelId": CH_NEWS, "title": "Broken", "items": [item(ROLE_PING, "P", json!(null))] } },
            { "store": "reactionRoles", "method": "create", "data": { "channelId": CH_NEWS, "title": "x", "type": "NORMAL", "layout": "BUTTONS", "items": [] }, "ctx": "notasks" },
            // Role templates: the bot creates the roles; a refusal deletes the empty panel again.
            { "store": "reactionRoles", "method": "create", "validate": true, "data": { "channelId": CH_TEXT, "title": "Reaction Roles", "template": "pronouns" } },
            { "store": "reactionRoles", "method": "create", "validate": true, "ctx": "pending", "data": { "channelId": CH_TEXT, "title": "Mine", "template": "colors" } },
            { "store": "reactionRoles", "method": "create", "validate": true, "ctx": "failing", "data": { "channelId": CH_TEXT, "title": "Reaction Roles", "template": "pronouns" } },
            count,
            { "store": "reactionRoles", "method": "list" },
            // Never posted: deleting needs no bot. Posted: the bot deletes it (rr.delete).
            { "store": "reactionRoles", "method": "remove", "id": "@id18", "ctx": "notasks" },
            { "op": "sql", "sql": "UPDATE \"ReactionRoleGroup\" SET \"messageId\" = '955017000000000777', \"messageMissingAt\" = NULL WHERE \"id\" = $1", "params": ["@id0"] },
            { "store": "reactionRoles", "method": "update", "id": "@id0", "data": { "plainText": true } },
            { "store": "reactionRoles", "method": "remove", "id": "@id0", "ctx": "notasks" },
            { "store": "reactionRoles", "method": "remove", "id": "@id0" },
            { "store": "reactionRoles", "method": "remove", "id": "cnotthere000000000000000" },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["reactionRoles"])).await;
}

// ── ticketPanels ────────────────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_panels_store_matches_js() {
    let r = side!();
    let (a, b) = guilds(10);
    let mut c = ctxs(json!({ "ticket.publish": { "status": "done", "message": "Published in #news.", "data": null },
        "ticket.refresh": { "status": "done", "message": "Updated the panel.", "data": null },
        "ticket.check": { "status": "done", "message": "ok", "data": null } }));
    c["failing"] = c["default"].clone();
    c["failing"]["tasks"]["ticket.publish"] = json!({ "status": "failed", "message": "I need Send Messages in #news.", "data": null });
    c["failing"]["tasks"]["ticket.check"] = json!({ "status": "failed", "message": "I can't use the category Support: I need Manage Channels.", "data": null });
    c["pending"] = c["default"].clone();
    c["pending"]["tasks"]["ticket.check"] = json!({ "status": "pending", "message": "Still checking", "data": null });
    let option = |label: &str, emoji: Value, extra: Value| {
        let mut o = json!({ "id": null, "label": label, "emoji": emoji, "description": null, "buttonStyle": 1, "categoryId": null, "introTemplate": { "content": null, "embed": null, "buttons": [] } });
        for (k, v) in extra.as_object().into_iter().flatten() {
            o[k] = v.clone();
        }
        o
    };
    let intro = json!({ "content": "{managers} a new ticket", "embed": null, "buttons": [] });
    let rows = json!({ "op": "query", "sql": "SELECT p.\"name\", p.\"template\", p.\"introTemplate\", p.\"managerRoleIds\", p.\"channelNameFormat\", (SELECT json_agg(json_build_object('label', o.\"label\", 'emoji', o.\"emoji\", 'position', o.\"position\", 'categoryId', o.\"categoryId\", 'introTemplate', o.\"introTemplate\") ORDER BY o.\"position\") FROM \"TicketPanelOption\" o WHERE o.\"panelId\" = p.\"id\") AS options FROM \"TicketPanel\" p WHERE p.\"guildId\" = $1 ORDER BY p.\"name\"", "params": ["$G"] });
    let spec = json!({
        "ctxs": c,
        "steps": [
            { "store": "ticketPanels", "method": "create", "validate": true, "data": { "name": "Support" } },
            rows,
            { "store": "ticketPanels", "method": "create", "validate": true, "data": { "name": "support" } },
            { "store": "ticketPanels", "method": "create", "validate": true, "data": { "name": "Sales", "channelId": CH_NEWS, "style": "SELECT", "managerRoleIds": [ROLE_PING, ROLE_PING, "955017000000000888"],
                "template": { "content": null, "embed": { "title": "Sales desk", "description": "Ask us" }, "buttons": [{ "label": "Site", "url": "https://example.com" }] },
                "introTemplate": intro, "openCategoryId": CAT,
                "options": [option("Buy", json!("🛒"), json!({ "description": " buying things " })), option("Refund", json!(format!("<:blob:{EMOJI_ID}>")), json!({ "categoryId": CAT, "introTemplate": { "content": "Refund {user}", "embed": null, "buttons": [] } }))] } },
            rows,
            { "store": "ticketPanels", "method": "create", "validate": true, "ctx": "plain", "data": { "name": "Pingy", "managerRoleIds": [ROLE_QUIET], "introTemplate": intro } },
            { "store": "ticketPanels", "method": "create", "validate": true, "ctx": "failing", "data": { "name": "Cats", "openCategoryId": CAT } },
            { "store": "ticketPanels", "method": "create", "validate": true, "ctx": "pending", "data": { "name": "Cats", "transcriptChannelId": CH_TEXT } },
            { "store": "ticketPanels", "method": "create", "validate": true, "ctx": "failing", "data": { "name": "Unpublished", "channelId": CH_NEWS } },
            { "store": "ticketPanels", "method": "create", "data": { "name": "Bad", "channelNameFormat": "{nope}", "options": [] } },
            { "store": "ticketPanels", "method": "create", "data": { "name": "Bad", "channelNameFormat": "t-{number}", "options": [option("A", json!("not emoji"), json!({}))], "managerRoleIds": [] } },
            { "store": "ticketPanels", "method": "create", "ctx": "nodiscord", "data": { "name": "NoDiscord", "channelNameFormat": "t-{number}", "options": [], "managerRoleIds": [] } },
            // A failed create leaves nothing behind (the panel is deleted again).
            { "store": "ticketPanels", "method": "create", "data": { "name": "Empty", "channelNameFormat": "t-{number}", "options": [], "managerRoleIds": [] } },
            rows,
            // Editing settings needs no task; a changed message updates the published panel.
            { "store": "ticketPanels", "method": "update", "id": "@id0", "data": { "askReason": true, "maxOpenPerUser": 3, "channelNameFormat": "help-{number}" } },
            { "store": "ticketPanels", "method": "update", "id": "@id0", "data": { "name": "SALES" } },
            { "store": "ticketPanels", "method": "update", "id": "@id0", "ctx": "notasks", "data": { "askReason": false } },
            { "store": "ticketPanels", "method": "update", "id": "@id0", "data": { "options": [] } },
            // Options are matched by id, then by label.
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "options": [option("refund", json!("💸"), json!({})), option("Buy", json!("🛒"), json!({ "categoryId": CAT })), option("New", json!(null), json!({}))] } },
            rows,
            { "op": "sql", "sql": "UPDATE \"TicketPanel\" SET \"channelId\" = $2, \"messageId\" = '955017000000000778' WHERE \"id\" = $1", "params": ["@id3", CH_NEWS] },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "style": "BUTTONS" } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "managerRoleIds": [] } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "managerRoleIds": [ROLE_PING, ROLE_QUIET] } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "options": [option("refund", json!("💸"), json!({ "categoryId": CAT })), option("Buy", json!("🛒"), json!({ "categoryId": CAT })), option("New", json!(null), json!({}))] } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "channelId": CH_TEXT, "transcriptChannelId": CH_TEXT, "closedCategoryId": CAT } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "ctx": "failing", "data": { "channelId": CH_TEXT } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "template": { "content": null, "embed": { "title": "How can we help?", "description": "Press a button below to open a private ticket with our team." }, "buttons": [] } } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": { "introTemplate": { "content": "", "embed": null, "buttons": [] } } },
            { "store": "ticketPanels", "method": "update", "id": "@id3", "data": {} },
            { "store": "ticketPanels", "method": "action", "action": "publish", "id": "@id3", "data": { "channelId": CH_NEWS } },
            { "store": "ticketPanels", "method": "action", "action": "refresh", "id": "@id3", "data": {} },
            { "store": "ticketPanels", "method": "action", "action": "refresh", "id": "cnotthere000000000000000", "data": {} },
            { "op": "sql", "sql": "INSERT INTO \"Ticket\" (\"id\", \"guildId\", \"panelId\", \"channelId\", \"ownerId\", \"number\", \"status\", \"updatedAt\") VALUES ('t1' || $1, $1, $2, 't1' || $1, $3, 1, 'OPEN', (now() AT TIME ZONE 'UTC')), ('t2' || $1, $1, $2, 't2' || $1, $3, 2, 'DELETED', (now() AT TIME ZONE 'UTC'))", "params": ["$G", "@id3", USER] },
            { "store": "ticketPanels", "method": "get", "id": "@id3" },
            { "store": "ticketPanels", "method": "list" },
            { "store": "ticketPanels", "method": "remove", "id": "@id0", "ctx": "notasks" },
            { "store": "ticketPanels", "method": "remove", "id": "@id3", "ctx": "notasks" },
            { "store": "ticketPanels", "method": "remove", "id": "@id3" },
            { "store": "ticketPanels", "method": "remove", "id": "@id0" },
        ]
    });
    scenario(&r, &a, &b, &spec, lists(&["ticketPanels"])).await;
}

// ── Raw statements and registration ─────────────────────────────────────────────────────────────────────────────────

/// Every raw statement the stores export is valid SQL (test/db.test.js runs each one in a rolled-back transaction).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_statements_run() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let all = dash_records_b::reminders_store::RAW_SQL
        .iter()
        .chain(dash_records_b::reaction_roles_store::RAW_SQL.iter())
        .chain(dash_records_b::ticket_panels_store::RAW_SQL.iter());
    for (name, sql, bind) in all {
        let mut tx = pool.begin().await.expect("tx");
        sqlx::query(*sql).bind(*bind).execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
    }
    // levelRoles.store.js lockGuildSettingsSql, used by statChannels.
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query(bot_db::tx::LOCK_GUILD_SETTINGS_SQL).bind("0").execute(&mut *tx).await.expect("lockGuildSettings");
    tx.rollback().await.expect("rollback");
}

// ── Caps under concurrency (the locks the stores take) ──────────────────────────────────────────────────────────────

/// Creates five items at once through the Rust store; answers how many were created and the error codes of the rest.
async fn race(r: &Rust, key: &str, guild: &str, ctx: &Value, data: impl Fn(usize) -> Value) -> (usize, Vec<String>) {
    let (c, _, _) = ctx_of(&fill(ctx, guild, &[]), guild);
    let store = r.store(key).clone();
    let futures: Vec<_> = (0..5)
        .map(|i| {
            let (store, c, d, g) = (store.clone(), c.clone(), fill(&data(i), guild, &[]), guild.to_string());
            tokio::spawn(async move { store.create(&g, d.as_object().cloned().unwrap_or_default(), &c).await })
        })
        .collect();
    let mut ok = 0;
    let mut codes = Vec::new();
    for f in futures {
        match f.await.expect("task") {
            Ok(_) => ok += 1,
            Err(StoreError::Record(e)) => codes.push(e.code.as_str().to_string()),
            Err(e) => panic!("{key}: unexpected {e:?}"),
        }
    }
    (ok, codes)
}

/// The per-server caps hold when creates race (test/dashboard-panels.test.js and dashboard-collections-more.test.js).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caps_hold_when_creates_race() {
    let r = side!();
    let (g, _) = guilds(20);
    let c = ctxs(json!({ "rr.publish": { "status": "done", "message": "Posted.", "data": null } }))["default"].clone();
    let fill_sql = |sql: &str| sql.replace("$G", &g);
    r.t.reset_guilds(&[&g]).await;
    r.t.app.settings().ensure_guild_settings(&g).await.expect("settings");

    // reactionRoles: LIMITS.REACTION_ROLE_PANELS.
    let cap = bot_config::tables::limits::REACTION_ROLE_PANELS as i64;
    let sql = format!(
        r#"INSERT INTO "ReactionRoleGroup" ("id", "guildId", "channelId", "messageId", "title", "updatedAt") SELECT 'rr' || g, '$G', '1', 'm$G' || g, 't', (now() AT TIME ZONE 'UTC') FROM generate_series(1, {}) g"#,
        cap - 2
    );
    r.sql(&fill_sql(&sql), &[]).await.expect("seed");
    let (ok, codes) =
        race(&r, "reactionRoles", &g, &c, |i| json!({ "channelId": CH_TEXT, "title": format!("P{i}"), "type": "NORMAL", "layout": "BUTTONS", "items": [] })).await;
    assert_eq!((ok, codes.iter().all(|c| c == "limit"), codes.len()), (2, true, 3), "reactionRoles");

    // reminders: LIMITS.REMINDERS.
    let cap = bot_config::tables::limits::REMINDERS as i64;
    let sql = format!(
        r#"INSERT INTO "Reminder" ("id", "guildId", "name", "channelId", "template", "mode", "timeOfDay", "enabled", "createdBy", "updatedAt") SELECT 're$G' || g, '$G', 'r' || g, '1', '{{}}'::jsonb, 'TIME_OF_DAY', '09:00', false, '1', (now() AT TIME ZONE 'UTC') FROM generate_series(1, {}) g"#,
        cap - 2
    );
    r.sql(&fill_sql(&sql), &[]).await.expect("seed");
    let (ok, codes) = race(&r, "reminders", &g, &c, |i| {
        json!({ "name": format!("race{i}"), "channelId": CH_TEXT, "mode": "TIME_OF_DAY", "timeOfDay": "10:00", "template": "Hi", "enabled": false })
    })
    .await;
    assert_eq!((ok, codes.iter().all(|c| c == "limit"), codes.len()), (2, true, 3), "reminders");

    // statChannels: at most LIMITS.ROLE_STAT_CHANNELS role counters.
    let role_cap = bot_config::tables::limits::ROLE_STAT_CHANNELS as i64;
    let sql = format!(
        r#"INSERT INTO "StatChannel" ("id", "guildId", "channelId", "kind", "roleId", "template", "updatedAt") SELECT 'sr$G' || g, '$G', '$G' || (200 + g), 'ROLE_MEMBERS', (700000000000000000 + g)::text, '{{role}} {{count}}', (now() AT TIME ZONE 'UTC') FROM generate_series(1, {}) g"#,
        role_cap - 2
    );
    r.sql(&fill_sql(&sql), &[]).await.expect("seed");
    let (ok, codes) = race(&r, "statChannels", &g, &c, |i| {
        json!({ "kind": "ROLE_MEMBERS", "channelId": format!("$G{}", 300 + i), "roleId": format!("{}", 800_000_000_000_000_000u64 + i as u64) })
    })
    .await;
    assert_eq!((ok, codes.iter().all(|c| c == "limit"), codes.len()), (2, true, 3), "statChannels (role counters)");

    // starboards: LIMITS.STARBOARDS.
    let cap = bot_config::tables::limits::STARBOARDS as i64;
    let sql = format!(
        r#"INSERT INTO "Starboard" ("id", "guildId", "name", "channelId", "updatedAt") SELECT 'sb$G' || g, '$G', 'b' || g, '1', (now() AT TIME ZONE 'UTC') FROM generate_series(1, {}) g"#,
        cap - 2
    );
    r.sql(&fill_sql(&sql), &[]).await.expect("seed");
    let (ok, codes) = race(&r, "starboards", &g, &c, |i| json!({ "name": format!("race{i}"), "channelId": CH_TEXT, "emojis": [] })).await;
    assert_eq!((ok, codes.iter().all(|c| c == "limit"), codes.len()), (2, true, 3), "starboards");
    r.t.reset_guilds(&[&g]).await;
}

// ── What a store returns is accepted back as an update (the editor sends the whole form) ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stores_accept_their_own_items_back() {
    let r = side!();
    let (g, _) = guilds(21);
    r.t.reset_guilds(&[&g]).await;
    let c = ctxs(json!({ "rr.publish": { "status": "done", "message": "", "data": null } }));
    let cases = [
        ("triggers", json!({ "trigger": "hello", "response": "Hi", "channelIds": [CH_TEXT], "cooldown": 5 })),
        ("translateChannels", json!({ "channelId": CH_TEXT, "targetLangs": ["es", "zh-tw"] })),
        ("tempVoiceHubs", json!({ "channelId": CH_VOICE, "categoryId": CAT, "nameFormat": "Room {index}", "userLimit": 4, "bitrate": 128 })),
        ("statChannels", json!({ "kind": "ROLE_MEMBERS", "channelId": CH_NEWS, "roleId": ROLE_PING, "template": null })),
        ("starboards", json!({ "name": "best", "channelId": CH_TEXT, "emojis": ["⭐"], "ignoredChannelIds": [CH_NEWS] })),
        ("shopItems", json!({ "name": "Gem", "price": 5, "type": "ROLE", "roleId": ROLE_PING, "stock": 4 })),
        ("tags", json!({ "name": "hello", "actions": [{ "type": "reply", "responses": [{ "text": "Hi" }], "pingRoleIds": [ROLE_PING] }] })),
        ("reminders", json!({ "name": "standup", "channelId": CH_TEXT, "timeOfDay": "09:30", "pingRoleId": ROLE_PING })),
        (
            "reactionRoles",
            json!({ "channelId": CH_TEXT, "title": "Roles", "layout": "SELECT", "placeholder": "Pick",
                "items": [{ "roleId": ROLE_PING, "label": "Pings", "emoji": "⭐", "description": "d" }] }),
        ),
        ("ticketPanels", json!({ "name": "Support", "managerRoleIds": [ROLE_PING], "openCategoryId": null })),
    ];
    for (key, data) in cases {
        let steps = json!({ "ctxs": c, "steps": [
            { "store": key, "method": "create", "validate": true, "data": data },
            { "store": key, "method": "get", "id": "@id0" },
        ] });
        let out = r.run(&steps, &g).await;
        let item = out[1]["out"].clone();
        assert!(item.is_object(), "{key}: {}", out[0]);
        let col = Collections::builtin().get(key).expect("collection");
        let mut form = Map::new();
        for f in &col.fields {
            let name = f.field();
            if !col.create_only.iter().any(|x| x == name) {
                form.insert(name.to_string(), item.get(name).cloned().unwrap_or(Value::Null));
            }
        }
        let valid = col_validate(key, &Value::Object(form.clone()), &item);
        assert_eq!(Value::Object(valid.clone()), Value::Object(form.clone()), "{key}: the validated form equals what was sent");
        let (ctx, _, _) = ctx_of(&fill(&c["default"], &g, &[]), &g);
        let saved = r.store(key).update(&g, item["id"].as_str().unwrap_or(""), valid, &ctx).await.unwrap_or_else(|e| panic!("{key}: {e:?}"));
        // (updatedAt moves where the JS store writes the unchanged fields too, as triggers and tags do.)
        let strip = |v: &Value| {
            let mut v = v.clone();
            if let Some(m) = v.as_object_mut() {
                m.remove("updatedAt");
            }
            v
        };
        assert_eq!(strip(&saved.item), strip(&item), "{key}: saving an unchanged form changes nothing");
        for f in &col.read_only {
            assert!(item.get(f.field()).is_some(), "{key}: item has read-only {}", f.field());
        }
    }
    r.t.reset_guilds(&[&g]).await;
}

fn col_validate(key: &str, form: &Value, current: &Value) -> Map<String, Value> {
    Collections::builtin()
        .validate_record_input(key, form, Mode::Update, None, Some(current), chrono::Utc::now().timestamp_millis() as f64)
        .unwrap_or_else(|e| panic!("{key}: {}", e.to_json()))
        .data
}
