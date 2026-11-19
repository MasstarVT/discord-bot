//! End to end: dash-server's real router with the real bridges of bins/dashboard on the mock database, served on a
//! socket, a session cookie sealed by dash-auth, Discord's API stubbed on a local port, and the bot side of the
//! dashboard tasks (feat-dashboard-tasks) running in-process on its own App. Settings are read and written, records
//! of four collections are listed, created, updated and removed, and a "Give XP" create travels as a dashboard task to
//! the bot and back.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use common::*;
use dash_server::discord_api::{DiscordApiConfig, DiscordApiOptions};
use dash_server::{DiscordApi, ServerOptions, create_dashboard_router};
use dashboard::index::dashboard_deps;
use dashboard::tasks::TaskClient;
use serde_json::{Value, json};

const G: &str = "970000000000000001";
const CLIENT_ID: &str = "123456789012345678";
const SECRET: &str = "a-session-secret-that-is-long-enough-0123456789";

/// The Discord API as the dashboard sees it for guild G (Bob manages it; the bot is in it).
fn discord_answer(method: &str, path: &str, auth: &str) -> (u16, Value) {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let role = |id: &str, name: &str, position: i64, perms: &str| json!({ "id": id, "name": name, "color": 0, "hoist": false, "position": position, "permissions": perms, "managed": false, "mentionable": false });
    let guild = || {
        json!({
            "id": G, "name": "zeta", "icon": null, "owner_id": OWNER, "approximate_member_count": 3, "premium_tier": 0,
            "premium_subscription_count": 0, "verification_level": 0,
            "roles": [role(G, "@everyone", 0, "1024"), role(ROLE_PLAIN, "Member", 1, "0"), role(ROLE_MOD, "Mods", 2, "2"), role(ROLE_ADMIN, "Admins", 3, "32"), role(ROLE_BOT, "Bot", 5, "8")],
            "emojis": []
        })
    };
    match (method, route) {
        ("GET", "/users/@me/guilds") if auth.starts_with("Bearer") => {
            (200, json!([{ "id": G, "name": "zeta", "icon": null, "owner": false, "permissions": "32", "features": [] }]))
        }
        ("GET", "/users/@me/guilds") => (200, if query.contains("after=0") { json!([{ "id": G, "name": "zeta", "icon": null }]) } else { json!([]) }),
        ("GET", r) if r == format!("/guilds/{G}") => (200, guild()),
        ("GET", r) if r == format!("/guilds/{G}/channels") => (
            200,
            json!([
                { "id": CH_TEXT, "type": 0, "name": "general", "position": 0, "parent_id": null, "nsfw": false, "permission_overwrites": [] },
                { "id": CH_LOG, "type": 0, "name": "mod-log", "position": 1, "parent_id": null, "nsfw": false, "permission_overwrites": [] }
            ]),
        ),
        ("GET", r) if r == format!("/guilds/{G}/members/{CLIENT_ID}") => {
            (200, json!({ "user": { "id": CLIENT_ID, "username": "bot", "bot": true }, "roles": [ROLE_BOT], "joined_at": "2025-01-01T00:00:00.000Z" }))
        }
        ("GET", r) if r == format!("/guilds/{G}/members/{BOB}") => {
            (200, json!({ "user": { "id": BOB, "username": "bob", "global_name": null, "avatar": null }, "roles": [ROLE_ADMIN], "joined_at": "2025-01-01T00:00:00.000Z" }))
        }
        ("GET", r) if r.starts_with("/users/") => {
            let id = &r["/users/".len()..];
            (200, json!({ "id": id, "username": format!("user{id}"), "global_name": null, "avatar": null, "discriminator": "0" }))
        }
        ("POST", r) if r.starts_with("/channels/") => (200, json!({ "id": "1300000000000000001", "channel_id": CH_TEXT })),
        _ => (404, json!({ "message": "Unknown", "code": 0 })),
    }
}

struct Stub {
    base: String,
    calls: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

async fn discord_stub() -> Stub {
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let calls = c2.clone();
        async move {
            let method = req.method().as_str().to_string();
            let full = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
            let path = full.strip_prefix("/api/v10").unwrap_or(&full).to_string();
            let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            calls.lock().unwrap().push(format!("{method} {path}"));
            let (status, body) = discord_answer(&method, &path, &auth);
            let mut res = axum::response::Response::new(axum::body::Body::from(body.to_string()));
            *res.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
            res.headers_mut().insert("content-type", axum::http::HeaderValue::from_static("application/json"));
            res
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub { base: format!("http://{addr}/api/v10"), calls, task }
}

struct Client {
    http: reqwest::Client,
    base: String,
    cookie: String,
    origin: String,
}

impl Client {
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method.clone(), format!("{}{path}", self.base)).header("cookie", &self.cookie);
        if method != reqwest::Method::GET {
            req = req.header("content-type", "application/json").header("x-dashboard", "1").header("origin", &self.origin);
        }
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let res = req.send().await.expect("the dashboard answers");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }
    async fn delete(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::DELETE, path, None).await
    }
}

#[tokio::test]
async fn the_dashboard_end_to_end() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;

    // The bot process, with the dashboard-tasks plugin, and the dashboard process, on the same database and Redis.
    let bot = Bot::new(&db, &redis, &[G]).await;
    bot.reset(&[G]).await;
    let bot_loop = bot.spawn_loop();
    let app = dashboard_app(&db, &redis).await;

    let stub = discord_stub().await;
    let env: HashMap<String, String> = [
        ("CLIENT_ID", CLIENT_ID),
        ("DISCORD_TOKEN", "bot-token"),
        ("DISCORD_CLIENT_SECRET", "client-secret"),
        ("DASHBOARD_SESSION_SECRET", SECRET),
        ("DATABASE_URL", db.as_str()),
        ("REDIS_URL", redis.as_str()),
        ("DASHBOARD_URL", "http://localhost:8080"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let loaded = dash_auth::config::load_dashboard_config_from_map(&env);
    assert!(loaded.ok, "{:?} {:?}", loaded.missing, loaded.invalid);
    let config = loaded.config;
    let mut opts = DiscordApiOptions::new(DiscordApiConfig::from(&config));
    opts.api_base = stub.base.clone();
    let discord = Arc::new(DiscordApi::new(opts));
    let deps = dashboard_deps(&app, discord, TaskClient::new(app.clone()).wait_ms(15_000).poll_ms(50)).expect("every collection has a store");
    let router = create_dashboard_router(ServerOptions::new(config.clone(), deps)).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let stop = Arc::new(tokio::sync::Notify::new());
    let wait = stop.clone();
    let server = tokio::spawn(dash_server::serve_listener(listener, router, dash_server::serve::HEADERS_TIMEOUT_MS, async move { wait.notified().await }));

    // A session as the login callback seals it (exp in ms, an integer).
    let key = dash_auth::auth::derive_session_key(SECRET).unwrap();
    let exp = chrono::Utc::now().timestamp_millis() + 3_600_000;
    let sealed = dash_auth::auth::seal_session(
        &json!({ "uid": BOB, "accessToken": "user-access-token", "username": "bob", "globalName": null, "avatar": null, "exp": exp }),
        &key,
    );
    let c = Client {
        http: reqwest::Client::new(),
        base: format!("http://{addr}"),
        cookie: format!("{}={sealed}", dash_auth::auth::SESSION_COOKIE),
        origin: config.origin.clone(),
    };

    // Who am I, and my servers.
    let (s, me) = c.get("/api/me").await;
    assert_eq!(s, 200, "{me}");
    let (s, guilds) = c.get("/api/guilds").await;
    assert_eq!(s, 200, "{guilds}");
    assert!(guilds.to_string().contains(G), "{guilds}");

    // Settings: read, write, read back.
    let (s, settings) = c.get(&format!("/api/guilds/{G}/settings")).await;
    assert_eq!(s, 200, "{settings}");
    assert_eq!(settings["guild"]["id"], json!(G));
    assert_eq!(settings["values"]["xpEnabled"], json!(false));
    let (s, written) = c.patch(&format!("/api/guilds/{G}/settings"), json!({ "changes": { "xpEnabled": true, "birthdayHour": 7, "modLogChannelId": CH_LOG } })).await;
    assert_eq!(s, 200, "{written}");
    assert_eq!(written["values"]["xpEnabled"], json!(true));
    assert_eq!(written["values"]["birthdayHour"], json!(7));
    assert_eq!(written["values"]["modLogChannelId"], json!(CH_LOG));
    let stored: (bool, i32) = sqlx::query_as(r#"SELECT "xpEnabled", "birthdayHour" FROM "GuildSettings" WHERE "guildId" = $1"#).bind(G).fetch_one(app.db()).await.unwrap();
    assert_eq!(stored, (true, 7));
    // The moderation log confirmation went to Discord.
    assert!(stub.calls.lock().unwrap().iter().any(|l| l == &format!("POST /channels/{CH_LOG}/messages")), "{:?}", stub.calls.lock().unwrap());
    let (s, again) = c.get(&format!("/api/guilds/{G}/settings")).await;
    assert_eq!(s, 200);
    assert_eq!(again["values"]["birthdayHour"], json!(7));
    // A refused write names its path and saves nothing.
    let (s, refused) = c.patch(&format!("/api/guilds/{G}/settings"), json!({ "changes": { "birthdayHour": 99 } })).await;
    assert_eq!(s, 400, "{refused}");
    assert_eq!(refused["path"], json!("birthdayHour"));

    // Records of four collections: create, list, update, remove.
    let cases = [
        ("levelRoles", json!({ "level": 5, "roleId": ROLE_PLAIN }), json!({ "level": 6 }), "level", json!(6)),
        ("shopItems", json!({ "name": "Badge", "price": 100 }), json!({ "price": 150 }), "price", json!(150)),
        ("economyBoosts", json!({ "name": "Weekend", "percent": 25 }), json!({ "percent": 30 }), "percent", json!(30)),
        ("triggers", json!({ "trigger": "hello", "response": "Hi!" }), json!({ "response": "Hey!" }), "response", json!("Hey!")),
    ];
    for (key, create, update, field, want) in cases {
        let base = format!("/api/guilds/{G}/records/{key}");
        let (s, created) = c.post(&base, json!({ "data": create })).await;
        assert_eq!(s, 201, "{key}: {created}");
        let id = created["item"]["id"].as_str().unwrap_or_else(|| panic!("{key}: {created}")).to_string();
        let (s, list) = c.get(&base).await;
        assert_eq!(s, 200, "{key}: {list}");
        assert!(list["items"].as_array().unwrap().iter().any(|i| i["id"] == json!(id)), "{key}: {list}");
        let (s, updated) = c.patch(&format!("{base}/{id}"), json!({ "data": update })).await;
        assert_eq!(s, 200, "{key}: {updated}");
        assert_eq!(updated["item"][field], want, "{key}: {updated}");
        let (s, one) = c.get(&format!("{base}/{id}")).await;
        assert_eq!((s, one["item"][field].clone()), (200, want.clone()), "{key}: {one}");
        let (s, removed) = c.delete(&format!("{base}/{id}")).await;
        assert!(s == 200 || s == 204, "{key}: {s} {removed}");
        let (s, gone) = c.get(&format!("{base}/{id}")).await;
        assert_eq!(s, 404, "{key}: {gone}");
    }
    let (s, unknown) = c.get(&format!("/api/guilds/{G}/records/noSuchList")).await;
    assert_eq!(s, 404, "{unknown}");

    // A dashboard task round trip: "Give XP" queues xp.adjust for the bot, which runs it and answers.
    let (s, xp) = c.post(&format!("/api/guilds/{G}/records/levels"), json!({ "data": { "userId": ALICE, "amount": 150 } })).await;
    assert_eq!(s, 201, "{xp}");
    assert_eq!(xp["item"]["xp"], json!(150), "{xp}");
    let row: i32 = sqlx::query_scalar(r#"SELECT "xp" FROM "UserLevel" WHERE "guildId" = $1 AND "userId" = $2"#).bind(G).bind(ALICE).fetch_one(app.db()).await.unwrap();
    assert_eq!(row, 150);
    let left: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "ScheduledJob" WHERE "guildId" = $1 AND "type" = 'dashboard.task'"#).bind(G).fetch_one(app.db()).await.unwrap();
    assert_eq!(left, 0, "the finished task's row is gone");
    // An action on the member: Remove XP.
    let (s, took) = c.post(&format!("/api/guilds/{G}/records/levels/{ALICE}/actions/take"), json!({ "data": { "amount": 50 } })).await;
    assert_eq!(s, 200, "{took}");
    assert_eq!(took["item"]["xp"], json!(100), "{took}");
    assert!(took["message"].as_str().is_some_and(|m| m.contains("50")), "{took}");

    stop.notify_one();
    server.await.unwrap();
    bot_loop.abort();
    stub.task.abort();
    bot.reset(&[G]).await;
}

