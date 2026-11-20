//! dash-server behaviour that the recorded HTTP sessions (tests/golden.rs) cannot show: the Discord client's caches,
//! shared loads, retries and error mapping over a local stub (test/dashboard.test.js "discordApi"), concurrent login
//! callbacks (test/sec-dashboard.test.js), and the server on a real socket (HEAD, 204/304 framing, 413 closing the
//! connection, request targets, peer addresses and shutdown).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use common::*;
use dash_collections::discord::DashboardDiscord;
use dash_server::discord_api::{DiscordApi, DiscordApiConfig, DiscordApiOptions, ServerDiscord};
use dash_server::static_files::{PublicDir, accepts_gzip, etag_matches};
use dash_server::{DashboardDeps, RecordingLogger, ServerOptions, create_dashboard_router, create_parked_router, serve_listener};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── A scriptable Discord stub ────────────────────────────────────────────────────────────────────────────────────

type Reply = (u16, String, Vec<(String, String)>);
type Handler = Arc<dyn Fn(&str, &str) -> Reply + Send + Sync>;

#[derive(Debug, Clone)]
struct StubCall {
    #[allow(dead_code)]
    method: String,
    path: String,
    auth: String,
    agent: String,
}

struct Stub {
    base: String,
    calls: Arc<Mutex<Vec<StubCall>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Stub {
    fn count(&self) -> usize {
        lock(&self.calls).len()
    }
    fn calls(&self) -> Vec<StubCall> {
        lock(&self.calls).clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn stub(handler: Handler) -> Stub {
    stub_with(handler, None).await
}

/// A closed gate is a semaphore without permits: the stub records the request, then waits for `open(&gate)`.
fn closed_gate() -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(0))
}

fn open(gate: &Arc<tokio::sync::Semaphore>) {
    gate.add_permits(1_000_000);
}

fn close(gate: &Arc<tokio::sync::Semaphore>) {
    gate.forget_permits(1_000_000);
}

async fn stub_with(handler: Handler, gate: Option<Arc<tokio::sync::Semaphore>>) -> Stub {
    let calls: Arc<Mutex<Vec<StubCall>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    let app = Router::new().fallback(move |req: axum::extract::Request| {
        let calls = c2.clone();
        let handler = handler.clone();
        let gate = gate.clone();
        async move {
            let method = req.method().as_str().to_string();
            let full = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
            let path = full.strip_prefix("/api/v10").unwrap_or(&full).to_string();
            let auth = req.headers().get("authorization").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default();
            let agent = req.headers().get("user-agent").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default();
            lock(&calls).push(StubCall { method: method.clone(), path: path.clone(), auth, agent });
            if let Some(g) = &gate {
                if let Ok(permit) = g.acquire().await {
                    permit.forget();
                }
            }
            let (status, body, headers) = handler(&method, &path);
            let mut res = axum::response::Response::new(axum::body::Body::from(body));
            *res.status_mut() = http::StatusCode::from_u16(status).expect("status");
            for (k, v) in headers {
                res.headers_mut().insert(http::HeaderName::from_bytes(k.as_bytes()).expect("name"), http::HeaderValue::from_str(&v).expect("value"));
            }
            res
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub { base: format!("http://{addr}/api/v10"), calls, handle }
}

fn ok(body: Value) -> Reply {
    (200, body.to_string(), Vec::new())
}

type Clock = Arc<Mutex<f64>>;

fn api(base: &str, clock: Option<Clock>, sleeps: Option<Arc<Mutex<Vec<u64>>>>) -> DiscordApi {
    let mut opts = DiscordApiOptions::new(DiscordApiConfig {
        client_id: "123456789012345678".into(),
        client_secret: "s".into(),
        bot_token: "bot-token".into(),
        redirect_uri: "http://localhost:8080/auth/callback".into(),
    });
    opts.api_base = base.to_string();
    opts.token_url = format!("{}/oauth2/token", base.trim_end_matches("/api/v10"));
    opts.revoke_url = format!("{}/oauth2/token/revoke", base.trim_end_matches("/api/v10"));
    if let Some(c) = clock {
        opts.now = Some(Arc::new(move || *lock(&c)));
    }
    if let Some(s) = sleeps {
        opts.sleep = Some(Arc::new(move |ms| {
            lock(&s).push(ms);
            Box::pin(async {})
        }));
    }
    DiscordApi::new(opts)
}

fn clock(t: f64) -> Clock {
    Arc::new(Mutex::new(t))
}

fn advance(c: &Clock, ms: f64) {
    *lock(c) += ms;
}

fn guild_list() -> Value {
    json!([{ "id": g::ZETA, "name": "Z", "icon": null, "owner": false, "permissions": "32" }, { "id": "bad" }])
}

#[tokio::test]
async fn user_guilds_are_cached_per_user_and_token_for_60_s_and_shared() {
    let s = stub(Arc::new(|_, _| ok(guild_list()))).await;
    let t = clock(1_000_000.0);
    let api = api(&s.base, Some(t.clone()), None);
    let (a, b) = tokio::join!(api.get_user_guilds(USER_ID, TOKEN, false), api.get_user_guilds(USER_ID, TOKEN, false));
    let (a, b) = (a.expect("a"), b.expect("b"));
    assert_eq!(s.count(), 1, "concurrent callers share one request");
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(a.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), vec![g::ZETA], "invalid entries are dropped");
    assert_eq!(s.calls()[0].auth, format!("Bearer {TOKEN}"));
    assert_eq!(s.calls()[0].agent, "node", "the User-Agent of Node's fetch");
    advance(&t, 59_000.0);
    api.get_user_guilds(USER_ID, TOKEN, false).await.expect("cached");
    assert_eq!(s.count(), 1);
    advance(&t, 2_000.0);
    api.get_user_guilds(USER_ID, TOKEN, false).await.expect("expired");
    assert_eq!(s.count(), 2);
    api.get_user_guilds(USER_ID, "another-token", false).await.expect("other token");
    assert_eq!(s.count(), 3, "a different token does not reuse the cache");
    api.forget_user(USER_ID);
    api.get_user_guilds(USER_ID, "another-token", false).await.expect("forgotten");
    assert_eq!(s.count(), 4);
}

#[tokio::test]
async fn user_guilds_map_401_and_429_and_serve_an_old_list_only_when_allowed() {
    let reply: Arc<Mutex<Reply>> = Arc::new(Mutex::new(ok(json!([{ "id": g::ZETA, "name": "Z", "owner": true, "permissions": "0" }]))));
    let r2 = reply.clone();
    let s = stub(Arc::new(move |_, _| lock(&r2).clone())).await;
    let t = clock(0.0);
    let api = api(&s.base, Some(t.clone()), None);
    api.get_user_guilds(USER_ID, TOKEN, false).await.expect("first");
    advance(&t, 61_000.0);
    for (status, body, code) in [(429, json!({ "retry_after": 2.5 }), "rate_limited"), (503, json!({}), "unavailable")] {
        *lock(&reply) = (status, body.to_string(), Vec::new());
        let e = api.get_user_guilds(USER_ID, TOKEN, false).await.expect_err("authoritative callers get the error");
        assert_eq!(e.code, code);
        if status == 429 {
            assert_eq!(e.retry_after, Some(3), "rounded up");
        }
        let stale = api.get_user_guilds(USER_ID, TOKEN, true).await.expect("stale list for display");
        assert_eq!(stale[0].id, g::ZETA);
    }
    let e = api.get_user_guilds(OWNER_ID, TOKEN, true).await.expect_err("nothing old for another user");
    assert_eq!((e.code.as_str(), e.status), ("unavailable", 503));

    // Callers sharing one in-flight request each decide for themselves.
    *lock(&reply) = (429, json!({ "retry_after": 2.5 }).to_string(), Vec::new());
    let before = s.count();
    let (strict, lenient) = tokio::join!(api.get_user_guilds(USER_ID, TOKEN, false), api.get_user_guilds(USER_ID, TOKEN, true));
    assert_eq!(s.count(), before + 1, "one request");
    assert_eq!(strict.expect_err("strict").retry_after, Some(3));
    assert!(lenient.is_ok());

    advance(&t, 5.0 * 60_000.0);
    assert_eq!(api.get_user_guilds(USER_ID, TOKEN, true).await.expect_err("too old").code, "rate_limited");

    *lock(&reply) = (401, json!({ "message": "401: Unauthorized" }).to_string(), Vec::new());
    assert_eq!(api.get_user_guilds(USER_ID, TOKEN, true).await.expect_err("401").code, "unauthenticated");
    *lock(&reply) = ok(json!([{ "id": g::ZETA }]));
    api.get_user_guilds(USER_ID, TOKEN, false).await.expect("fresh");
    let cached = s.count();
    api.get_user_guilds(USER_ID, TOKEN, false).await.expect("cached");
    api.get_user_guilds(USER_ID, TOKEN, true).await.expect("cached");
    assert_eq!(s.count(), cached);
}

#[tokio::test]
async fn transport_failures_and_redirects_are_unavailable() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let a = l.local_addr().expect("addr");
        drop(l);
        a
    };
    let down = api(&format!("http://{dead}/api/v10"), None, None);
    let e = down.get_user_guilds(USER_ID, TOKEN, false).await.expect_err("down");
    assert_eq!((e.code.as_str(), e.status, e.message.as_str()), ("unavailable", 0, "Discord request failed: fetch failed"));

    let s = stub(Arc::new(|_, _| (302, String::new(), vec![("location".into(), "https://example.com/".into())]))).await;
    let e = api(&s.base, None, None).get_current_user(TOKEN).await.expect_err("redirect");
    assert_eq!(e.message, "Discord request failed: fetch failed");
}

fn page1() -> Vec<Value> {
    (0..200u64).map(|i| json!({ "id": (300_000_000_000_000_000u64 + i).to_string() })).collect()
}

#[tokio::test]
async fn bot_guilds_page_through_the_list_and_misses_are_double_checked() {
    let s = stub(Arc::new(|_, path| {
        if path.starts_with("/users/@me/guilds") {
            return ok(if path.ends_with("after=0") { json!(page1()) } else { json!([{ "id": g::ZETA }]) });
        }
        if path == format!("/guilds/{}", g::BETA) {
            return ok(json!({ "id": g::BETA }));
        }
        if path == format!("/guilds/{}", g::ALPHA) {
            return (403, json!({ "code": 50001 }).to_string(), Vec::new());
        }
        (404, json!({ "code": 10004 }).to_string(), Vec::new())
    }))
    .await;
    let api = api(&s.base, Some(clock(0.0)), None);
    assert!(api.is_bot_in_guild(g::ZETA, true).await.expect("zeta"));
    assert_eq!(s.count(), 2, "two pages");
    assert!(s.calls()[1].path.contains("after=300000000000000199"));
    assert_eq!(s.calls()[0].auth, "Bot bot-token");
    assert!(api.is_bot_in_guild("300000000000000005", true).await.expect("cached"));
    assert_eq!(s.count(), 2);
    assert!(!api.is_bot_in_guild(g::ALPHA, false).await.expect("no verify"));
    assert_eq!(s.count(), 2, "verify:false never double-checks");
    assert!(!api.is_bot_in_guild(g::ALPHA, true).await.expect("403"));
    let n = s.count();
    assert!(!api.is_bot_in_guild(g::ALPHA, true).await.expect("remembered miss"));
    assert_eq!(s.count(), n, "a miss is remembered for a few seconds");
    assert!(!api.is_bot_in_guild(g::STRANGER, true).await.expect("404"));
    assert!(api.is_bot_in_guild(g::BETA, true).await.expect("beta"));
    let n = s.count();
    assert!(api.is_bot_in_guild(g::BETA, true).await.expect("remembered"));
    assert_eq!(s.count(), n, "a confirmed guild is remembered");
    assert!(!api.is_bot_in_guild("not-a-snowflake", true).await.expect("bad id"));
    assert_eq!(api.get_bot_guilds().await.expect("list").len(), 201);
    assert_eq!(api.get_bot_guild_ids().await.expect("ids").len(), 202, "the confirmed guild joins the id set");
}

/// A bot guild list whose page 2 may answer a 429 first.
async fn paged(limit: impl Fn() -> Option<Reply> + Send + Sync + 'static) -> Stub {
    stub(Arc::new(move |_, path| {
        if path.ends_with("after=0") {
            return ok(json!(page1()));
        }
        limit().unwrap_or_else(|| ok(json!([{ "id": g::ZETA }])))
    }))
    .await
}

#[tokio::test]
async fn the_bot_guild_load_waits_out_short_429s_and_gives_up_on_long_ones() {
    let limits = Arc::new(AtomicUsize::new(1));
    let l2 = limits.clone();
    let s = paged(move || (l2.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()).then(|| (429, json!({ "retry_after": 0.01 }).to_string(), Vec::new()))).await;
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let a = api(&s.base, None, Some(sleeps.clone()));
    let ids = a.get_bot_guild_ids().await.expect("ids");
    assert_eq!(ids.len(), 201);
    assert_eq!(s.count(), 3, "page 1, the limited page 2, page 2 again");
    assert_eq!(s.calls()[2].path, s.calls()[1].path);
    assert_eq!(*lock(&sleeps), vec![1000], "retry_after is rounded up to whole seconds");
    assert!(a.is_bot_in_guild(g::ZETA, false).await.expect("cached"));
    assert_eq!(s.count(), 3);

    let long = paged(|| Some((429, json!({ "retry_after": 30 }).to_string(), Vec::new()))).await;
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let e = api(&long.base, None, Some(sleeps.clone())).get_bot_guild_ids().await.expect_err("long");
    assert_eq!((e.code.as_str(), e.retry_after), ("rate_limited", Some(30)));
    assert_eq!(long.count(), 2);
    assert!(lock(&sleeps).is_empty());

    let stuck = paged(|| Some((429, json!({ "retry_after": 10 }).to_string(), Vec::new()))).await;
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    assert_eq!(api(&stuck.base, None, Some(sleeps.clone())).get_bot_guild_ids().await.expect_err("stuck").code, "rate_limited");
    assert_eq!(*lock(&sleeps), vec![10_000, 10_000, 10_000]);
    assert_eq!(stuck.count(), 5, "page 1, then the limited page 2 once and three retries");

    let down = paged(|| Some((503, "{}".into(), Vec::new()))).await;
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    assert_eq!(api(&down.base, None, Some(sleeps.clone())).get_bot_guild_ids().await.expect_err("down").code, "unavailable");
    assert!(lock(&sleeps).is_empty());
    assert_eq!(down.count(), 2);
}

#[tokio::test]
async fn the_default_pause_between_bot_guild_retries_is_a_real_wait() {
    let limits = Arc::new(AtomicUsize::new(1));
    let l2 = limits.clone();
    let s = paged(move || (l2.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()).then(|| (429, json!({ "retry_after": 1 }).to_string(), Vec::new()))).await;
    let a = api(&s.base, None, None);
    let started = Instant::now();
    assert_eq!(a.get_bot_guild_ids().await.expect("ids").len(), 201);
    assert!(started.elapsed() >= Duration::from_millis(900), "waited about retry_after seconds");
    assert_eq!(s.count(), 3);
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_previous_bot_guild_list() {
    let fail = Arc::new(Mutex::new(false));
    let f2 = fail.clone();
    let s = stub(Arc::new(move |_, _| if *lock(&f2) { (503, "{}".into(), Vec::new()) } else { ok(json!([{ "id": g::ZETA }])) })).await;
    let t = clock(0.0);
    let logger = Arc::new(RecordingLogger::new());
    let mut opts = DiscordApiOptions::new(DiscordApiConfig { client_id: BOT_ID.into(), client_secret: "s".into(), bot_token: "b".into(), redirect_uri: "r".into() });
    opts.api_base = s.base.clone();
    let t2 = t.clone();
    opts.now = Some(Arc::new(move || *lock(&t2)));
    opts.logger = Some(logger.clone());
    let a = DiscordApi::new(opts);
    assert!(a.is_bot_in_guild(g::ZETA, false).await.expect("loaded"));
    *lock(&fail) = true;
    advance(&t, 61_000.0);
    assert!(a.is_bot_in_guild(g::ZETA, false).await.expect("old list"));
    assert_eq!(logger.lines().len(), 1);
    assert!(logger.lines()[0].contains("could not refresh the bot's server list (Discord returned HTTP 503 for GET /users/@me/guilds); using the previous one."));
    let n = s.count();
    assert!(a.is_bot_in_guild(g::ZETA, false).await.expect("no retry before another TTL"));
    assert_eq!(s.count(), n);
}

fn resources_stub(with_lists: bool) -> Handler {
    Arc::new(move |_, path| {
        let roles = json!([{ "id": g::ZETA, "name": "@everyone", "position": 0, "permissions": "1024" }, { "id": "410000000000000001", "name": "Bot", "position": 5, "managed": true, "permissions": "268435456" }]);
        if path == format!("/guilds/{}?with_counts=true", g::ZETA) {
            return ok(if with_lists { json!({ "id": g::ZETA, "name": "Zeta", "roles": roles, "emojis": [] }) } else { json!({ "id": g::ZETA, "name": "Zeta" }) });
        }
        if path == format!("/guilds/{}/channels", g::ZETA) {
            return ok(json!([{ "id": "310000000000000001", "type": 0, "name": "general", "position": 0 }]));
        }
        if path == format!("/guilds/{}/members/123456789012345678", g::ZETA) {
            return ok(json!({ "roles": ["410000000000000001"] }));
        }
        if path == format!("/guilds/{}/roles", g::ZETA) {
            return ok(roles);
        }
        if path == format!("/guilds/{}/emojis", g::ZETA) {
            return ok(json!([]));
        }
        (404, json!({ "code": 10004 }).to_string(), Vec::new())
    })
}

#[tokio::test]
async fn guild_resources_are_cached_30_s_shared_and_reloaded_on_fresh() {
    let s = stub(resources_stub(true)).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    let (x, y) = tokio::join!(a.get_guild_resources(g::ZETA, false), a.get_guild_resources(g::ZETA, false));
    let (x, y) = (x.expect("x"), y.expect("y"));
    assert!(Arc::ptr_eq(&x, &y));
    assert_eq!(s.count(), 3, "one load");
    advance(&t, 29_000.0);
    assert!(Arc::ptr_eq(&a.get_guild_resources(g::ZETA, false).await.expect("cached"), &x));
    assert_eq!(s.count(), 3);
    a.get_guild_resources(g::ZETA, true).await.expect("fresh");
    assert_eq!(s.count(), 6, "fresh skips the cache");
    advance(&t, 29_000.0);
    a.get_guild_resources(g::ZETA, false).await.expect("refreshed");
    assert_eq!(s.count(), 6, "the fresh load refreshed the cache");
    advance(&t, 31_000.0);
    a.get_guild_resources(g::ZETA, false).await.expect("expired");
    assert_eq!(s.count(), 9);
    let e = a.get_guild_resources("nope", false).await.expect_err("bad id");
    assert_eq!(e.code, "not_found");
    assert_eq!(s.count(), 9, "bad ids never reach Discord");

    // A plain caller shares a fresh load in flight; a fresh caller never reuses a plain one.
    advance(&t, 31_000.0);
    let (f, p) = tokio::join!(a.get_guild_resources(g::ZETA, true), a.get_guild_resources(g::ZETA, false));
    assert!(Arc::ptr_eq(&f.expect("fresh"), &p.expect("plain")));
    assert_eq!(s.count(), 12);
    advance(&t, 31_000.0);
    let (p, f) = tokio::join!(a.get_guild_resources(g::ZETA, false), a.get_guild_resources(g::ZETA, true));
    assert!(!Arc::ptr_eq(&p.expect("plain"), &f.expect("fresh")));
    assert_eq!(s.count(), 18);

    let lists = stub(resources_stub(false)).await;
    let res = api(&lists.base, None, None).get_guild_resources(g::ZETA, false).await.expect("lists");
    assert!(lists.calls().iter().any(|c| c.path.ends_with("/roles")) && lists.calls().iter().any(|c| c.path.ends_with("/emojis")));
    assert_eq!(res.roles.len(), 2);
    assert!(res.bot.manage_roles && !res.bot.create_invite);
}

#[tokio::test]
async fn oauth_exchange_and_revocation_use_the_injected_endpoints() {
    let s = stub(Arc::new(|method, path| match (method, path) {
        ("POST", "/oauth2/token") => ok(json!({ "access_token": "tok", "token_type": "Bearer", "scope": "identify guilds", "expires_in": 600 })),
        ("POST", "/oauth2/token/revoke") => ok(json!({})),
        _ => (404, "{}".into(), Vec::new()),
    }))
    .await;
    let a = api(&s.base, None, None);
    let grant = a.exchange_code("abc").await.expect("grant");
    assert_eq!((grant.access_token.as_str(), grant.expires_in), ("tok", 600.0));
    assert!(a.revoke_token("tok").await);
    let calls = s.calls();
    assert!(calls[0].auth.starts_with("Basic "));
    assert_eq!(calls.len(), 2);
}

// ── Login callbacks in parallel (test/sec-dashboard.test.js) ─────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_callbacks_of_a_fresh_source_cannot_all_skip_the_overall_allowance() {
    let discord = Arc::new(LoginDiscord::new(false));
    let t = Arc::new(Mutex::new(NOW));
    let t2 = t.clone();
    let deps = DashboardDeps {
        discord: discord.clone(),
        settings: Arc::new(NoSettings),
        records: None,
        bot_joins: None,
        collections: None,
        now: Some(Arc::new(move || *lock(&t2))),
    };
    let mut opts = ServerOptions::new(config(&[]), deps);
    opts.logger = Some(Arc::new(RecordingLogger::new()));
    let router = create_dashboard_router(opts).expect("router");
    let mut pending = Vec::new();
    for s in 0..7 {
        for i in 0..10 {
            let router = router.clone();
            pending.push(tokio::spawn(async move {
                let headers = vec![("cookie", "dash_oauth_state=abc".to_string()), ("x-forwarded-for", format!("198.51.100.{}", 40 + s))];
                call(&router, "GET", &format!("/auth/callback?code=slow{s}x{i}&state=abc"), &headers, None).await.status
            }));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock(&discord.exchanges).len() < 67 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(lock(&discord.exchanges).len(), 67, "7 first callbacks are free and 63 spend the allowance of 60");
    discord.release();
    let mut statuses = Vec::new();
    for p in pending {
        statuses.push(p.await.expect("join"));
    }
    assert_eq!(statuses.iter().filter(|s| **s == 429).count(), 3);
}

// ── Pure helpers ─────────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn gzip_and_etag_negotiation() {
    assert!(accepts_gzip(Some("br, gzip;q=0.8")));
    assert!(accepts_gzip(Some("GZIP")));
    assert!(!accepts_gzip(Some("gzip;q=0")));
    assert!(!accepts_gzip(Some("deflate")));
    assert!(!accepts_gzip(None));
    assert!(etag_matches(Some("W/\"a\""), &["\"a\""]));
    assert!(etag_matches(Some("\"x\", \"b\""), &["\"a\"", "\"b\""]));
    assert!(etag_matches(Some(" * "), &["\"a\""]));
    assert!(!etag_matches(Some("  "), &["\"a\""]));
    assert!(!etag_matches(None, &["\"a\""]));
}

#[test]
fn contact_html_links_an_email_or_url_and_escapes_it() {
    use dash_auth::config::{Contact, ContactKind};
    use dash_server::server::contact_html;
    assert_eq!(contact_html(Some(&Contact { kind: ContactKind::Email, value: "a@b.co".into() })), "<a href=\"mailto:a@b.co\">a@b.co</a>");
    assert_eq!(
        contact_html(Some(&Contact { kind: ContactKind::Url, value: "https://x.co/?a=1&b=\"2\"".into() })),
        "<a href=\"https://x.co/?a=1&amp;b=&quot;2&quot;\">https://x.co/?a=1&amp;b=&quot;2&quot;</a>"
    );
    assert!(!contact_html(None).contains('<'));
}

#[test]
fn limits_and_a_short_session_secret() {
    assert_eq!(dash_server::BODY_LIMIT_BYTES, 256 * 1024);
    assert_eq!(dash_server::RECORD_BODY_LIMIT_BYTES, 128 * 1024);
    let f = create_fakes();
    let mut cfg = config(&[]);
    cfg.session_secret = String::new();
    let deps = DashboardDeps { discord: f.discord.clone(), settings: f.settings.clone(), records: None, bot_joins: None, collections: None, now: None };
    assert!(create_dashboard_router(ServerOptions::new(cfg, deps)).is_err());
}

// ── Speed-ups: refresh-ahead, stale-while-revalidate, warm-up (test/dashboard.test.js "Speed-ups") ───────────────

/// Lets background tasks run: the refreshes are spawned, so a short real pause is enough.
async fn settle() {
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn user_list(name: &str) -> Reply {
    ok(json!([{ "id": g::ZETA, "name": name, "icon": null, "owner": true, "permissions": "0" }]))
}

fn first_name(list: &[dash_server::OAuthGuild]) -> String {
    list[0].name.clone()
}

#[tokio::test]
async fn user_guilds_refresh_ahead_and_share_one_background_refresh() {
    let name = Arc::new(Mutex::new("v1".to_string()));
    let n2 = name.clone();
    let gate = closed_gate();
    let s = stub_with(Arc::new(move |_, _| user_list(&lock(&n2))), Some(gate.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    open(&gate);
    assert_eq!(first_name(&a.get_user_guilds(USER_ID, TOKEN, false).await.expect("v1")), "v1");
    advance(&t, 29_000.0);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("cached");
    settle().await;
    assert_eq!(s.count(), 1, "under half the TTL: nothing in the background");

    close(&gate);
    *lock(&name) = "v2".into();
    advance(&t, 2_000.0); // 31 s old
    let served = a.get_user_guilds(USER_ID, TOKEN, false).await.expect("served at once");
    assert_eq!(first_name(&served), "v1", "under the TTL, so served while the refresh runs");
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("again");
    a.get_user_guilds(USER_ID, TOKEN, true).await.expect("again");
    settle().await;
    assert_eq!(s.count(), 2, "one background refresh, however many callers");
    open(&gate);
    settle().await;
    advance(&t, 1_000.0);
    assert_eq!(first_name(&a.get_user_guilds(USER_ID, TOKEN, false).await.expect("v2")), "v2", "the refreshed entry replaced it");
    assert_eq!(s.count(), 2);
}

#[tokio::test]
async fn a_plain_caller_waits_instead_of_taking_a_list_older_than_60_s() {
    let gate = closed_gate();
    let s = stub_with(Arc::new(|_, _| user_list("v3")), Some(gate.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    open(&gate);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("loaded");
    close(&gate);
    advance(&t, 31_000.0);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("ahead");
    settle().await;
    assert_eq!(s.count(), 2);
    advance(&t, 30_000.0); // the entry is now 61 s old
    let a2 = a.clone();
    let waiting = tokio::spawn(async move { a2.get_user_guilds(USER_ID, TOKEN, false).await });
    settle().await;
    assert!(!waiting.is_finished(), "a non-stale caller waits instead of taking a list older than 60 s");
    open(&gate);
    assert_eq!(first_name(&waiting.await.expect("join").expect("v3")), "v3");
    assert_eq!(s.count(), 2, "it joined the refresh already in flight");
}

#[tokio::test]
async fn a_failed_background_refresh_keeps_the_entry_and_spaces_its_retries() {
    let fail = Arc::new(Mutex::new(false));
    let f2 = fail.clone();
    let s = stub(Arc::new(move |_, _| if *lock(&f2) { (503, "{}".into(), Vec::new()) } else { user_list("keep") })).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("loaded");
    *lock(&fail) = true;
    advance(&t, 31_000.0);
    assert_eq!(first_name(&a.get_user_guilds(USER_ID, TOKEN, false).await.expect("kept")), "keep");
    settle().await;
    assert_eq!(s.count(), 2);
    advance(&t, 1_000.0);
    assert_eq!(first_name(&a.get_user_guilds(USER_ID, TOKEN, false).await.expect("still there")), "keep");
    settle().await;
    assert_eq!(s.count(), 2, "no retry on every request");
    advance(&t, 10_000.0);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("kept");
    settle().await;
    assert_eq!(s.count(), 3, "retried after the pause");
}

#[tokio::test]
async fn forget_user_stops_a_refresh_in_flight_from_putting_the_list_back() {
    let gate = closed_gate();
    let s = stub_with(Arc::new(|_, _| user_list("late")), Some(gate.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    open(&gate);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("loaded");
    close(&gate);
    advance(&t, 31_000.0);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("starts the refresh");
    settle().await;
    assert_eq!(s.count(), 2);
    a.forget_user(USER_ID); // logout while it is in flight
    open(&gate);
    settle().await;
    advance(&t, 1_000.0);
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("loads again");
    assert_eq!(s.count(), 3, "the late answer was dropped: the next call loads again");

    // The same for a warm-up still running at logout.
    let gate = closed_gate();
    let w = stub_with(Arc::new(|_, _| user_list("warm")), Some(gate.clone())).await;
    let a = api(&w.base, Some(t.clone()), None);
    a.warm_user_guilds(USER_ID, TOKEN);
    settle().await;
    a.forget_user(USER_ID);
    open(&gate);
    settle().await;
    a.get_user_guilds(USER_ID, TOKEN, false).await.expect("loads");
    assert_eq!(w.count(), 2);
}

#[tokio::test]
async fn warm_user_guilds_loads_once_with_the_given_token_into_its_own_slot() {
    let s = stub(Arc::new(|_, _| user_list("warm"))).await;
    let a = api(&s.base, Some(clock(0.0)), None);
    a.warm_user_guilds(USER_ID, TOKEN);
    a.warm_user_guilds(USER_ID, TOKEN);
    settle().await;
    assert_eq!(s.count(), 1, "one load");
    assert_eq!(s.calls()[0].auth, format!("Bearer {TOKEN}"));
    assert_eq!(first_name(&a.get_user_guilds(USER_ID, TOKEN, false).await.expect("cached")), "warm");
    assert_eq!(s.count(), 1, "the first page after login is served from it");
    a.warm_user_guilds(USER_ID, TOKEN);
    settle().await;
    assert_eq!(s.count(), 1, "a current entry is not reloaded");
    a.get_user_guilds(USER_ID, "another-token", false).await.expect("other token");
    assert_eq!(s.count(), 2, "another token never uses it");
}

fn bot_stub(list: Arc<Mutex<Vec<&'static str>>>, beta_in: Arc<Mutex<bool>>) -> Handler {
    Arc::new(move |_, path| {
        if path.starts_with("/users/@me/guilds") {
            return ok(Value::Array(lock(&list).iter().map(|id| json!({ "id": id, "name": format!("g{}", &id[id.len() - 2..]) })).collect()));
        }
        if path == format!("/guilds/{}", g::BETA) {
            return if *lock(&beta_in) { ok(json!({ "id": g::BETA })) } else { (403, json!({ "code": 50001 }).to_string(), Vec::new()) };
        }
        (404, json!({ "code": 10004 }).to_string(), Vec::new())
    })
}

#[tokio::test]
async fn bot_guild_list_serves_stale_while_it_refreshes_and_waits_past_the_bound() {
    let list = Arc::new(Mutex::new(vec![g::ZETA]));
    let gate = closed_gate();
    let s = stub_with(bot_stub(list.clone(), Arc::new(Mutex::new(false))), Some(gate.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    open(&gate);
    assert!(a.is_bot_in_guild_swr(g::ZETA, false).await.expect("first"), "nothing cached: it waits for the first list");
    assert_eq!(s.count(), 1);
    close(&gate);

    *lock(&list) = vec![g::ZETA, g::ALPHA];
    advance(&t, 61_000.0);
    assert!(!a.is_bot_in_guild_swr(g::ALPHA, false).await.expect("stale"), "the old list is served at once");
    assert!(a.is_bot_in_guild_swr(g::ZETA, false).await.expect("stale"));
    settle().await;
    assert_eq!(s.count(), 2, "one background refresh");
    open(&gate);
    settle().await;
    assert!(a.is_bot_in_guild_swr(g::ALPHA, false).await.expect("fresh"), "the refresh landed");
    assert_eq!(a.get_bot_guilds_swr().await.expect("entries").len(), 2);

    // Without swr (the default) a list past 60 s is waited for, as before.
    *lock(&list) = vec![g::ZETA];
    advance(&t, 61_000.0);
    assert!(!a.is_bot_in_guild(g::ALPHA, false).await.expect("reloaded"));
    assert_eq!(s.count(), 3);

    // Past the max-stale bound even an swr caller waits.
    *lock(&list) = vec![g::ZETA, g::BETA];
    close(&gate);
    advance(&t, 10.0 * 60_000.0 + 1_000.0);
    let a2 = a.clone();
    let waiting = tokio::spawn(async move { a2.is_bot_in_guild_swr(g::BETA, false).await });
    settle().await;
    assert!(!waiting.is_finished(), "older than the bound: it waits");
    open(&gate);
    assert!(waiting.await.expect("join").expect("loaded"));
}

#[tokio::test]
async fn bot_guild_list_refreshes_ahead_past_half_the_ttl_and_never_serves_past_it() {
    let list = Arc::new(Mutex::new(vec![g::ZETA]));
    let gate = closed_gate();
    let s = stub_with(bot_stub(list.clone(), Arc::new(Mutex::new(false))), Some(gate.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    open(&gate);
    assert!(!a.is_bot_in_guild(g::ALPHA, false).await.expect("first"));
    assert_eq!(s.count(), 1);

    *lock(&list) = vec![g::ZETA, g::ALPHA];
    advance(&t, 20_000.0);
    assert!(!a.is_bot_in_guild(g::ALPHA, false).await.expect("under half"), "served, no refresh");
    settle().await;
    assert_eq!(s.count(), 1);

    close(&gate);
    advance(&t, 11_000.0); // 31 s old
    assert!(!a.is_bot_in_guild(g::ALPHA, false).await.expect("ahead"), "past half: served at once");
    assert!(!a.is_bot_in_guild(g::ALPHA, false).await.expect("ahead"));
    settle().await;
    assert_eq!(s.count(), 2, "one background refresh through the single in-flight slot");
    open(&gate);
    settle().await;
    advance(&t, 29_000.0); // 60 s after the first load
    assert!(a.is_bot_in_guild(g::ALPHA, false).await.expect("landed"), "the refresh landed before the old list expired");
    assert_eq!(s.count(), 2, "nothing waited or loaded again");
}

#[tokio::test]
async fn a_removed_bot_answers_bot_not_in_guild_on_the_first_read_past_the_ttl() {
    let list = Arc::new(Mutex::new(vec![g::ZETA]));
    let s = stub(bot_stub(list.clone(), Arc::new(Mutex::new(false)))).await;
    let t = clock(0.0);
    let (router, _probe) = probe_router_over(Some(api(&s.base, Some(t.clone()), None)));
    let cookie = session_cookie(json!({}));
    let get = |path: String| {
        let router = router.clone();
        let cookie = cookie.clone();
        async move { call(&router, "GET", &path, &[("cookie", cookie)], None).await }
    };
    assert_eq!(get(format!("/api/guilds/{}/settings", g::ZETA)).await.status, 200);
    lock(&list).clear(); // the bot is removed
    advance(&t, 61_000.0);
    let r = get(format!("/api/guilds/{}/settings", g::ZETA)).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"], "bot_not_in_guild");
    let r = get(format!("/api/guilds/{}/resources", g::ZETA)).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"], "bot_not_in_guild");
}

#[tokio::test]
async fn bot_guild_misses_are_still_checked_live_on_a_stale_list_and_a_failed_refresh_keeps_it() {
    let list = Arc::new(Mutex::new(vec![g::ZETA]));
    let beta_in = Arc::new(Mutex::new(true));
    let s = stub(bot_stub(list.clone(), beta_in.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    assert!(a.is_bot_in_guild(g::ZETA, false).await.expect("loaded"));
    advance(&t, 120_000.0);
    assert!(a.is_bot_in_guild_swr(g::BETA, true).await.expect("live"), "a fresh invite shows at once");
    assert!(s.calls().iter().any(|c| c.path == format!("/guilds/{}", g::BETA)), "GET /guilds/{{id}} ran");
    *lock(&beta_in) = false;
    assert!(!a.is_bot_in_guild_swr(g::ALPHA, true).await.expect("miss"));

    let fail = Arc::new(Mutex::new(false));
    let f2 = fail.clone();
    let inner = bot_stub(list.clone(), beta_in.clone());
    let d = stub(Arc::new(move |m, p| if *lock(&f2) { (503, "{}".into(), Vec::new()) } else { inner(m, p) })).await;
    let t = clock(0.0);
    let a = api(&d.base, Some(t.clone()), None);
    assert!(a.is_bot_in_guild(g::ZETA, false).await.expect("loaded"));
    *lock(&fail) = true;
    advance(&t, 61_000.0);
    assert!(a.is_bot_in_guild_swr(g::ZETA, false).await.expect("stale list"));
    settle().await;
    let spent = d.count();
    assert!(a.is_bot_in_guild_swr(g::ZETA, false).await.expect("the previous list stays"));
    settle().await;
    assert_eq!(d.count(), spent, "and the retry waits another TTL");
}

#[tokio::test]
async fn guild_resources_display_reads_are_stale_while_revalidate_and_every_other_read_is_not() {
    let gate = closed_gate();
    let s = stub_with(resources_stub(true), Some(gate.clone())).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    open(&gate);
    let first = a.get_guild_resources_display(g::ZETA).await.expect("first");
    assert_eq!(s.count(), 3, "nothing cached: it waits for a live load");
    close(&gate);

    advance(&t, 31_000.0);
    assert!(Arc::ptr_eq(&a.get_guild_resources_display(g::ZETA).await.expect("stale"), &first), "older than 30 s: served at once");
    assert!(Arc::ptr_eq(&a.get_guild_resources_display(g::ZETA).await.expect("stale"), &first));
    settle().await;
    assert_eq!(s.count(), 6, "one background reload");
    open(&gate);
    settle().await;
    let second = a.get_guild_resources_display(g::ZETA).await.expect("reloaded");
    assert!(!Arc::ptr_eq(&second, &first), "the reload replaced the entry");
    assert_eq!(s.count(), 6);

    // The plain read (every id check) never takes an entry older than 30 s.
    advance(&t, 31_000.0);
    close(&gate);
    let a2 = a.clone();
    let strict = tokio::spawn(async move { a2.get_guild_resources(g::ZETA, false).await });
    settle().await;
    assert!(!strict.is_finished(), "no swr: it waits for a live load");
    open(&gate);
    let third = strict.await.expect("join").expect("live");
    assert!(!Arc::ptr_eq(&third, &second));

    // fresh is live, and does not join a plain background load.
    advance(&t, 31_000.0);
    close(&gate);
    a.get_guild_resources_display(g::ZETA).await.expect("stale");
    settle().await;
    let before = s.count();
    let a3 = a.clone();
    let fresh = tokio::spawn(async move { a3.get_guild_resources(g::ZETA, true).await });
    settle().await;
    assert!(s.count() > before, "a separate live load started");
    open(&gate);
    fresh.await.expect("join").expect("fresh");

    // Past the max-stale bound the display read waits too.
    advance(&t, 10.0 * 60_000.0 + 1_000.0);
    close(&gate);
    let a4 = a.clone();
    let late = tokio::spawn(async move { a4.get_guild_resources_display(g::ZETA).await });
    settle().await;
    assert!(!late.is_finished());
    open(&gate);
    late.await.expect("join").expect("loaded");
}

#[tokio::test]
async fn a_failed_background_resources_reload_keeps_the_entry_and_spaces_its_retries() {
    let down = Arc::new(Mutex::new(false));
    let d2 = down.clone();
    let inner = resources_stub(true);
    let s = stub(Arc::new(move |m, p| if *lock(&d2) { (503, "{}".into(), Vec::new()) } else { inner(m, p) })).await;
    let t = clock(0.0);
    let a = api(&s.base, Some(t.clone()), None);
    let first = a.get_guild_resources(g::ZETA, false).await.expect("loaded");
    *lock(&down) = true;
    advance(&t, 31_000.0);
    assert!(Arc::ptr_eq(&a.get_guild_resources_display(g::ZETA).await.expect("kept"), &first));
    settle().await;
    let spent = s.count();
    assert!(Arc::ptr_eq(&a.get_guild_resources_display(g::ZETA).await.expect("still served"), &first));
    settle().await;
    assert_eq!(s.count(), spent, "the retry waits another 30 s");
    let e = a.get_guild_resources(g::ZETA, false).await.expect_err("a strict caller is not given the old entry");
    assert_eq!(e.code, "unavailable");
}

// ── The server's use of them, and the gzip copy of the catalog ────────────────────────────────────────────────────

type DErr = dash_collections::discord::DiscordApiError;

/// A ServerDiscord that records which flavour of read the server asked for, over the usual fakes.
struct Probe {
    inner: Arc<FakeDiscord>,
    /// When set, the bot's presence and server list come from this real client (over a stub) instead of the fakes.
    real: Option<DiscordApi>,
    log: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl DashboardDiscord for Probe {
    async fn get_guild_resources(&self, guild_id: &str, fresh: bool) -> Result<Arc<dash_collections::discord::DiscordResources>, DErr> {
        lock(&self.log).push(format!("resources:fresh={fresh}:swr=false"));
        self.inner.get_guild_resources(guild_id, fresh).await
    }
    async fn get_guild_resources_display(&self, guild_id: &str) -> Result<Arc<dash_collections::discord::DiscordResources>, DErr> {
        lock(&self.log).push("resources:fresh=false:swr=true".into());
        self.inner.get_guild_resources(guild_id, false).await
    }
    async fn get_member(&self, g: &str, u: &str) -> Result<Option<dash_collections::discord::MemberRoles>, DErr> {
        self.inner.get_member(g, u).await
    }
    async fn search_members(&self, g: &str, q: &str) -> Result<Vec<dash_collections::discord::MemberProfile>, DErr> {
        self.inner.search_members(g, q).await
    }
    async fn get_users(&self, ids: &[String]) -> Result<std::collections::HashMap<String, Option<dash_collections::discord::MemberProfile>>, DErr> {
        self.inner.get_users(ids).await
    }
    async fn send_message(&self, c: &str, p: Value) -> Result<dash_collections::discord::MessageRef, DErr> {
        self.inner.send_message(c, p).await
    }
    async fn edit_message(&self, c: &str, m: &str, p: Value) -> Result<dash_collections::discord::MessageRef, DErr> {
        self.inner.edit_message(c, m, p).await
    }
    async fn delete_message(&self, c: &str, m: &str) -> Result<(), DErr> {
        self.inner.delete_message(c, m).await
    }
    async fn create_invite(&self, c: &str, r: &str) -> Result<dash_collections::discord::InviteRef, DErr> {
        self.inner.create_invite(c, r).await
    }
}

#[async_trait::async_trait]
impl ServerDiscord for Probe {
    async fn get_current_user(&self, t: &str) -> Result<dash_server::CurrentUser, DErr> {
        self.inner.get_current_user(t).await
    }
    async fn get_user_guilds(&self, u: &str, t: &str, s: bool) -> Result<Arc<Vec<dash_server::OAuthGuild>>, DErr> {
        self.inner.get_user_guilds(u, t, s).await
    }
    fn forget_user(&self, u: &str) {
        self.inner.forget_user(u);
    }
    fn warm_user_guilds(&self, u: &str, t: &str) {
        lock(&self.log).push(format!("warm:{u}:{t}"));
    }
    fn prefetch_bot_guilds(&self) {
        lock(&self.log).push("prefetch".into());
    }
    async fn get_bot_guilds(&self) -> Result<Arc<Vec<dash_server::BotGuild>>, DErr> {
        lock(&self.log).push("botGuilds:swr=false".into());
        if let Some(r) = &self.real {
            return r.get_bot_guilds().await;
        }
        self.inner.get_bot_guilds().await
    }
    async fn get_bot_guilds_swr(&self) -> Result<Arc<Vec<dash_server::BotGuild>>, DErr> {
        lock(&self.log).push("botGuilds:swr=true".into());
        if let Some(r) = &self.real {
            return r.get_bot_guilds_swr().await;
        }
        self.inner.get_bot_guilds().await
    }
    async fn is_bot_in_guild(&self, g: &str, v: bool) -> Result<bool, DErr> {
        lock(&self.log).push(format!("presence:verify={v}:swr=false"));
        if let Some(r) = &self.real {
            return r.is_bot_in_guild(g, v).await;
        }
        self.inner.is_bot_in_guild(g, v).await
    }
    async fn is_bot_in_guild_swr(&self, g: &str, v: bool) -> Result<bool, DErr> {
        lock(&self.log).push(format!("presence:verify={v}:swr=true"));
        if let Some(r) = &self.real {
            return r.is_bot_in_guild_swr(g, v).await;
        }
        self.inner.is_bot_in_guild(g, v).await
    }
    fn invite_url(&self, g: Option<&str>) -> String {
        self.inner.invite_url(g)
    }
    async fn exchange_code(&self, c: &str) -> anyhow::Result<dash_auth::auth::TokenGrant> {
        self.inner.exchange_code(c).await
    }
}

fn probe_router() -> (Router, Arc<Probe>) {
    probe_router_over(None)
}

fn probe_router_over(real: Option<DiscordApi>) -> (Router, Arc<Probe>) {
    let f = create_fakes();
    let probe = Arc::new(Probe { inner: f.discord.clone(), real, log: Mutex::new(Vec::new()) });
    let deps = DashboardDeps {
        discord: probe.clone(),
        settings: f.settings.clone(),
        records: None,
        bot_joins: None,
        collections: None,
        now: Some(Arc::new(|| NOW)),
    };
    let mut opts = ServerOptions::new(config(&[("BOT_OWNER_ID", OWNER_ID)]), deps);
    opts.logger = Some(Arc::new(RecordingLogger::new()));
    opts.public_dir = PublicDir::Path(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/dash-server/public-test"));
    (create_dashboard_router(opts).expect("router"), probe)
}

#[tokio::test]
async fn the_server_takes_stale_tolerant_lists_for_display_reads_only() {
    let (router, probe) = probe_router();
    let cookie = session_cookie(json!({}));
    let get = |path: String| {
        let router = router.clone();
        let cookie = cookie.clone();
        async move { call(&router, "GET", &path, &[("cookie", cookie)], None).await }
    };
    let r = get("/api/guilds".into()).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
    {
        let log = lock(&probe.log);
        assert_eq!(log.first().map(String::as_str), Some("prefetch"), "the bot list starts before the user's list is awaited");
        assert!(log.iter().any(|l| l == "presence:verify=false:swr=true"));
    }
    lock(&probe.log).clear();
    assert_eq!(get(format!("/api/guilds/{}/settings", g::ZETA)).await.status, 200);
    assert_eq!(lock(&probe.log).as_slice(), ["presence:verify=true:swr=false"], "GET settings checks the bot presence within the TTL");
    lock(&probe.log).clear();
    assert_eq!(get(format!("/api/guilds/{}/resources", g::ZETA)).await.status, 200);
    assert_eq!(lock(&probe.log).as_slice(), ["presence:verify=true:swr=false", "resources:fresh=false:swr=true"]);
    lock(&probe.log).clear();
    assert_eq!(get(format!("/api/guilds/{}/resources?fresh=1", g::ZETA)).await.status, 200);
    assert_eq!(lock(&probe.log).as_slice(), ["presence:verify=true:swr=false", "resources:fresh=true:swr=false"], "fresh forces a live load");
    lock(&probe.log).clear();
    let body = serde_json::to_vec(&json!({ "changes": { "welcomeChannelId": ch::TEXT } })).expect("json");
    let r = call(&router, "PATCH", &format!("/api/guilds/{}/settings", g::ZETA), &write_headers(&cookie), Some(body)).await;
    assert!(r.status == 200 || r.status == 400, "PATCH answered {}", r.status);
    let log = lock(&probe.log).clone();
    assert_eq!(log.first().map(String::as_str), Some("presence:verify=true:swr=false"), "PATCH presence is not stale-tolerant");
    assert!(log.iter().any(|l| l.starts_with("resources:")) && log.iter().all(|l| !l.ends_with("swr=true")), "the id checks never use a stale list: {log:?}");
}

#[tokio::test]
async fn the_callback_warms_the_users_guild_list_off_the_redirect() {
    let (router, probe) = probe_router();
    let h = [("cookie", "dash_oauth_state=s9".to_string())];
    let r = call(&router, "GET", "/auth/callback?code=good-code&state=s9", &h, None).await;
    assert_eq!(r.status, 302);
    assert_eq!(r.header("location").as_deref(), Some("/dashboard"));
    assert_eq!(lock(&probe.log).as_slice(), [format!("warm:{USER_ID}:{TOKEN}")]);
}

#[tokio::test]
async fn the_catalog_is_gzipped_once_and_nothing_else_is() {
    let router = main_router().0;
    let cookie = session_cookie(json!({}));
    let plain = call(&router, "GET", "/api/catalog", &[("cookie", cookie.clone())], None).await;
    assert_eq!(plain.status, 200);
    assert_eq!(plain.header("content-encoding"), None);
    assert_eq!(plain.header("vary").as_deref(), Some("Accept-Encoding"));
    assert_eq!(plain.header("cache-control").as_deref(), Some("no-store"));

    let zipped = call(&router, "GET", "/api/catalog", &[("cookie", cookie.clone()), ("accept-encoding", "gzip, br".into())], None).await;
    assert_eq!(zipped.status, 200);
    assert_eq!(zipped.header("content-encoding").as_deref(), Some("gzip"));
    assert_eq!(zipped.header("vary").as_deref(), Some("Accept-Encoding"));
    assert_eq!(zipped.header("cache-control").as_deref(), Some("no-store"));
    assert!(zipped.body.len() < plain.body.len() / 2);
    let mut unpacked = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&zipped.body[..]), &mut unpacked).expect("gunzip");
    assert_eq!(unpacked, plain.body, "the same bytes");

    let refused = call(&router, "GET", "/api/catalog", &[("cookie", cookie.clone()), ("accept-encoding", "gzip;q=0".into())], None).await;
    assert_eq!(refused.header("content-encoding"), None);
    let anon = call(&router, "GET", "/api/catalog", &[("accept-encoding", "gzip".into())], None).await;
    assert_eq!(anon.status, 401, "the session check stays");

    // No other API JSON is compressed (BREACH).
    for path in ["/api/guilds".to_string(), format!("/api/guilds/{}/settings", g::ZETA), format!("/api/guilds/{}/resources", g::ZETA), "/api/me".to_string()] {
        let r = call(&router, "GET", &path, &[("cookie", cookie.clone()), ("accept-encoding", "gzip".into())], None).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-encoding"), None, "{path}");
    }
}

// ── On a real socket ─────────────────────────────────────────────────────────────────────────────────────────────

struct Running {
    addr: std::net::SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

async fn run(router: Router) -> Running {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve_listener(listener, router, 10_000, async move {
        let _ = rx.await;
    }));
    Running { addr, stop: Some(tx), task }
}

impl Running {
    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.task).await.expect("stops promptly").expect("join");
    }
}

/// Sends raw bytes on a fresh connection and reads until the server closes it (or a short quiet period).
async fn raw(addr: std::net::SocketAddr, request: &[u8]) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    s.write_all(request).await.expect("write");
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).into_owned()
}

fn main_router() -> (Router, Fakes) {
    let f = create_fakes();
    let deps = DashboardDeps {
        discord: f.discord.clone(),
        settings: f.settings.clone(),
        records: None,
        bot_joins: None,
        collections: None,
        now: Some(Arc::new(|| NOW)),
    };
    let mut opts = ServerOptions::new(config(&[("BOT_OWNER_ID", OWNER_ID)]), deps);
    opts.logger = Some(Arc::new(RecordingLogger::new()));
    opts.public_dir = PublicDir::Path(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/dash-server/public-test"));
    (create_dashboard_router(opts).expect("router"), f)
}

fn head_of(answer: &str) -> String {
    answer.split("\r\n\r\n").next().unwrap_or_default().to_lowercase()
}

#[tokio::test]
async fn framing_targets_and_limits_on_the_wire() {
    let (router, f) = main_router();
    let srv = run(router).await;
    let addr = srv.addr;

    let head = raw(addr, b"HEAD /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head_of(&head).contains("content-length: 11"), "HEAD keeps the length of the GET body: {head}");
    assert!(head.ends_with("\r\n\r\n"), "and sends no body");

    let cookie = session_cookie(json!({}));
    let logout = format!(
        "POST /auth/logout HTTP/1.1\r\nHost: x\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nX-Dashboard: 1\r\nOrigin: http://localhost:8080\r\nContent-Length: 2\r\n\r\n{{}}"
    );
    let out = raw(addr, logout.as_bytes()).await;
    assert!(out.starts_with("HTTP/1.1 204"), "{out}");
    assert!(!head_of(&out).contains("content-length"), "a 204 carries no Content-Length: {out}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(lock(&f.calls)["revokeToken"], json!([TOKEN]), "the token is revoked after the answer");
    assert_eq!(lock(&f.calls)["forgetUser"], json!([USER_ID]));

    let not_modified = raw(addr, b"GET /styles.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\nIf-None-Match: *\r\n\r\n").await;
    assert!(not_modified.starts_with("HTTP/1.1 304"), "{not_modified}");
    assert!(!head_of(&not_modified).contains("content-length"));
    assert!(head_of(&not_modified).contains("etag: \""));

    // A declared body over the limit is refused before it is read, and the connection is closed.
    let big = format!(
        "PATCH /api/guilds/{}/settings HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nX-Dashboard: 1\r\nOrigin: http://localhost:8080\r\nContent-Length: 300000\r\n\r\n{{",
        g::ZETA
    );
    let started = Instant::now();
    let refused = raw(addr, big.as_bytes()).await;
    assert!(refused.starts_with("HTTP/1.1 413"), "{refused}");
    assert!(head_of(&refused).contains("connection: close"));
    assert!(refused.contains("The request body is too large."));
    assert!(started.elapsed() < Duration::from_secs(2), "closed, not left waiting for the rest of the body");

    let absolute = raw(addr, b"GET http://evil.example/healthz HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n").await;
    assert!(absolute.starts_with("HTTP/1.1 400"), "{absolute}");
    assert!(absolute.contains("Bad request target."));

    for path in ["//etc/passwd", "/%2e%2e/package.json", "/..%2fpackage.json", "/app.js%00"] {
        let out = raw(addr, format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert!(out.starts_with("HTTP/1.1 404"), "{path}: {out}");
    }

    // The peer address reaches the login limit: X-Forwarded-For is believed from the loopback proxy.
    let mut statuses = Vec::new();
    for _ in 0..11 {
        let out = raw(
            addr,
            b"GET /auth/callback?code=junk&state=abc HTTP/1.1\r\nHost: x\r\nConnection: close\r\nCookie: dash_oauth_state=abc\r\nX-Forwarded-For: 198.51.100.77\r\n\r\n",
        )
        .await;
        statuses.push(out[9..12].to_string());
    }
    assert_eq!(statuses.iter().filter(|s| *s == "429").count(), 1, "{statuses:?}");
    srv.stop().await;
}

#[tokio::test]
async fn shutdown_closes_every_open_connection() {
    let (router, _f) = main_router();
    let srv = run(router).await;
    let mut s = tokio::net::TcpStream::connect(srv.addr).await.expect("connect");
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").await.expect("write");
    let mut buf = [0u8; 2048];
    let n = s.read(&mut buf).await.expect("read");
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    srv.stop().await;
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await.expect("closed").unwrap_or(0);
    assert_eq!(n, 0, "the keep-alive connection is closed at shutdown");
}

#[tokio::test]
async fn trusted_proxies_and_other_peers() {
    let f = create_fakes();
    let deps = DashboardDeps { discord: f.discord.clone(), settings: f.settings.clone(), records: None, bot_joins: None, collections: None, now: Some(Arc::new(|| NOW)) };
    let mut opts = ServerOptions::new(config(&[("DASHBOARD_TRUSTED_PROXIES", "10.0.1.30")]), deps);
    opts.logger = Some(Arc::new(RecordingLogger::new()));
    let router = create_dashboard_router(opts).expect("router");
    let send = |peer: [u8; 4], xff: &'static str| {
        let router = router.clone();
        async move {
            let mut req = http::Request::builder()
                .uri("/auth/callback?code=junk&state=abc")
                .header("cookie", "dash_oauth_state=abc")
                .header("x-forwarded-for", xff)
                .body(axum::body::Body::empty())
                .expect("request");
            req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((peer, 5000))));
            use tower::ServiceExt;
            router.oneshot(req).await.expect("answer").status().as_u16()
        }
    };
    // Behind the trusted proxy every client is its own source; from another LAN host the header is ignored.
    for i in 0..12 {
        assert_eq!(send([10, 0, 1, 30], ["198.51.100.1", "198.51.100.2"][i % 2]).await, 302);
    }
    let mut limited = 0;
    for i in 0..11 {
        if send([10, 0, 1, 31], ["198.51.100.3", "198.51.100.4"][i % 2]).await == 429 {
            limited += 1;
        }
    }
    assert_eq!(limited, 1, "the untrusted peer is one source whatever it claims");
}

#[tokio::test]
async fn the_parked_server_on_the_wire() {
    let srv = run(create_parked_router(&["DISCORD_CLIENT_SECRET".into()], &[])).await;
    let health = raw(srv.addr, b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert!(health.starts_with("HTTP/1.1 200") && health.contains("{\"ok\":true,\"parked\":true}"));
    let page = raw(srv.addr, b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert!(page.starts_with("HTTP/1.1 503") && page.contains("dashboard not configured: missing DISCORD_CLIENT_SECRET\n"));
    assert!(head_of(&page).contains("x-content-type-options: nosniff"));
    // Node's req.url keeps an absolute-form target whole, so it is not /healthz (nor /api/...).
    let absolute = raw(srv.addr, b"GET http://x/healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert!(absolute.starts_with("HTTP/1.1 503") && absolute.contains("text/plain"), "{absolute}");
    srv.stop().await;
}

#[tokio::test]
async fn logs_never_contain_tokens_or_secrets() {
    let f = create_fakes();
    let logger = Arc::new(RecordingLogger::new());
    let deps = DashboardDeps { discord: f.discord.clone(), settings: f.settings.clone(), records: None, bot_joins: None, collections: None, now: Some(Arc::new(|| NOW)) };
    let mut opts = ServerOptions::new(config(&[]), deps);
    opts.logger = Some(logger.clone());
    let router = create_dashboard_router(opts).expect("router");
    let state = vec![("cookie", "dash_oauth_state=s1".to_string())];
    let r = call(&router, "GET", "/auth/callback?code=good-code&state=s1", &state, None).await;
    assert_eq!(r.header("location").as_deref(), Some("/dashboard"));
    call(&router, "GET", "/auth/callback?code=bad-code&state=s1", &state, None).await;
    let body = serde_json::to_vec(&json!({ "changes": { "welcomeMessage": "Hi {user}!" } })).expect("json");
    call(&router, "PATCH", &format!("/api/guilds/{}/settings", g::ZETA), &write_headers(&session_cookie(json!({}))), Some(body)).await;
    let lines = logger.lines();
    assert!(lines.iter().any(|l| l.contains("Dashboard login: alice")));
    assert!(lines.iter().any(|l| l.ends_with("changed guild 200000000000000001: welcomeMessage")));
    for line in &lines {
        for secret in [TOKEN, SECRET, "good-code", "Hi {user}"] {
            assert!(!line.contains(secret), "{line}");
        }
    }
}

#[tokio::test]
async fn repeated_cookie_headers_are_joined_like_node() {
    let (router, _f) = main_router();
    let headers = vec![("cookie", "a=1".to_string()), ("cookie", session_cookie(json!({})))];
    let r = call(&router, "GET", "/api/me", &headers, None).await;
    assert_eq!(r.status, 200, "the second Cookie header is its own cookie");
}

/// test/dashboard.test.js "front end": the pages link only to paths the server serves, and every marker is filled.
#[test]
fn pages_link_only_to_served_paths() {
    use bot_util::jsre::JsRegex;
    use dash_server::landing::{PageOptions, render_changelog, render_docs, render_landing, render_legal};
    use dash_server::static_files::{EMBEDDED, EMBEDDED_CHANGELOG, static_file};
    let refs = JsRegex::new(r##"(?:src|href)="(\/[^"#]*)(?:#[^"]*)?""##, "g");
    let marker = JsRegex::new(r"<!--[a-z]|@@INVITE_URL@@", "");
    let file = |name: &str| String::from_utf8_lossy(EMBEDDED.iter().find(|(n, _)| *n == name).expect("embedded").1).into_owned();
    let opts = PageOptions {
        invite_url: Some("https://discord.com/oauth2/authorize?client_id=1".into()),
        contact: None,
        support_url: Some("https://discord.gg/abc".into()),
        changelog: Some(EMBEDDED_CHANGELOG.to_string()),
    };
    let contact = dash_server::server::contact_html(None);
    let pages = [
        ("index.html", file("index.html")),
        ("landing.html", render_landing(&file("landing.html"), &opts).expect("landing")),
        ("docs.html", render_docs(&file("docs.html"), &opts).expect("docs")),
        ("changelog.html", render_changelog(&file("changelog.html"), &opts).expect("changelog")),
        ("privacy.html", render_legal(&file("privacy.html").replace("<!--contact-->", &contact), &opts).expect("privacy")),
        ("terms.html", render_legal(&file("terms.html").replace("<!--contact-->", &contact), &opts).expect("terms")),
    ];
    let mut checked = 0;
    for (name, html) in pages {
        assert!(!marker.test(&html), "{name}: every marker is filled");
        let units: Vec<u16> = html.encode_utf16().collect();
        for m in refs.find_all(&units) {
            let path = m.group(&units, 1).unwrap_or_default();
            assert!(static_file(&path).is_some(), "{name}: {path} is not served");
            checked += 1;
        }
    }
    assert!(checked > 20, "{checked} links checked");
}
