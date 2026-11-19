//! dash-server against the JS dashboard: rust/fixtures/golden/dash-server/ is written by
//! rust/fixtures/gen/dash-server-golden.mjs from createDashboardServer, createDiscordApi and landing.js.
//! - every HTTP session of http.json is replayed against the Router, in order, with fakes ported one to one from the
//!   JS tests: status, headers, body, the log lines and the calls the fakes saw must all match;
//! - every Discord client call of discord-api.json is replayed against a local stub serving the recorded answers;
//! - the public pages are rendered byte for byte as landing.js renders them;
//! - the name collator is checked against Intl.Collator.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use common::*;
use dash_auth::config::{Contact, ContactKind};
use dash_collections::index::{CheckCtx, Problem};
use dash_collections::{Collection, Collections, Mode};
use dash_schema::RoleRef;
use dash_server::discord_api::{DiscordApi, DiscordApiConfig, DiscordApiOptions, ServerDiscord};
use dash_server::landing::{PageOptions, render_changelog, render_docs, render_landing, render_legal};
use dash_server::static_files::PublicDir;
use dash_server::{DashboardDeps, RecordingLogger, ServerOptions, create_dashboard_router, create_parked_router};
use dash_collections::discord::DashboardDiscord;
use indexmap::IndexSet;
use serde_json::{Map, Value, json};
use sha2::Digest;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/dash-server")
}

fn golden(name: &str) -> Value {
    let text = std::fs::read_to_string(golden_dir().join(name)).expect("golden file (run rust/fixtures/gen/dash-server-golden.mjs)");
    serde_json::from_str(&text).expect("golden JSON")
}

const KEEP_HEADERS: [&str; 16] = [
    "content-type",
    "cache-control",
    "retry-after",
    "allow",
    "location",
    "set-cookie",
    "content-encoding",
    "vary",
    "etag",
    "content-security-policy",
    "x-content-type-options",
    "referrer-policy",
    "x-frame-options",
    "cross-origin-opener-policy",
    "permissions-policy",
    "strict-transport-security",
];

fn things() -> Collection {
    fn locked(_data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
        let name = ctx.current.and_then(|c| c.get("name")).and_then(Value::as_str);
        (ctx.mode == Mode::Update && name == Some("Locked")).then(|| Problem::new("channelId", "Locked things keep their channel."))
    }
    Collection::from_json(json!({
        "key": "things", "module": "embeds", "label": "Things", "singular": "thing", "description": "", "limit": 5, "titleField": "name",
        "fields": [
            { "field": "name", "type": "text", "label": "Name", "maxLength": 32, "minLength": 1, "required": true },
            { "field": "channelId", "type": "channel", "label": "Channel", "channelTypes": ["text"], "nullable": true, "default": null },
            { "field": "roleId", "type": "role", "label": "Role", "assignable": true, "nullable": true, "default": null },
        ],
        "createOnly": ["name"], "readOnly": [],
        "actions": [{ "id": "post", "label": "Post", "fields": [{ "field": "channelId", "type": "channel", "label": "Channel", "channelTypes": ["text"], "required": true }] }],
    }))
    .expect("things")
    .with_check(locked)
}

fn role_collections() -> Collections {
    fn ref_keys(item: Option<&Value>, keys: IndexSet<String>) -> IndexSet<String> {
        let mut out: IndexSet<String> = keys.into_iter().filter(|k| !k.starts_with("actions.roleIds|")).collect();
        for a in item.and_then(|i| i.get("actions")).and_then(Value::as_array).into_iter().flatten() {
            let ty = dash_schema::js::to_js_string(a.get("type"));
            for id in a.get("roleIds").and_then(Value::as_array).into_iter().flatten() {
                out.insert(format!("actions.{ty}|{}", dash_schema::js::to_js_string(Some(id))));
            }
        }
        out
    }
    fn ref_key_of(r: &RoleRef, data: &Map<String, Value>) -> String {
        let parts: Vec<&str> = r.path.split('.').collect();
        if parts.len() == 4 && parts[0] == "actions" && parts[2] == "roleIds" && parts[1].bytes().all(|b| b.is_ascii_digit()) && !parts[1].is_empty()
            && !parts[3].is_empty() && parts[3].bytes().all(|b| b.is_ascii_digit())
        {
            let i: usize = parts[1].parse().unwrap_or(0);
            let ty = dash_schema::js::to_js_string(data.get("actions").and_then(|a| a.get(i)).and_then(|a| a.get("type")));
            return format!("actions.{ty}|{}", r.id);
        }
        dash_schema::ref_key(&r.path, &r.id)
    }
    let perks = Collection::from_json(json!({
        "key": "perks", "module": "embeds", "label": "Perks", "singular": "perk", "description": "", "limit": 5, "titleField": "name",
        "fields": [
            { "field": "name", "type": "text", "label": "Name", "maxLength": 32, "minLength": 1, "required": true },
            { "field": "pingRoleIds", "type": "roles", "label": "Ping roles", "maxItems": 5, "default": [] },
            { "field": "roleId", "type": "role", "label": "Role", "assignable": true, "nullable": true, "default": null },
            { "field": "rewardRoleId", "type": "role", "label": "Reward", "assignable": true, "allowDangerous": true, "nullable": true, "default": null },
        ],
        "createOnly": [], "readOnly": [], "actions": [],
    }))
    .expect("perks");
    let acts = Collection::from_json(json!({
        "key": "acts", "module": "embeds", "label": "Acts", "singular": "act", "description": "", "limit": 5, "titleField": "id",
        "fields": [{
            "field": "actions", "type": "objectList", "label": "Actions", "maxItems": 5, "default": [],
            "itemFields": [
                { "field": "type", "type": "enum", "label": "Type", "options": [{ "value": "GIVE", "label": "Give" }, { "value": "TAKE", "label": "Take" }], "default": "GIVE" },
                { "field": "roleIds", "type": "roles", "label": "Roles", "assignable": true, "maxItems": 5, "default": [] },
            ],
        }],
        "createOnly": [], "readOnly": [], "actions": [],
    }))
    .expect("acts")
    .with_ref_keys(ref_keys, ref_key_of);
    let locks = Collection::from_json(json!({
        "key": "locks", "module": "embeds", "label": "Locks", "singular": "lock", "description": "", "limit": 5, "titleField": "name",
        "fields": [
            { "field": "name", "type": "text", "label": "Name", "maxLength": 32, "minLength": 1, "required": true },
            { "field": "enabled", "type": "boolean", "label": "Enabled", "default": true },
            { "field": "actions", "type": "objectList", "label": "Actions", "maxItems": 5, "default": [],
              "itemFields": [{ "field": "roleId", "type": "role", "label": "Role", "assignable": true, "nullable": true, "default": null }] },
        ],
        "createOnly": [], "readOnly": [], "actions": [], "checkStoredRoles": true,
    }))
    .expect("locks");
    Collections::new(vec![perks, acts, locks])
}

type Act = Box<dyn Fn(&str) + Send + Sync>;
type CallsFn = Box<dyn Fn() -> Value + Send + Sync>;

struct Env {
    router: Router,
    clock: Arc<Mutex<f64>>,
    logger: Arc<RecordingLogger>,
    act: Act,
    calls: Option<CallsFn>,
}

fn deps(discord: Arc<dyn ServerDiscord>, settings: Arc<dyn dash_collections::SettingsBridge>, clock: &Arc<Mutex<f64>>) -> DashboardDeps {
    let c = clock.clone();
    DashboardDeps { discord, settings, records: None, bot_joins: None, collections: None, now: Some(Arc::new(move || *lock(&c))) }
}

fn build(name: &str) -> Env {
    let clock = Arc::new(Mutex::new(NOW));
    let logger = Arc::new(RecordingLogger::new());
    let public_test = PublicDir::Path(golden_dir().join("public-test"));
    let make = |config, deps: DashboardDeps, public: PublicDir| {
        let mut opts = ServerOptions::new(config, deps);
        opts.logger = Some(logger.clone());
        opts.public_dir = public;
        create_dashboard_router(opts).expect("router")
    };
    let none: Act = Box::new(|_| {});
    match name {
        "main" => {
            let f = create_fakes();
            let d = f.discord.clone();
            let calls = f.calls.clone();
            let router = make(config(&[("BOT_OWNER_ID", OWNER_ID)]), deps(f.discord.clone(), f.settings.clone(), &clock), public_test);
            let act: Act = Box::new(move |what| {
                let mut deny = lock(&d.invite_deny);
                if what == "denyAll" {
                    for id in [ch::TEXT, ch::NEWS, ch::VOICE] {
                        deny.insert(id.to_string());
                    }
                }
                if what == "restoreDeny" {
                    deny.clear();
                    deny.insert(ch::TEXT.to_string());
                }
            });
            Env { router, clock, logger, act, calls: Some(Box::new(move || lock(&calls).clone())) }
        }
        "records" => {
            let f = create_fakes();
            let rec = Arc::new(FakeRecords::new());
            let mut d = deps(f.discord.clone(), f.settings.clone(), &clock);
            d.records = Some(rec.clone());
            d.collections = Some(Collections::new(vec![things()]));
            let router = make(config(&[]), d, PublicDir::Embedded);
            Env { router, clock, logger, act: none, calls: Some(Box::new(move || Value::Array(lock(&rec.calls).clone()))) }
        }
        "registry" | "bare" => {
            let f = create_fakes();
            let mut d = deps(f.discord.clone(), f.settings.clone(), &clock);
            if name == "registry" {
                d.records = Some(Arc::new(FakeRecords::new()));
            }
            let router = make(config(&[]), d, PublicDir::Embedded);
            Env { router, clock, logger, act: none, calls: None }
        }
        "roles" => {
            let f = create_fakes();
            let rec = Arc::new(RoleRecords::new());
            let mut d = deps(f.discord.clone(), f.settings.clone(), &clock);
            d.records = Some(rec.clone());
            d.collections = Some(role_collections());
            let router = make(config(&[]), d, PublicDir::Embedded);
            let r2 = rec.clone();
            let act: Act = Box::new(move |what| {
                if what == "resetHigh" {
                    lock(&r2.items).insert("l-high".into(), json!({ "id": "l-high", "name": "High", "enabled": false, "actions": [{ "roleId": rl::MID }] }));
                }
            });
            Env { router, clock, logger, act, calls: Some(Box::new(move || Value::Array(lock(&rec.items).values().cloned().collect()))) }
        }
        "https" => {
            let f = create_fakes();
            let cfg = config(&[
                ("DASHBOARD_URL", "https://dash.example.com"),
                ("DASHBOARD_CONTACT", "owner@example.com"),
                ("DASHBOARD_SUPPORT_URL", "https://discord.gg/abc"),
            ]);
            let router = make(cfg, deps(f.discord.clone(), f.settings.clone(), &clock), public_test);
            Env { router, clock, logger, act: none, calls: None }
        }
        "joins" => {
            let f = create_fakes();
            let joins = Arc::new(FakeJoins { failing: Mutex::new(false), asked: Mutex::new(Vec::new()) });
            let mut d = deps(f.discord.clone(), f.settings.clone(), &clock);
            d.bot_joins = Some(joins.clone());
            let router = make(config(&[("BOT_OWNER_ID", OWNER_ID)]), d, PublicDir::Embedded);
            let j2 = joins.clone();
            let act: Act = Box::new(move |_| *lock(&j2.failing) = true);
            Env { router, clock, logger, act, calls: Some(Box::new(move || json!(*lock(&joins.asked)))) }
        }
        "site" => {
            let f = create_fakes();
            let router = make(config(&[]), deps(f.discord.clone(), f.settings.clone(), &clock), PublicDir::Embedded);
            Env { router, clock, logger, act: none, calls: None }
        }
        "parked" => Env {
            router: create_parked_router(
                &["DISCORD_CLIENT_SECRET".to_string(), "DASHBOARD_SESSION_SECRET".to_string()],
                &["DASHBOARD_URL".to_string()],
            ),
            clock,
            logger,
            act: none,
            calls: None,
        },
        login if login.starts_with("login-") => {
            let discord = Arc::new(LoginDiscord::new(true));
            let router = make(config(&[]), deps(discord.clone(), Arc::new(NoSettings), &clock), PublicDir::Embedded);
            Env { router, clock, logger, act: none, calls: Some(Box::new(move || json!(*lock(&discord.exchanges)))) }
        }
        other => panic!("unknown scenario {other}"),
    }
}

fn header_json(a: &Answer, name: &str) -> Option<Value> {
    if name == "set-cookie" {
        let cookies = a.cookies();
        return (!cookies.is_empty()).then(|| json!(cookies));
    }
    let values: Vec<String> = a.headers.get_all(name).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
    (!values.is_empty()).then(|| json!(values.join(", ")))
}

/// The answer in the golden's form (see normalize() in the script).
fn normalize(a: &Answer) -> Value {
    let key = dash_auth::auth::derive_session_key(SECRET).expect("key");
    let mut headers = Map::new();
    let mut session = None;
    for name in KEEP_HEADERS {
        let Some(mut v) = header_json(a, name) else { continue };
        if name == "set-cookie" {
            let list: Vec<Value> = v
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|c| {
                    let c = c.as_str().unwrap_or_default().to_string();
                    if let Some(rest) = c.strip_prefix("dash_session=v1.") {
                        let (sealed, tail) = rest.split_once(';').unwrap_or((rest, ""));
                        let token = dash_auth::auth::decode_uri_component(&format!("v1.{sealed}")).unwrap_or_default();
                        session = Some(dash_auth::auth::unseal_session(Some(&token), &key, NOW).map_or(Value::Null, Value::Object));
                        return json!(format!("dash_session=<sealed>;{tail}"));
                    }
                    if let Some(rest) = c.strip_prefix("dash_oauth_state=")
                        && rest.len() > 43
                        && rest.as_bytes()[43] == b';'
                    {
                        return json!(format!("dash_oauth_state=<state>{}", &rest[43..]));
                    }
                    json!(c)
                })
                .collect();
            v = Value::Array(list);
        }
        if name == "location"
            && let Some(s) = v.as_str()
            && let Some(i) = s.find("state=")
            && s.len() >= i + 6 + 43
        {
            v = json!(format!("{}state=<state>{}", &s[..i], &s[i + 6 + 43..]));
        }
        headers.insert(name.to_string(), v);
    }
    let mut out = Map::new();
    out.insert("status".into(), json!(a.status));
    out.insert("headers".into(), Value::Object(headers));
    let mut raw = a.body.clone();
    if a.header("content-encoding").as_deref() == Some("gzip") && !raw.is_empty() {
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut plain = Vec::new();
        d.read_to_end(&mut plain).expect("gunzip");
        raw = plain;
    }
    let ty = a.header("content-type").unwrap_or_default();
    if ty.starts_with("application/json") {
        match serde_json::from_slice::<Value>(&raw) {
            Ok(v) => out.insert("json".into(), v),
            Err(_) => out.insert("text".into(), json!(String::from_utf8_lossy(&raw))),
        };
    } else if ty.starts_with("text/") || ty.starts_with("image/svg") {
        out.insert("text".into(), json!(String::from_utf8_lossy(&raw)));
    } else {
        let digest = sha2::Sha256::digest(&raw);
        out.insert("sha256".into(), json!(digest.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        out.insert("length".into(), json!(raw.len()));
    }
    if let Some(s) = session {
        out.insert("session".into(), s);
    }
    Value::Object(out)
}

fn body_of(step: &Value) -> Option<Vec<u8>> {
    if let Some(gen_) = step.get("bodyGen") {
        let n = gen_["n"].as_u64().unwrap_or(0) as usize;
        let text = match gen_["kind"].as_str() {
            Some("settingsPad") => json!({ "changes": { "modEnabled": true }, "pad": "x".repeat(n) }).to_string(),
            _ => json!({ "data": { "name": "x".repeat(n) } }).to_string(),
        };
        return Some(text.into_bytes());
    }
    step.get("body").and_then(Value::as_str).map(|s| s.as_bytes().to_vec())
}

fn compare(expected: &Value, actual: &Value, what: &str, problems: &mut Vec<String>) {
    let mut e = dash_schema::js::canon(expected.clone());
    let a = dash_schema::js::canon(actual.clone());
    if let Value::Object(m) = &mut e {
        m.remove("hasContentLength");
    }
    if e != a {
        problems.push(format!("{what}\n  expected {}\n  actual   {}", trunc(&e), trunc(&a)));
    }
}

fn trunc(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 1500 { format!("{}...", &s[..s.char_indices().nth(1500).map_or(s.len(), |(i, _)| i)]) } else { s }
}

#[tokio::test]
async fn http_sessions_match_the_js_server() {
    let g = golden("http.json");
    let mut problems = Vec::new();
    let mut answers = 0;
    for scenario in g["scenarios"].as_array().expect("scenarios") {
        let name = scenario["name"].as_str().unwrap_or_default();
        let env = build(name);
        let expected = scenario["responses"].as_array().expect("responses");
        let mut i = 0;
        let mut last_session: Option<String> = None;
        for step in scenario["steps"].as_array().expect("steps") {
            if let Some(ms) = step.get("advance").and_then(Value::as_f64) {
                *lock(&env.clock) += ms;
                continue;
            }
            if let Some(ms) = step.get("wait").and_then(Value::as_u64) {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                continue;
            }
            if let Some(what) = step.get("fake").and_then(Value::as_str) {
                (env.act)(what);
                continue;
            }
            let method = step["method"].as_str().unwrap_or("GET");
            let path = step["path"].as_str().unwrap_or("/");
            let mut headers: Vec<(&str, String)> = Vec::new();
            let mut owned: Vec<(String, String)> = Vec::new();
            for (k, v) in step["headers"].as_object().into_iter().flatten() {
                owned.push((k.clone(), v.as_str().unwrap_or_default().to_string()));
            }
            if step.get("cookieFromPrevious") == Some(&Value::Bool(true)) {
                owned.retain(|(k, _)| k != "cookie");
                owned.push(("cookie".into(), format!("dash_session={}", last_session.clone().unwrap_or_default())));
            }
            for (k, v) in &owned {
                headers.push((k.as_str(), v.clone()));
            }
            let answer = call(&env.router, method, path, &headers, body_of(step)).await;
            for c in answer.cookies() {
                if let Some(rest) = c.strip_prefix("dash_session=v1.") {
                    let sealed = rest.split(';').next().unwrap_or_default();
                    last_session = dash_auth::auth::decode_uri_component(&format!("v1.{sealed}"));
                }
            }
            let actual = normalize(&answer);
            compare(&expected[i], &actual, &format!("{name} #{i}: {method} {path}"), &mut problems);
            i += 1;
            answers += 1;
        }
        assert_eq!(i, expected.len(), "{name}: every answer replayed");
        let log: Vec<String> = env.logger.lines();
        let expected_log: Vec<String> =
            scenario["log"].as_array().into_iter().flatten().map(|l| l.as_str().unwrap_or_default().to_string()).collect();
        if log != expected_log {
            problems.push(format!("{name}: log lines differ\n  expected {expected_log:#?}\n  actual   {log:#?}"));
        }
        if let Some(calls) = &env.calls {
            compare(&scenario["calls"], &calls(), &format!("{name}: fake calls"), &mut problems);
        }
    }
    assert!(answers > 1500, "replayed {answers} answers");
    assert!(problems.is_empty(), "{} differences:\n{}", problems.len(), problems.iter().take(25).cloned().collect::<Vec<_>>().join("\n"));
}

// ── Discord client ──────────────────────────────────────────────────────────────────────────────────────────────

type StubCalls = Arc<Mutex<Vec<Value>>>;

async fn start_stub(routes: Vec<Value>) -> (String, StubCalls, tokio::task::JoinHandle<()>) {
    let calls: StubCalls = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    let app = Router::new().fallback(move |req: axum::extract::Request| {
        let calls = c2.clone();
        let routes = routes.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
            let path = path.strip_prefix("/api/v10").unwrap_or(&path).to_string();
            let (auth, ct, reason) = {
                let h = |n: &str| req.headers().get(n).map(|v| json!(String::from_utf8_lossy(v.as_bytes()))).unwrap_or(Value::Null);
                (h("authorization"), h("content-type"), h("x-audit-log-reason"))
            };
            let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap_or_default();
            let body = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
            lock(&calls).push(json!({ "method": method, "path": path, "auth": auth, "contentType": ct, "reason": reason, "body": body }));
            let route = routes.iter().find(|r| r["method"] == json!(method) && r["path"] == json!(path));
            let mut res = axum::response::Response::new(axum::body::Body::empty());
            match route {
                None => {
                    *res.status_mut() = http::StatusCode::NOT_FOUND;
                    *res.body_mut() = axum::body::Body::from("{\"code\":0}");
                }
                Some(r) => {
                    *res.status_mut() = http::StatusCode::from_u16(r["status"].as_u64().unwrap_or(200) as u16).expect("status");
                    let body = if r["raw"] == json!(true) {
                        r["body"].as_str().unwrap_or_default().to_string()
                    } else if r["body"].is_null() {
                        String::new()
                    } else {
                        r["body"].to_string()
                    };
                    *res.body_mut() = axum::body::Body::from(body);
                    for (k, v) in r["headers"].as_object().into_iter().flatten() {
                        res.headers_mut().insert(
                            http::HeaderName::from_bytes(k.as_bytes()).expect("name"),
                            http::HeaderValue::from_str(v.as_str().unwrap_or_default()).expect("value"),
                        );
                    }
                }
            }
            res
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/api/v10"), calls, handle)
}

pub fn api_for(base: &str) -> DiscordApi {
    let mut opts = DiscordApiOptions::new(DiscordApiConfig {
        client_id: "123456789012345678".into(),
        client_secret: "s".into(),
        bot_token: "bot-token".into(),
        redirect_uri: "http://localhost:8080/auth/callback".into(),
    });
    opts.api_base = base.to_string();
    opts.now = Some(Arc::new(|| 0.0));
    opts.sleep = Some(Arc::new(|_| Box::pin(async {})));
    DiscordApi::new(opts)
}

fn result_json<T: serde::Serialize>(r: Result<T, dash_collections::DiscordApiError>) -> Value {
    match r {
        Ok(v) => json!({ "value": serde_json::to_value(v).unwrap_or(Value::Null) }),
        Err(e) => json!({ "error": { "message": e.message, "status": e.status, "code": e.code, "retryAfter": e.retry_after, "discordCode": e.discord_code } }),
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn discord_client_matches_create_discord_api() {
    let g = golden("discord-api.json");
    let (base, calls, stub) = start_stub(g["routes"].as_array().cloned().unwrap_or_default()).await;
    let api = api_for(&base);
    let mut problems = Vec::new();
    for case in g["results"].as_array().expect("results") {
        let op = case["op"].as_str().unwrap_or_default();
        let a = case["args"].as_array().cloned().unwrap_or_default();
        let before = lock(&calls).len();
        let result = match op {
            "getGuildResources" => result_json(api.get_guild_resources(&s(&a[0]), false).await.map(|r| (*r).clone())),
            "getMember" => result_json(api.get_member(&s(&a[0]), &s(&a[1])).await),
            "searchMembers" if a.len() == 3 => result_json(api.search_members_limit(&s(&a[0]), &s(&a[1]), a[2].as_f64().unwrap_or(0.0)).await),
            "searchMembers" => result_json(api.search_members(&s(&a[0]), &s(&a[1])).await),
            "getUsers" => {
                let ids: Vec<String> = a[0].as_array().into_iter().flatten().map(s).collect();
                result_json(api.get_users(&ids).await.map(|m| m.into_iter().collect::<BTreeMap<_, _>>()))
            }
            "getCurrentUser" => result_json(api.get_current_user(&s(&a[0])).await),
            "getUserGuilds" => result_json(api.get_user_guilds(&s(&a[0]), &s(&a[1]), false).await.map(|g| (*g).clone())),
            "getBotGuilds" => result_json(api.get_bot_guilds().await.map(|g| (*g).clone())),
            "isBotInGuild" => result_json(api.is_bot_in_guild(&s(&a[0]), a[1] == json!(true)).await),
            "sendMessage" => result_json(api.send_message(&s(&a[0]), a[1].clone()).await),
            "editMessage" => result_json(api.edit_message(&s(&a[0]), &s(&a[1]), a[2].clone()).await),
            "deleteMessage" => result_json(api.delete_message(&s(&a[0]), &s(&a[1])).await),
            "createInvite" => result_json(api.create_invite(&s(&a[0]), &s(&a[1])).await),
            other => panic!("unknown op {other}"),
        };
        compare(&case["result"], &result, &format!("{op} {a:?}"), &mut problems);
        let mut got: Vec<Value> = lock(&calls)[before..].to_vec();
        let mut want: Vec<Value> = case["calls"].as_array().cloned().unwrap_or_default();
        let key = |v: &Value| format!("{} {}", v["method"], v["path"]);
        got.sort_by_key(key);
        want.sort_by_key(key);
        compare(&Value::Array(want), &Value::Array(got), &format!("{op} {a:?}: requests"), &mut problems);
    }
    let urls = &g["urls"];
    let invites: Vec<String> = vec![api.invite_url(None), api.invite_url(Some(common::g::ZETA)), api.invite_url(Some("x"))];
    compare(&urls["invite"], &json!(invites), "inviteUrl", &mut problems);
    for case in urls["avatars"].as_array().into_iter().flatten() {
        let url = dash_server::discord_api::user_avatar_url(case["id"].as_str(), case["avatar"].as_str());
        compare(&case["url"], &json!(url), &format!("userAvatarUrl {case}"), &mut problems);
    }
    for case in urls["icons"].as_array().into_iter().flatten() {
        let url = dash_server::discord_api::guild_icon_url(case["id"].as_str(), case["icon"].as_str());
        compare(&case["url"], &json!(url), &format!("guildIconUrl {case}"), &mut problems);
    }
    assert_eq!(urls["invitePermissions"], json!(dash_server::discord_api::INVITE_PERMISSIONS.to_string()));
    assert_eq!(urls["dangerousPermissions"], json!(dash_server::discord_api::DANGEROUS_PERMISSIONS.to_string()));
    for case in g["sortCases"].as_array().into_iter().flatten() {
        let input: Vec<dash_collections::ResChannel> = serde_json::from_value(case["input"].clone()).expect("channels");
        let out: Vec<String> = dash_server::discord_api::sort_channels(input).into_iter().map(|c| c.id).collect();
        compare(&case["output"], &json!(out), "sortChannels", &mut problems);
    }
    stub.abort();
    assert!(problems.is_empty(), "{} differences:\n{}", problems.len(), problems.join("\n"));
}

// ── Pages and collator ──────────────────────────────────────────────────────────────────────────────────────────

fn page_options(o: &Value) -> PageOptions {
    let contact = o.get("contact").and_then(|c| {
        let kind = match c["kind"].as_str()? {
            "email" => ContactKind::Email,
            _ => ContactKind::Url,
        };
        Some(Contact { kind, value: s(&c["value"]) })
    });
    PageOptions {
        invite_url: o.get("inviteUrl").and_then(Value::as_str).map(str::to_string),
        contact,
        support_url: o.get("supportUrl").and_then(Value::as_str).map(str::to_string),
        changelog: o.get("changelog").and_then(Value::as_str).map(str::to_string),
    }
}

#[test]
fn public_pages_are_byte_identical() {
    let g = golden("pages.json");
    let public = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../src/dashboard/public");
    let read = |f: &str| String::from_utf8_lossy(&std::fs::read(public.join(f)).expect("template")).into_owned();
    let mut checked = 0;
    for p in g["pages"].as_array().expect("pages") {
        let opts = page_options(&p["options"]);
        let page = p["page"].as_str().unwrap_or_default();
        let template = p.get("template").and_then(Value::as_str).map(str::to_string);
        let html = match page {
            "landing" => render_landing(&template.unwrap_or_else(|| read("landing.html")), &opts),
            "docs" => render_docs(&read("docs.html"), &opts),
            "changelog" => render_changelog(&read("changelog.html"), &opts),
            f => render_legal(&read(f), &opts),
        }
        .expect("renders");
        assert!(html == s(&p["html"]), "{page} ({}) differs", p["name"]);
        checked += 1;
    }
    assert_eq!(checked, 16);
    for e in g["errors"].as_array().expect("errors") {
        let got = render_landing(&s(&e["template"]), &PageOptions::default()).err();
        assert_eq!(got.as_deref(), e["error"].as_str(), "{}", e["template"]);
    }
}

#[test]
fn name_collator_matches_intl_collator() {
    let g = golden("collator.json");
    let names: Vec<String> = g["names"].as_array().expect("names").iter().map(s).collect();
    let pairs = g["pairs"].as_array().expect("pairs");
    let mut wrong = Vec::new();
    let mut k = 0;
    for a in &names {
        for b in &names {
            let want = pairs[k].as_i64().unwrap_or(0);
            let got = match dash_server::collate::name_compare(a, b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            if want != got {
                wrong.push(format!("{a:?} vs {b:?}: Intl {want}, Rust {got}"));
            }
            k += 1;
        }
    }
    assert!(wrong.is_empty(), "{} of {} pairs differ:\n{}", wrong.len(), k, wrong.join("\n"));
    let _ = HashMap::<String, String>::new();
}
