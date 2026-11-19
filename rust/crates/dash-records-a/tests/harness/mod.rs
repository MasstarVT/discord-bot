//! The Rust side of the cross-language scenarios (the JS side is rust/fixtures/gen/dash-records-a-js.mjs): the same
//! spec runs through the JS stores on one guild and through the Rust stores on another, and the answers are compared
//! after the guild ids, cuids and timestamps are normalised. Then each side reads back what the other wrote.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::App;
use bot_core::testing::{DbTestApp, json_diff, run_node};
use dash_collections::testing::FakeDiscord;
use dash_collections::{
    DashboardDiscord, DashboardMember, DashboardUser, DiscordApiError, DiscordResources, ListOpts, MemberProfile, ReadCtx, StoreCtx,
    StoreError, TaskOpts, TaskResult, TaskRunner,
};
use regex::Regex;
use serde_json::{Map, Value, json};

/// The test app (PostgreSQL and Redis), or None when the variables are not set.
pub async fn app() -> Option<DbTestApp> {
    let db = bot_core::testing::database_url()?;
    let redis = bot_core::testing::redis_url()?;
    Some(DbTestApp::with_redis(&db, &redis).await)
}

/// Runs the JS script with a spec written to a temporary file.
pub fn node(cmd: &str, spec: &Value) -> Value {
    let dir = std::env::temp_dir().join("dash-records-a");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = dir.join(format!("spec-{}-{n}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string(spec).expect("spec")).expect("write spec");
    let out = run_node("rust/fixtures/gen/dash-records-a-js.mjs", &[cmd, path.to_str().expect("utf8 path")]);
    let _ = std::fs::remove_file(&path);
    if let Some(e) = out.get("error") {
        panic!("the JS side failed: {e}");
    }
    out
}

/// Replaces $G, $O, $S<n> and $ID<n> in every string.
pub fn subst(v: &Value, vars: &HashMap<String, String>) -> Value {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\$(ID\d+|S\d+|C\d+|G|O)").expect("regex"));
    match v {
        Value::String(s) => Value::String(re.replace_all(s, |c: &regex::Captures| vars.get(&c[1]).cloned().unwrap_or_else(|| c[0].to_string())).into_owned()),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst(x, vars)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), subst(x, vars))).collect()),
        other => other.clone(),
    }
}

/// The task runner of the scenarios: answers `spec.tasks[kind]` (substituted when called), records every call.
pub struct ScriptTasks {
    pub answers: Mutex<Map<String, Value>>,
    pub vars: Arc<Mutex<HashMap<String, String>>>,
    pub calls: Mutex<Vec<Value>>,
}

#[async_trait]
impl TaskRunner for ScriptTasks {
    async fn run(&self, guild_id: &str, kind: &str, payload: Value, opts: TaskOpts) -> anyhow::Result<TaskResult> {
        self.calls.lock().unwrap().push(json!({ "guildId": guild_id, "kind": kind, "payload": payload, "actorId": opts.actor_id, "waitMs": opts.wait_ms }));
        let answer = self.answers.lock().unwrap().get(kind).cloned();
        match answer {
            Some(a) => {
                if let Some(t) = a.get("throw") {
                    anyhow::bail!("{}", t.as_str().unwrap_or("failed"));
                }
                let a = subst(&a, &self.vars.lock().unwrap());
                Ok(serde_json::from_value(a)?)
            }
            None => Ok(TaskResult::done("Done.", None)),
        }
    }
}

fn api_error(e: &Value) -> DiscordApiError {
    let mut err = DiscordApiError::new(
        e["message"].as_str().unwrap_or(""),
        e.get("status").and_then(Value::as_u64).unwrap_or(0) as u16,
        e["code"].as_str().unwrap_or(""),
    );
    if let Some(c) = e.get("discordCode").and_then(Value::as_i64) {
        err = err.with_discord_code(c);
    }
    err.retry_after = e.get("retryAfter").and_then(Value::as_u64);
    err
}

/// The fake Discord of a spec (None without `discord`).
pub fn fake_discord(spec: &Value, guilds: &[&str]) -> Option<Arc<FakeDiscord>> {
    let cfg = spec.get("discord").filter(|d| !d.is_null())?;
    let fake = FakeDiscord::new();
    if let Some(r) = cfg.get("resources") {
        for g in guilds {
            let mut r = r.clone();
            r["guild"]["id"] = json!(g);
            let res: DiscordResources = serde_json::from_value(r).expect("resources");
            fake.set_resources(g, res);
        }
    }
    for u in cfg.get("users").and_then(Value::as_array).into_iter().flatten() {
        fake.set_user(serde_json::from_value::<MemberProfile>(u.clone()).expect("profile"));
    }
    if let Some(m) = cfg.get("members").and_then(Value::as_object) {
        for g in guilds {
            for (user, roles) in m {
                fake.set_member(g, user, serde_json::from_value(roles.clone()).expect("roles"));
            }
        }
    }
    Some(Arc::new(fake))
}

fn error_json(e: &StoreError) -> Value {
    match e {
        StoreError::Record(r) => r.to_json(),
        StoreError::Discord(d) => json!({ "discord": d.code, "message": d.message }),
        StoreError::Field(f) => json!({ "field": f.path, "message": f.message }),
        StoreError::Internal(e) => json!({ "internal": e.to_string() }),
    }
}

fn id_of(v: &Value) -> Option<String> {
    v.get("id").and_then(Value::as_str).map(str::to_string).or_else(|| v.get("item").and_then(|i| i.get("id")).and_then(Value::as_str).map(str::to_string))
}

fn opt_member(v: Option<&Value>) -> Option<DashboardMember> {
    v.filter(|m| !m.is_null()).map(|m| serde_json::from_value(m.clone()).expect("member"))
}

fn opt_user(v: Option<&Value>) -> Option<DashboardUser> {
    v.filter(|m| !m.is_null()).map(|m| serde_json::from_value(m.clone()).expect("user"))
}

/// The feeds' HTTP client: one canned answer for every URL.
pub struct CannedFetcher {
    pub status: Mutex<u16>,
    pub body: String,
    pub content_type: String,
    pub calls: Mutex<u64>,
}

impl feat_feeds::net::FeedFetcher for CannedFetcher {
    fn fetch(
        &self,
        url: String,
        _opts: bot_util::safe_fetch::FetchOptions,
    ) -> futures::future::BoxFuture<'static, Result<bot_util::safe_fetch::FetchResponse, feat_feeds::net::FetchFailure>> {
        *self.calls.lock().unwrap() += 1;
        let status = *self.status.lock().unwrap();
        let r = bot_util::safe_fetch::FetchResponse {
            ok: (200..300).contains(&status),
            status,
            status_text: String::new(),
            headers: [("content-type".to_string(), self.content_type.clone())].into_iter().collect(),
            url,
            buffer: self.body.as_bytes().to_vec(),
        };
        Box::pin(async move { Ok(r) })
    }

    fn fetch_json(
        &self,
        _url: String,
        _opts: bot_util::safe_fetch::FetchOptions,
    ) -> futures::future::BoxFuture<'static, Result<Value, feat_feeds::net::FetchFailure>> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(async move { Ok(json!({})) })
    }
}

/// Runs a spec through the Rust stores (the same answer shape as the JS `run`).
pub async fn run_rust(app: &App, spec: &Value) -> Value {
    let fetcher = spec.get("fetch").filter(|f| !f.is_null()).map(|f| {
        let c = Arc::new(CannedFetcher {
            status: Mutex::new(f.get("status").and_then(Value::as_u64).unwrap_or(200) as u16),
            body: f.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
            content_type: f.get("contentType").and_then(Value::as_str).unwrap_or("application/rss+xml").to_string(),
            calls: Mutex::new(0),
        });
        feat_feeds::net::set_fetcher(app, Some(c.clone()));
        c
    });
    let guild = spec["guild"].as_str().expect("guild").to_string();
    let other = spec.get("other").and_then(Value::as_str).unwrap_or("").to_string();
    let vars = Arc::new(Mutex::new(HashMap::from([("G".to_string(), guild.clone()), ("O".to_string(), other.clone())])));
    for (i, s) in spec.get("seeds").and_then(Value::as_array).into_iter().flatten().enumerate() {
        if let Some(s) = s.as_str() {
            vars.lock().unwrap().insert(format!("S{i}"), s.to_string());
        }
    }
    let guilds: Vec<&str> = [guild.as_str(), other.as_str()].into_iter().filter(|g| !g.is_empty()).collect();
    let fake = fake_discord(spec, &guilds);
    let tasks = Arc::new(ScriptTasks {
        answers: Mutex::new(spec.get("tasks").and_then(Value::as_object).cloned().unwrap_or_default()),
        vars: vars.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let default_actor = json!({ "userId": "700000000000000001", "user": null, "member": { "owner": true, "permissions": "0" } });
    let actor = spec.get("actor").cloned().unwrap_or(default_actor);
    let mut results = Vec::new();
    // One store per key for the whole run, as the registry keeps them (the feeds test throttle lives in its store).
    let mut stores: HashMap<String, Arc<dyn dash_collections::RecordStore>> = HashMap::new();
    for (i, op0) in spec["ops"].as_array().expect("ops").iter().enumerate() {
        let op = subst(op0, &vars.lock().unwrap());
        let g = op.get("guild").and_then(Value::as_str).unwrap_or(&guild).to_string();
        let c = op.get("ctx").cloned().unwrap_or(json!({}));
        let discord: Option<Arc<dyn DashboardDiscord>> =
            if c.get("discord") == Some(&json!(false)) { None } else { fake.clone().map(|f| f as Arc<dyn DashboardDiscord>) };
        let user_id = c.get("userId").and_then(Value::as_str).or(actor["userId"].as_str()).unwrap_or_default().to_string();
        let ctx = StoreCtx {
            guild_id: g.clone(),
            user_id,
            user: if c.get("user").is_some() { opt_user(c.get("user")) } else { opt_user(actor.get("user")) },
            member: if c.get("member").is_some() { opt_member(c.get("member")) } else { opt_member(actor.get("member")) },
            discord: discord.clone(),
            tasks: if c.get("tasks") == Some(&json!(false)) { None } else { Some(tasks.clone() as Arc<dyn TaskRunner>) },
        };
        let read = ReadCtx { guild_id: g.clone(), discord: discord.clone() };
        let key = op.get("store").and_then(Value::as_str).unwrap_or("").to_string();
        if !key.is_empty() && !stores.contains_key(&key) {
            stores.insert(key.clone(), dash_records_a::store_for(&key, app).expect("a store of this crate"));
        }
        let store = || stores.get(&key).expect("store").clone();
        let mut validated: Option<Result<Map<String, Value>, StoreError>> = None;
        if op.get("validate") == Some(&json!(true)) {
            let mode = match op["op"].as_str() {
                Some("create") => dash_collections::Mode::Create,
                Some("update") => dash_collections::Mode::Update,
                _ => dash_collections::Mode::Action,
            };
            let current = if mode == dash_collections::Mode::Create {
                None
            } else {
                let read0 = ReadCtx { guild_id: g.clone(), discord: None };
                store().get(&g, op["id"].as_str().unwrap_or(""), &read0).await.expect("get").filter(|v| !v.is_null())
            };
            let now = chrono::Utc::now().timestamp_millis() as f64;
            let raw = op.get("data").cloned().unwrap_or(json!({}));
            validated = Some(
                dash_collections::validate_record_input(&key, &raw, mode, op.get("action").and_then(Value::as_str), current.as_ref(), now).map(|v| v.data),
            );
        }
        if let Some(Err(e)) = validated {
            results.push(json!({ "err": error_json(&e) }));
            continue;
        }
        let validated = validated.map(|v| v.unwrap_or_default());
        let data = || validated.clone().unwrap_or_else(|| op.get("data").and_then(Value::as_object).cloned().unwrap_or_default());
        let id = op.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let r: Result<Value, StoreError> = match op["op"].as_str().expect("op") {
            "list" => {
                let opts = ListOpts {
                    query: op.get("query").and_then(Value::as_str).unwrap_or("").into(),
                    cursor: op.get("cursor").and_then(Value::as_str).unwrap_or("").into(),
                };
                store().list(&g, &opts, &read).await.map(|a| {
                    let mut m = json!({ "items": a.items });
                    if let Some(c) = a.next_cursor {
                        m["nextCursor"] = json!(c);
                    }
                    m
                })
            }
            "get" => store().get(&g, &id, &read).await.map(|v| v.unwrap_or(Value::Null)),
            "create" => store().create(&g, data(), &ctx).await.map(|s| json!({ "item": s.item, "message": s.message })),
            "update" => store().update(&g, &id, data(), &ctx).await.map(|s| json!({ "item": s.item, "message": s.message })),
            "remove" => store().remove(&g, &id, &ctx).await.map(|()| Value::Null),
            "action" => {
                let action = op["action"].as_str().expect("action");
                store().action(&g, &id, action, data(), &ctx).await.map(|a| json!({ "item": a.item, "message": a.message }))
            }
            "sql" => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(op["sql"].as_str().expect("sql").to_string())).execute(app.db()).await.expect("setup sql runs");
                Ok(Value::Null)
            }
            "query" => {
                let sql = format!("SELECT COALESCE(json_agg(t), '[]'::json)::text AS j FROM ({}) t", op["sql"].as_str().expect("sql"));
                let text: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).fetch_one(app.db()).await.expect("query runs");
                Ok(serde_json::from_str(&text).expect("json"))
            }
            "settings" => {
                let mut patch = bot_core::settings::SettingsPatch::new();
                for (k, v) in op["data"].as_object().expect("data") {
                    patch = patch.set(k, v.clone());
                }
                app.settings().update_settings(&g, patch).await.expect("settings update");
                Ok(Value::Null)
            }
            "cache" => Ok(app.cache().get(op["key"].as_str().expect("key")).await.unwrap_or(Value::Null)),
            "cacheSet" => Ok(json!(app.cache().set(op["key"].as_str().expect("key"), &op["value"], op["ttl"].as_u64().unwrap_or(60)).await)),
            "job" => Ok(match app.scheduler().get_job(op["key"].as_str().expect("key")).await {
                Some(j) => json!({
                    "type": j.r#type, "guildId": j.guild_id, "payload": j.payload, "maxAttempts": j.max_attempts,
                    "runInMs": j.run_at.timestamp_millis() - chrono::Utc::now().timestamp_millis(),
                }),
                None => Value::Null,
            }),
            "fail" => {
                fake.as_ref().expect("discord").fail_next(op["method"].as_str().expect("method"), api_error(&op["error"]));
                Ok(Value::Null)
            }
            "fetch" => {
                *fetcher.as_ref().expect("fetch").status.lock().unwrap() = op["status"].as_u64().unwrap_or(200) as u16;
                Ok(Value::Null)
            }
            "fetched" => Ok(json!(*fetcher.as_ref().expect("fetch").calls.lock().unwrap())),
            "answer" => {
                tasks.answers.lock().unwrap().insert(op["kind"].as_str().expect("kind").into(), op["result"].clone());
                Ok(Value::Null)
            }
            other => panic!("unknown op {other}"),
        };
        match r {
            Ok(v) => {
                let target = if matches!(op["op"].as_str(), Some("create" | "update")) { &v["item"] } else { &v };
                if let Some(id) = id_of(target) {
                    vars.lock().unwrap().insert(format!("ID{i}"), id);
                }
                if let Some(c) = v.get("nextCursor").and_then(Value::as_str) {
                    vars.lock().unwrap().insert(format!("C{i}"), c.to_string());
                }
                results.push(json!({ "ok": v }));
            }
            Err(e) => results.push(json!({ "err": error_json(&e) })),
        }
    }
    let discord_calls: Vec<Value> =
        fake.map(|f| f.calls().into_iter().map(|c| json!({ "method": c.method, "args": c.args })).collect()).unwrap_or_default();
    let task_calls = tasks.calls.lock().unwrap().clone();
    json!({ "results": results, "tasks": task_calls, "discord": discord_calls })
}

/// Reads back a store on guilds (list, then get of every listed item), like the JS `read`.
pub async fn read_rust(app: &App, spec: &Value, store_key: &str, guilds: &[&str]) -> Value {
    let fake = fake_discord(spec, guilds);
    let store = dash_records_a::store_for(store_key, app).expect("store");
    let mut out = Map::new();
    for g in guilds {
        let read = ReadCtx { guild_id: g.to_string(), discord: fake.clone().map(|f| f as Arc<dyn DashboardDiscord>) };
        let list = store.list(g, &ListOpts::default(), &read).await.unwrap_or_else(|e| panic!("list {store_key}: {e:?}"));
        let mut gets = Vec::new();
        for item in &list.items {
            gets.push(store.get(g, item["id"].as_str().expect("id"), &read).await.expect("get").unwrap_or(Value::Null));
        }
        let mut l = json!({ "items": list.items });
        if let Some(c) = list.next_cursor {
            l["nextCursor"] = json!(c);
        }
        out.insert(g.to_string(), json!({ "list": l, "gets": gets }));
    }
    Value::Object(out)
}

/// Normalises an answer for comparison across guilds: guild ids, cuids (by first appearance), ISO and UTC times,
/// internal error texts and job delays.
pub fn normalize(v: &Value, guild: &str, other: &str) -> Value {
    let cuid = Regex::new(r"\bc[a-z0-9]{24}\b").unwrap();
    let iso = Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z").unwrap();
    let utc = Regex::new(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC").unwrap();
    let mut seen: Vec<String> = Vec::new();
    fn go(v: &Value, f: &mut dyn FnMut(&str) -> String, key: Option<&str>) -> Value {
        match v {
            Value::String(s) => Value::String(f(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| go(x, f, None)).collect()),
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, x)| {
                        let x = match (k.as_str(), x) {
                            ("internal", Value::String(_)) => json!("<internal>"),
                            ("runInMs", Value::Number(n)) => json!(((n.as_f64().unwrap_or(0.0)) / 10_000.0).round() as i64 * 10),
                            _ => go(x, f, Some(k)),
                        };
                        (k.clone(), x)
                    })
                    .collect(),
            ),
            other => {
                let _ = key;
                other.clone()
            }
        }
    }
    let mut f = |s: &str| {
        let mut s = s.to_string();
        if !guild.is_empty() {
            s = s.replace(guild, "$G");
        }
        if !other.is_empty() {
            s = s.replace(other, "$O");
        }
        s = iso.replace_all(&s, "<date>").into_owned();
        s = utc.replace_all(&s, "<utc>").into_owned();
        cuid.replace_all(&s, |c: &regex::Captures| {
            let id = c[0].to_string();
            let n = match seen.iter().position(|x| *x == id) {
                Some(n) => n,
                None => {
                    seen.push(id);
                    seen.len() - 1
                }
            };
            format!("<cuid{n}>")
        })
        .into_owned()
    };
    go(v, &mut f, None)
}

/// Asserts two JSON values are equal, with the first difference in the message.
pub fn assert_same(what: &str, js: &Value, rs: &Value) {
    if let Some(d) = json_diff(js, rs, true) {
        panic!("{what}: JS and Rust differ at {d}\nJS:   {js}\nRust: {rs}");
    }
}

/// Guild ids of a scenario: (JS guild, JS other, Rust guild, Rust other).
pub fn guilds(n: u32) -> [String; 4] {
    [1, 2, 3, 4].map(|k| format!("94{n:06}0000000{k:03}"))
}

/// One scenario both ways:
/// 1. seeds (JS services) for every guild;
/// 2. the ops through the JS stores on guild A and through the Rust stores on guild B, compared normalised;
/// 3. each side reads back `read_store` on both guilds (JS writes, Rust reads and the reverse), compared exactly.
///
/// Returns the Rust answer (for extra assertions).
pub async fn scenario(t: &DbTestApp, n: u32, spec: Value, seed_steps: Value, read_store: Option<&str>) -> Value {
    let [ga, oa, gb, ob] = guilds(n);
    let all: Vec<&str> = vec![&ga, &oa, &gb, &ob];
    t.reset_guilds(&all).await;
    let seeds = if seed_steps.as_array().is_some_and(|a| !a.is_empty()) {
        node("seed", &json!({ "guilds": [ga, gb], "steps": seed_steps }))
    } else {
        json!({})
    };
    let mut spec_a = spec.clone();
    spec_a["guild"] = json!(ga);
    spec_a["other"] = json!(oa);
    spec_a["seeds"] = seeds.get(&ga).cloned().unwrap_or(json!([]));
    let mut spec_b = spec.clone();
    spec_b["guild"] = json!(gb);
    spec_b["other"] = json!(ob);
    spec_b["seeds"] = seeds.get(&gb).cloned().unwrap_or(json!([]));
    let js = node("run", &spec_a);
    // The Rust app's settings memo must not hold rows the JS side changed.
    for g in &all {
        t.settings().invalidate_settings(g).await;
    }
    let rs = run_rust(t, &spec_b).await;
    let (jn, rn) = (normalize(&js, &ga, &oa), normalize(&rs, &gb, &ob));
    assert_eq!(jn["results"].as_array().map(Vec::len), spec["ops"].as_array().map(Vec::len), "JS answered every op: {js}");
    assert_eq!(rn["results"].as_array().map(Vec::len), spec["ops"].as_array().map(Vec::len));
    for (k, (a, b)) in jn["results"].as_array().unwrap().iter().zip(rn["results"].as_array().unwrap()).enumerate() {
        assert_same(&format!("scenario {n} op {k} ({})", spec["ops"][k]), a, b);
    }
    assert_same(&format!("scenario {n} task calls"), &jn["tasks"], &rn["tasks"]);
    assert_same(&format!("scenario {n} Discord calls"), &jn["discord"], &rn["discord"]);
    if let Some(store) = read_store {
        let read_spec = json!({ "store": store, "guilds": [ga, gb], "discord": spec.get("discord").cloned().unwrap_or(Value::Null) });
        let js_read = node("read", &read_spec);
        for g in &all {
            t.settings().invalidate_settings(g).await;
        }
        let rs_read = read_rust(t, &read_spec, store, &[&ga, &gb]).await;
        assert_same(&format!("scenario {n} read back of {store}"), &js_read, &rs_read);
    }
    t.reset_guilds(&all).await;
    rs
}

/// The error code of a result (`ok` for a success).
pub fn code(result: &Value) -> String {
    match result.get("err") {
        Some(e) => e.get("code").and_then(Value::as_str).map_or_else(|| e.to_string(), str::to_string),
        None => "ok".into(),
    }
}
