//! dash-auth against the JS: goldens produced by rust/fixtures/gen/dash-auth-golden.mjs, a live cross-language seal
//! check (rust/fixtures/gen/dash-auth-seal.mjs), and the auth, config and site cases of test/dashboard.test.js,
//! test/sec-dashboard.test.js and test/review-dashboard-server.test.js.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use dash_auth::auth::{self, CookieOptions, OAuthCredentials, WriteHeaders};
use dash_auth::changelog::{self, ChangelogEntry};
use dash_auth::command_list::{self, CommandEntry, CommandSummary};
use dash_auth::config::{self, load_dashboard_config_from_map, Contact, ContactKind};
use dash_auth::rate_limit::{self, LoginLimiter, RateDecision, RateLimiter, LOGIN_GLOBAL_RATE_LIMIT, LOGIN_RATE_LIMIT};
use dash_auth::{docs, js_ip, site_parts};
use serde_json::{json, Value};

fn root() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    // Node cannot load a script from a Windows verbatim path (`\\?\E:\...`).
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_string())
}

fn golden(name: &str) -> Value {
    let path = root().join("rust/fixtures/golden/dash-auth").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

fn opt_s(v: &Value) -> Option<&str> {
    v.as_str()
}

const SECRET: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
const USER_ID: &str = "111111111111111111";
const OWNER_ID: &str = "999999999999999999";
const TOKEN: &str = "user-access-token-abc123";

fn base_env(extra: &[(&str, &str)]) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = [
        ("CLIENT_ID", "123456789012345678"),
        ("DISCORD_TOKEN", "bot-token"),
        ("DISCORD_CLIENT_SECRET", "client-secret"),
        ("DASHBOARD_SESSION_SECRET", SECRET),
        ("DATABASE_URL", "postgresql://localhost/db"),
        ("REDIS_URL", "redis://localhost:6379"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    env
}

// ── config: goldens ──────────────────────────────────────────────────────────

#[test]
fn config_matches_the_js_for_every_environment() {
    let g = golden("config.json");
    let cases = g["cases"].as_array().unwrap();
    assert!(cases.len() > 60);
    for case in cases {
        let env: HashMap<String, String> =
            case["env"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), s(v).to_string())).collect();
        let got = serde_json::to_value(load_dashboard_config_from_map(&env)).unwrap();
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            serde_json::to_string(&case["result"]).unwrap(),
            "env {}",
            case["env"]
        );
    }
    let required: Vec<&str> = g["required"].as_array().unwrap().iter().map(s).collect();
    assert_eq!(required, config::REQUIRED_KEYS);
    for c in g["parseDashboardUrl"].as_array().unwrap() {
        let got = match config::parse_dashboard_url(s(&c["input"])) {
            Ok(url) => json!({ "ok": true, "url": url }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(got, c["output"], "{}", c["input"]);
    }
    for c in g["parseContact"].as_array().unwrap() {
        let got = serde_json::to_value(config::parse_contact(s(&c["input"]))).unwrap();
        assert_eq!(got, c["output"], "{}", c["input"]);
    }
}

#[test]
fn ip_helpers_match_node() {
    let g = golden("ip.json");
    for c in g["ipCases"].as_array().unwrap() {
        let input = s(&c["input"]);
        assert_eq!(u64::from(js_ip::is_ip(input)), c["isIP"].as_u64().unwrap(), "isIP {input:?}");
        assert_eq!(config::normalize_ip(input), s(&c["normalizeIp"]), "normalizeIp {input:?}");
    }
    for c in g["loginSourceKey"].as_array().unwrap() {
        assert_eq!(rate_limit::login_source_key(s(&c["input"])), s(&c["output"]), "loginSourceKey {}", c["input"]);
    }
    let trusted: HashSet<String> = g["clientAddress"]["trusted"].as_array().unwrap().iter().map(|v| s(v).to_string()).collect();
    let empty = HashSet::new();
    for c in g["clientAddress"]["cases"].as_array().unwrap() {
        let set = if c["trusted"].as_bool().unwrap() { &trusted } else { &empty };
        let got = rate_limit::client_address(opt_s(&c["peer"]), opt_s(&c["forwarded"]), set);
        assert_eq!(got, s(&c["output"]), "{c}");
    }
    let limits = &g["limits"];
    let pair = |l: rate_limit::RateLimit| json!({ "max": l.max, "windowMs": l.window_ms });
    assert_eq!(pair(rate_limit::WRITE_RATE_LIMIT), limits["WRITE_RATE_LIMIT"]);
    assert_eq!(pair(rate_limit::SEARCH_RATE_LIMIT), limits["SEARCH_RATE_LIMIT"]);
    assert_eq!(pair(LOGIN_RATE_LIMIT), limits["LOGIN_RATE_LIMIT"]);
    assert_eq!(pair(LOGIN_GLOBAL_RATE_LIMIT), limits["LOGIN_GLOBAL_RATE_LIMIT"]);
    assert_eq!(pair(rate_limit::LOGIN_FIRST_ATTEMPT_RATE_LIMIT), limits["LOGIN_FIRST_ATTEMPT_RATE_LIMIT"]);
}

#[test]
fn rate_limiter_sequence_matches_the_js() {
    let g = golden("rate-limiter.json");
    let limiter = RateLimiter::new(rate_limit::RateLimit {
        max: g["max"].as_u64().unwrap() as usize,
        window_ms: g["windowMs"].as_u64().unwrap(),
    });
    for step in g["steps"].as_array().unwrap() {
        let got = match limiter.hit(s(&step["key"]), step["at"].as_f64().unwrap()) {
            RateDecision::Ok => json!({ "ok": true }),
            RateDecision::Limited { retry_after } => json!({ "ok": false, "retryAfter": retry_after }),
        };
        assert_eq!(got, step["result"], "{step}");
    }
}

// ── config: test/dashboard.test.js ───────────────────────────────────────────

#[test]
fn config_reports_every_missing_required_key() {
    let r = load_dashboard_config_from_map(&HashMap::new());
    assert!(!r.ok);
    assert_eq!(r.missing, config::REQUIRED_KEYS);
    assert_eq!(r.config.port, 8080);
    assert_eq!(r.config.host, "0.0.0.0");
}

#[test]
fn config_defaults_and_derived_values() {
    let r = load_dashboard_config_from_map(&base_env(&[]));
    assert!(r.ok, "{:?}", r.invalid);
    assert_eq!(r.config.url, "http://localhost:8080");
    assert_eq!(r.config.origin, "http://localhost:8080");
    assert_eq!(r.config.redirect_uri, "http://localhost:8080/auth/callback");
    assert!(!r.config.secure_cookies);
    assert_eq!(r.config.bot_owner_id, None);
    assert!(r.config.url_is_default);
    assert!(load_dashboard_config_from_map(&HashMap::new()).config.url_is_default);
    for url in ["https://dash.example.com", "http://localhost:8080"] {
        let r = load_dashboard_config_from_map(&base_env(&[("DASHBOARD_URL", url)]));
        assert!(!r.config.url_is_default, "{url}");
        assert_eq!(r.config.url, url);
    }
    assert!(load_dashboard_config_from_map(&base_env(&[("DASHBOARD_URL", "   ")])).config.url_is_default, "blank counts as unset");
    let r = load_dashboard_config_from_map(&base_env(&[
        ("DASHBOARD_URL", "https://dash.example.com/"),
        ("DASHBOARD_PORT", "9000"),
        ("BOT_OWNER_ID", OWNER_ID),
    ]));
    assert!(r.ok);
    assert_eq!(r.config.url, "https://dash.example.com");
    assert_eq!(r.config.redirect_uri, "https://dash.example.com/auth/callback");
    assert!(r.config.secure_cookies);
    assert_eq!(r.config.port, 9000);
    assert_eq!(r.config.bot_owner_id.as_deref(), Some(OWNER_ID));
}

#[test]
fn config_invalid_values_are_reported_by_key() {
    let cases: [(&str, &str); 10] = [
        ("DASHBOARD_SESSION_SECRET", "short"),
        ("DASHBOARD_URL", "ftp://example.com"),
        ("DASHBOARD_URL", "not a url"),
        ("DASHBOARD_URL", "https://example.com/dashboard"),
        ("DASHBOARD_URL", "https://user:pw@example.com"),
        ("DASHBOARD_PORT", "70000"),
        ("DASHBOARD_PORT", "abc"),
        ("CLIENT_ID", "my-app"),
        ("DASHBOARD_TRUSTED_PROXIES", "10.0.1.30, cloudflared"),
        ("DASHBOARD_TRUSTED_PROXIES", "10.0.1.0/24"),
    ];
    for (key, value) in cases {
        let r = load_dashboard_config_from_map(&base_env(&[(key, value)]));
        assert!(!r.ok, "{key}={value} was accepted");
        assert_eq!(r.invalid.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), [key]);
    }
    let owner = load_dashboard_config_from_map(&base_env(&[("BOT_OWNER_ID", "me")]));
    assert!(owner.ok);
    assert_eq!(owner.config.bot_owner_id, None);
    assert_eq!(owner.warnings.len(), 1);
}

#[test]
fn config_trusted_proxies_contact_and_support() {
    assert!(load_dashboard_config_from_map(&base_env(&[])).config.trusted_proxies.is_empty());
    let r = load_dashboard_config_from_map(&base_env(&[(
        "DASHBOARD_TRUSTED_PROXIES",
        " 10.0.1.30,10.0.1.31 , ::FFFF:10.0.1.30,,fd7a::1 ",
    )]));
    assert!(r.ok, "{:?}", r.invalid);
    assert_eq!(r.config.trusted_proxies, ["10.0.1.30", "10.0.1.31", "fd7a::1"]);

    assert_eq!(load_dashboard_config_from_map(&base_env(&[])).config.contact, None);
    assert_eq!(
        load_dashboard_config_from_map(&base_env(&[("DASHBOARD_CONTACT", "owner@example.com")])).config.contact,
        Some(Contact { kind: ContactKind::Email, value: "owner@example.com".into() })
    );
    assert_eq!(
        load_dashboard_config_from_map(&base_env(&[("DASHBOARD_CONTACT", "https://example.com/contact")])).config.contact,
        Some(Contact { kind: ContactKind::Url, value: "https://example.com/contact".into() })
    );
    for bad in ["http://example.com", "javascript:alert(1)", "owner at example", "<b>@x.y", "https://u:p@example.com"] {
        let r = load_dashboard_config_from_map(&base_env(&[("DASHBOARD_CONTACT", bad)]));
        assert!(r.ok, "{bad}");
        assert_eq!(r.config.contact, None, "{bad}");
        assert_eq!(r.warnings.len(), 1, "{bad}");
    }
    assert_eq!(load_dashboard_config_from_map(&base_env(&[])).config.support_url, None);
    assert_eq!(
        load_dashboard_config_from_map(&base_env(&[("DASHBOARD_SUPPORT_URL", "https://discord.gg/abc123")])).config.support_url,
        Some("https://discord.gg/abc123".into())
    );
    for bad in ["http://discord.gg/abc123", "owner@example.com", "javascript:alert(1)", "https://u:p@discord.gg/x"] {
        let r = load_dashboard_config_from_map(&base_env(&[("DASHBOARD_SUPPORT_URL", bad)]));
        assert!(r.ok, "{bad}");
        assert_eq!(r.config.support_url, None, "{bad}");
        assert_eq!(r.warnings.len(), 1, "{bad}");
    }
}

#[test]
fn every_key_the_dashboard_reads_is_documented_in_env_example() {
    let env = std::fs::read_to_string(root().join(".env.example")).unwrap();
    for key in [
        "DISCORD_CLIENT_SECRET",
        "DASHBOARD_URL",
        "DASHBOARD_SESSION_SECRET",
        "DASHBOARD_PORT",
        "DASHBOARD_HOST",
        "DASHBOARD_TRUSTED_PROXIES",
        "DASHBOARD_CONTACT",
        "DASHBOARD_SUPPORT_URL",
    ] {
        let documented = env.lines().any(|l| {
            l.strip_prefix('#').is_some_and(|rest| rest.trim_start().starts_with(&format!("{key}=")))
        });
        assert!(documented, "{key} must be a commented-out line");
    }
    for key in ["CLIENT_ID", "BOT_OWNER_ID"] {
        assert!(env.lines().any(|l| l.starts_with(&format!("{key}="))), "{key}");
    }
}

// ── auth: goldens ────────────────────────────────────────────────────────────

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn keys_and_js_sealed_cookies_open_in_rust() {
    let g = golden("auth.json");
    let c = &g["constants"];
    assert_eq!(s(&c["SESSION_COOKIE"]), auth::SESSION_COOKIE);
    assert_eq!(s(&c["STATE_COOKIE"]), auth::STATE_COOKIE);
    assert_eq!(c["SESSION_MAX_AGE_SECONDS"].as_u64(), Some(auth::SESSION_MAX_AGE_SECONDS));
    assert_eq!(c["STATE_MAX_AGE_SECONDS"].as_u64(), Some(auth::STATE_MAX_AGE_SECONDS));
    assert_eq!(c["OAUTH_SCOPES"], json!(auth::OAUTH_SCOPES));
    assert_eq!(c["OAUTH_TIMEOUT_MS"].as_u64(), Some(auth::OAUTH_TIMEOUT_MS));

    for k in g["keys"].as_array().unwrap() {
        let key = auth::derive_session_key(s(&k["secret"])).unwrap();
        assert_eq!(hex(key.as_bytes()), s(&k["keyHex"]), "{}", k["secret"]);
    }
    assert_eq!(auth::derive_session_key(""), Err("deriveSessionKey needs a secret"));

    let now = 1_800_000_000_000.0;
    for case in g["sealed"].as_array().unwrap() {
        let key = auth::derive_session_key(s(&case["secret"])).unwrap();
        let opened = auth::unseal_session(Some(s(&case["token"])), &key, now).expect("a JS cookie opens");
        assert_eq!(Value::Object(opened), case["payload"]);
        let other = auth::derive_session_key("another-secret-another-secret-another").unwrap();
        assert!(auth::unseal_session(Some(s(&case["token"])), &other, now).is_none());
    }
    let key0 = auth::derive_session_key(SECRET).unwrap();
    for case in g["unsealCases"].as_array().unwrap() {
        let got = auth::unseal_session(Some(s(&case["token"])), &key0, case["now"].as_f64().unwrap()).map(Value::Object);
        assert_eq!(got.unwrap_or(Value::Null), case["output"], "{}", case["token"]);
    }
}

#[test]
fn cookies_match_the_js() {
    let g = golden("auth.json");
    for c in g["cookieCases"].as_array().unwrap() {
        let got: Vec<Value> =
            auth::parse_cookies(Some(s(&c["header"]))).into_iter().map(|(k, v)| json!([k, v])).collect();
        assert_eq!(Value::Array(got), c["entries"], "{}", c["header"]);
    }
    for c in g["serializeCases"].as_array().unwrap() {
        let o = &c["opts"];
        let opts = CookieOptions {
            max_age: o["maxAge"].as_f64(),
            path: o["path"].as_str().unwrap_or("/").to_string(),
            secure: o["secure"].as_bool().unwrap_or(false),
        };
        assert_eq!(auth::serialize_cookie(s(&c["name"]), s(&c["value"]), &opts), s(&c["output"]), "{c}");
    }
    for c in g["clearCases"].as_array().unwrap() {
        let o = &c["opts"];
        let got = auth::clear_cookie(s(&c["name"]), o["path"].as_str().unwrap_or("/"), o["secure"].as_bool().unwrap_or(false));
        assert_eq!(got, s(&c["output"]));
    }
}

fn creds_of(v: &Value) -> OAuthCredentials {
    OAuthCredentials {
        client_id: s(&v["clientId"]).into(),
        client_secret: s(&v["clientSecret"]).into(),
        redirect_uri: s(&v["redirectUri"]).into(),
    }
}

#[tokio::test]
async fn oauth_requests_and_answers_match_the_js() {
    let g = golden("auth.json");
    for c in g["authorizeCases"].as_array().unwrap() {
        let got = auth::build_authorize_url(s(&c["clientId"]), s(&c["redirectUri"]), s(&c["state"]));
        assert_eq!(got, s(&c["output"]));
    }
    for c in g["requests"].as_array().unwrap() {
        let creds = creds_of(&c["config"]);
        let req = if s(&c["kind"]) == "exchange" {
            auth::token_request(&creds, auth::TOKEN_URL, s(&c["code"]))
        } else {
            auth::revoke_request(&creds, auth::TOKEN_REVOCATION_URL, s(&c["token"]))
        };
        let js = &c["request"];
        assert_eq!(req.url, s(&js["url"]));
        assert_eq!(req.body, s(&js["body"]));
        assert_eq!(req.authorization, s(&js["headers"]["Authorization"]));
        assert_eq!(s(&js["headers"]["Content-Type"]), "application/x-www-form-urlencoded");
        assert_eq!(s(&js["headers"]["Accept"]), "application/json");
        assert_eq!(s(&js["method"]), "POST");
    }
    let creds = creds_of(&g["requests"][0]["config"]);
    for c in g["responseCases"].as_array().unwrap() {
        let status = c["status"].as_u64().unwrap() as u16;
        let body = s(&c["body"]).as_bytes().to_vec();
        let got = auth::exchange_code_with(&creds, auth::TOKEN_URL, "code", |req| async move {
            assert!(req.body.contains("code=code"));
            Ok((status, body))
        })
        .await;
        match got {
            Ok(grant) => {
                assert_eq!(grant.access_token, s(&c["ok"]["accessToken"]), "{c}");
                assert_eq!(grant.expires_in, c["ok"]["expiresIn"].as_f64().unwrap(), "{c}");
            }
            Err(err) => {
                assert_eq!(err.message, s(&c["err"]["message"]), "{c}");
                assert_eq!(err.status.map(u64::from), c["err"]["status"].as_u64(), "{c}");
            }
        }
    }
    for c in g["revokeCases"].as_array().unwrap() {
        let status = c["status"].as_u64().map(|n| n as u16);
        let got = auth::revoke_token_with(&creds, auth::TOKEN_REVOCATION_URL, "t", |_| async move {
            status.map(|st| (st, Vec::new())).ok_or_else(|| "down".to_string())
        })
        .await;
        assert_eq!(got, c["output"].as_bool().unwrap(), "{c}");
    }
    // A transport failure is an error without a status, as a fetch that throws.
    let err = auth::exchange_code_with(&creds, auth::TOKEN_URL, "c", |_| async { Err("fetch failed".to_string()) })
        .await
        .unwrap_err();
    assert_eq!((err.message.as_str(), err.status), ("fetch failed", None));
}

#[tokio::test]
async fn oauth_client_fails_offline_without_a_status() {
    // A port nothing listens on: the request itself fails, and nothing leaks a token into the message.
    let client = auth::OAuthClient::with_endpoints("http://127.0.0.1:9/token", "http://127.0.0.1:9/revoke");
    let creds = OAuthCredentials { client_id: "1".into(), client_secret: "secret".into(), redirect_uri: "http://x/cb".into() };
    let err = client.exchange_code(&creds, "code").await.unwrap_err();
    assert_eq!(err.status, None);
    assert!(!err.message.contains("secret"));
    assert!(!client.revoke_token(&creds, TOKEN).await);
}

#[test]
fn access_rules_match_the_js() {
    let g = golden("auth.json");
    for c in g["guildCases"].as_array().unwrap() {
        let guild = if c["guild"] == json!("__undefined__") { None } else { Some(&c["guild"]) };
        assert_eq!(auth::can_manage_guild(guild), c["output"].as_bool().unwrap(), "{}", c["guild"]);
    }
    for c in g["writeCases"].as_array().unwrap() {
        let h = &c["headers"];
        let headers = WriteHeaders {
            content_type: opt_s(&h["content-type"]).map(str::to_string),
            x_dashboard: opt_s(&h["x-dashboard"]).map(str::to_string),
            origin: opt_s(&h["origin"]).map(str::to_string),
            sec_fetch_site: opt_s(&h["sec-fetch-site"]).map(str::to_string),
        };
        let got = match auth::check_write_request(s(&c["method"]), &headers, "https://dash.example.com") {
            Ok(()) => json!({ "ok": true }),
            Err(r) => json!({ "ok": false, "status": r.status, "error": r.error, "message": r.message }),
        };
        assert_eq!(got, c["output"], "{c}");
    }
}

// ── auth: test/dashboard.test.js ─────────────────────────────────────────────

fn payload(exp: f64) -> Value {
    json!({ "uid": USER_ID, "username": "alice", "globalName": "Alice", "avatar": null, "accessToken": TOKEN, "exp": exp as i64 })
}

#[test]
fn seal_unseal_round_trips_and_hides_the_payload() {
    let key = auth::derive_session_key(SECRET).unwrap();
    let now = bot_util::js::now_ms();
    let p = payload(now + 60_000.0);
    let sealed = auth::seal_session(&p, &key);
    assert!(sealed.starts_with("v1.") && sealed[3..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
    assert!(!sealed.contains(TOKEN));
    assert_eq!(auth::unseal_session(Some(&sealed), &key, now).map(Value::Object), Some(p.clone()));
    assert_ne!(auth::seal_session(&p, &key), sealed, "a fresh IV per seal");
}

#[test]
fn tampered_expired_foreign_or_malformed_values_unseal_to_none() {
    let key = auth::derive_session_key(SECRET).unwrap();
    let now = bot_util::js::now_ms();
    let exp = (now + 60_000.0).floor();
    let p = payload(exp);
    let sealed = auth::seal_session(&p, &key);
    let body = &sealed[3..];
    let mid = body.len() / 2;
    let flip = if &body[mid..=mid] == "A" { "B" } else { "A" };
    let flipped = format!("v1.{}{}{}", &body[..mid], flip, &body[mid + 1..]);
    assert!(auth::unseal_session(Some(&flipped), &key, now).is_none());
    assert!(auth::unseal_session(Some(&sealed[..sealed.len() - 4]), &key, now).is_none());
    assert!(auth::unseal_session(Some(&format!("v2.{body}")), &key, now).is_none());
    assert!(auth::unseal_session(Some(&sealed), &auth::derive_session_key(&"y".repeat(48)).unwrap(), now).is_none());
    assert!(auth::unseal_session(Some(&sealed), &key, exp).is_none(), "expired at exp");
    let mut never = p.clone();
    never["exp"] = json!("never");
    assert!(auth::unseal_session(Some(&auth::seal_session(&never, &key)), &key, now).is_none());
    assert!(auth::unseal_session(Some(&auth::seal_session(&json!([1, 2]), &key)), &key, now).is_none());
    for junk in [None, Some(""), Some("v1."), Some("v1.!!!"), Some("abc")] {
        assert!(auth::unseal_session(junk, &key, now).is_none());
    }
    assert!(auth::unseal_session(Some(&format!("v1.{}", "A".repeat(5000))), &key, now).is_none());
}

#[test]
fn cookies_parse_and_serialise() {
    let jar = auth::parse_cookies(Some("a=1; dash_session=v1.abc; a=2; bad=%E0%A4%A; q=\"quoted\"; =x; flag"));
    assert_eq!(jar.get("a").map(String::as_str), Some("1"));
    assert_eq!(jar.get("dash_session").map(String::as_str), Some("v1.abc"));
    assert!(!jar.contains_key("bad"));
    assert_eq!(jar.get("q").map(String::as_str), Some("quoted"));
    assert!(auth::parse_cookies(None).is_empty());
    let c = auth::serialize_cookie("dash_session", "v1.x", &CookieOptions { max_age: Some(60.9), secure: true, ..Default::default() });
    assert_eq!(c, "dash_session=v1.x; Path=/; HttpOnly; SameSite=Lax; Max-Age=60; Secure");
    assert!(auth::clear_cookie("dash_session", "/", false)
        .starts_with("dash_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Expires="));
}

#[test]
fn the_oauth_state_is_random_and_compared_exactly() {
    let st = auth::create_oauth_state();
    assert_eq!(st.len(), 43);
    assert!(st.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
    assert_ne!(auth::create_oauth_state(), st);
    assert!(auth::states_match(Some(&st), Some(&st)));
    assert!(!auth::states_match(Some(&st), Some(&format!("{st}x"))));
    assert!(!auth::states_match(Some(&st), Some(&st.to_uppercase())));
    assert!(!auth::states_match(None, Some(&st)));
    assert!(!auth::states_match(Some(""), Some("")));
    assert!(!auth::states_match(Some(&st), None));
}

#[test]
fn the_authorize_url_asks_for_identify_and_guilds() {
    let raw = auth::build_authorize_url("123456789012345678", "https://d.example/auth/callback", "st");
    let url = url::Url::parse(&raw).unwrap();
    assert_eq!(url.origin().ascii_serialization(), "https://discord.com");
    assert_eq!(url.path(), "/oauth2/authorize");
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["scope"], "identify guilds");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["redirect_uri"], "https://d.example/auth/callback");
    assert_eq!(q["state"], "st");
    assert_eq!(q["prompt"], "none");
}

#[test]
fn can_manage_guild_mirrors_the_admin_level_with_no_owner_bypass() {
    let cases = [
        (json!({ "owner": true, "permissions": "0" }), true),
        (json!({ "owner": false, "permissions": "8" }), true),
        (json!({ "owner": false, "permissions": "32" }), true),
        (json!({ "owner": false, "permissions": "1099511627775" }), true),
        (json!({ "owner": false, "permissions": "0" }), false),
        (json!({ "owner": false, "permissions": "16" }), false),
        (json!({ "owner": false, "permissions": "1099511627735" }), false),
        (json!({ "owner": false, "permissions": "abc" }), false),
        (json!({ "owner": false, "permissions": "-8" }), false),
        (json!({ "owner": false }), false),
        (json!({ "owner": "true", "permissions": "0" }), false),
        (Value::Null, false),
    ];
    for (guild, expected) in cases {
        assert_eq!(auth::can_manage_guild(Some(&guild)), expected, "{guild}");
    }
    assert!(!auth::can_manage_guild(None), "the guild must be in the user's list");
}

fn write_headers(origin: Option<&str>, site: Option<&str>) -> WriteHeaders {
    WriteHeaders {
        content_type: Some("application/json".into()),
        x_dashboard: Some("1".into()),
        origin: origin.map(str::to_string),
        sec_fetch_site: site.map(str::to_string),
    }
}

#[test]
fn check_write_request_names_the_configured_address_when_a_well_formed_origin_differs() {
    let origin = "https://dash.example.com";
    let check = |o: &str| auth::check_write_request("PATCH", &write_headers(Some(o), None), origin);
    let expected =
        "This dashboard is configured for https://dash.example.com. Open it there, or set DASHBOARD_URL to the address you use.";
    for o in ["http://192.168.1.5:8080", "http://localhost:8080", "https://evil.example", "https://dash.example.com.evil.example"] {
        let r = check(o).unwrap_err();
        assert_eq!((r.status, r.error), (403, "forbidden"), "{o}");
        assert_eq!(r.message, expected, "{o}");
        let host = url::Url::parse(o).unwrap().host_str().unwrap().to_string();
        assert!(!r.message.contains(&host), "the request origin is never echoed");
    }
    for o in ["null", "", "not an origin", "ftp://dash.example.com", "https://evil.example/path", "javascript:alert(1)"] {
        let r = check(o).unwrap_err();
        assert_eq!(r.status, 403, "{o:?}");
        assert_eq!(r.message, "Cross-origin requests are not allowed.", "{o:?}");
    }
    assert_eq!(check(origin), Ok(()));
    let site = auth::check_write_request("PATCH", &write_headers(Some(origin), Some("cross-site")), origin).unwrap_err();
    assert_eq!(site.message, "Cross-site requests are not allowed.");
}

// ── rate limits: test/dashboard.test.js, review-dashboard-server and sec-dashboard ──

#[test]
fn create_rate_limiter_allows_max_hits_per_window() {
    let hit = RateLimiter::new(rate_limit::RateLimit { max: 2, window_ms: 1_000 });
    assert!(hit.hit("a", 0.0).is_ok());
    assert!(hit.hit("a", 0.0).is_ok());
    assert_eq!(hit.hit("a", 0.0), RateDecision::Limited { retry_after: 1 });
    assert!(hit.hit("b", 0.0).is_ok());
    assert!(hit.hit("a", 1_001.0).is_ok());
}

#[test]
fn client_address_believes_x_forwarded_for_only_from_loopback_or_a_trusted_proxy() {
    let none = HashSet::new();
    let ca = |peer: Option<&str>, fwd: Option<&str>| rate_limit::client_address(peer, fwd, &none);
    assert_eq!(ca(Some("203.0.113.9"), None), "203.0.113.9");
    assert_eq!(ca(Some("203.0.113.9"), Some("198.51.100.1")), "203.0.113.9", "a client cannot choose its own key");
    assert_eq!(ca(Some("10.0.0.5"), Some("198.51.100.1")), "10.0.0.5", "only the loopback peer is trusted as a proxy");
    for peer in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
        assert_eq!(ca(Some(peer), Some("198.51.100.1")), "198.51.100.1", "{peer}");
        assert_eq!(ca(Some(peer), Some("spoofed, 198.51.100.2")), "198.51.100.2");
    }
    assert_eq!(ca(Some("127.0.0.1"), None), "127.0.0.1", "no header: the proxy itself");
    assert_eq!(ca(Some("127.0.0.1"), Some(&"x".repeat(200))), "127.0.0.1", "an absurd header is not used as a key");
    assert_eq!(ca(None, None), "unknown");
    let trusted: HashSet<String> = ["10.0.1.30".to_string()].into();
    assert_eq!(rate_limit::client_address(Some("::ffff:10.0.1.30"), Some("198.51.100.9"), &trusted), "198.51.100.9");
}

#[test]
fn login_source_key_an_ipv6_address_is_its_64() {
    let key = rate_limit::login_source_key;
    assert_eq!(key("198.51.100.7:51234"), "198.51.100.7", "a port is dropped");
    assert_eq!(key("[2001:db8:1:2::a]:443"), key("2001:db8:1:2::b"));
    assert_eq!(key("[2001:db8:1:2::a]"), key("2001:db8:1:2::b"));
    assert_eq!(key("[fe80::1%eth0]:80"), key("fe80::9"));
    assert_eq!(key("[::ffff:203.0.113.9]:5000"), "203.0.113.9");
    assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
    assert_eq!(key("::FFFF:203.0.113.9%eth0"), "203.0.113.9");
    assert_eq!(key("2001:db8:1:2::a"), key("2001:DB8:1:2:0:0:0:f"));
    assert_eq!(key("2001:db8:1:2::a"), key("2001:db8:1:2:ffff:ffff:ffff:ffff"));
    assert_ne!(key("2001:db8:1:2::a"), key("2001:db8:1:3::a"));
    assert_eq!(key("2001:db8:1:2::a"), "2001:0db8:0001:0002::/64");
    assert_eq!(key("::1"), "0000:0000:0000:0000::/64");
    assert_eq!(key("fe80::1%eth0"), key("fe80::2"));
    assert_eq!(key("::ffff:cb00:7109"), "203.0.113.9");
    assert_eq!(key("203.0.113.9"), "203.0.113.9");
    assert_eq!(key("unknown"), "unknown");
    assert_eq!(key("not an ip"), "not an ip");
    assert_eq!(key(""), "");
}

/// The login callback of server.js reduced to its counting: a callback behind the loopback proxy, from `address`
/// (the X-Forwarded-For value), that reaches the token exchange unless refused; `legit` succeeds.
struct LoginHarness {
    limiter: LoginLimiter,
    clock: f64,
    exchanges: usize,
}

impl LoginHarness {
    fn new() -> Self {
        Self { limiter: LoginLimiter::new(), clock: 1_800_000_000_000.0, exchanges: 0 }
    }

    fn call(&mut self, forwarded: &str, legit: bool) -> u16 {
        let address = rate_limit::client_address(Some("127.0.0.1"), Some(forwarded), &HashSet::new());
        let source = rate_limit::login_source_key(&address);
        let check = self.limiter.check(&source, self.clock);
        if let RateDecision::Limited { retry_after } = check.decision {
            assert!((1..=60).contains(&retry_after));
            return 429;
        }
        self.exchanges += 1;
        if legit {
            self.limiter.resolved(&source);
        }
        302
    }
}

#[test]
fn one_address_gets_10_exchanges_a_minute_and_forged_entries_do_not_help() {
    let mut h = LoginHarness::new();
    for _ in 0..LOGIN_RATE_LIMIT.max {
        assert_eq!(h.call("198.51.100.1", false), 302);
    }
    assert_eq!(h.call("198.51.100.1", false), 429);
    assert_eq!(h.exchanges, LOGIN_RATE_LIMIT.max);
    assert_eq!(h.call("198.51.100.2", false), 302);
    h.clock += LOGIN_RATE_LIMIT.window_ms as f64 + 1.0;
    assert_eq!(h.call("198.51.100.1", false), 302);

    h.clock += LOGIN_RATE_LIMIT.window_ms as f64 + 1.0;
    let before = h.exchanges;
    let limited = (0..LOGIN_RATE_LIMIT.max + 3).filter(|i| h.call(&format!("10.9.9.{i}, 198.51.100.3"), false) == 429).count();
    assert_eq!(limited, 3);
    assert_eq!(h.exchanges - before, LOGIN_RATE_LIMIT.max);
}

#[test]
fn all_addresses_together_get_60_exchanges_once_their_first_has_failed() {
    let mut h = LoginHarness::new();
    let max = LOGIN_GLOBAL_RATE_LIMIT.max;
    for i in 0..=max {
        assert_eq!(h.call(&format!("203.0.113.{i}"), false), 302, "first callback of source {i}");
    }
    for i in 0..max {
        assert_eq!(h.call(&format!("203.0.113.{i}"), false), 302, "second callback of source {i}");
    }
    assert_eq!(h.exchanges, 2 * max + 1);
    assert_eq!(h.call(&format!("203.0.113.{max}"), false), 429);
    assert_eq!(h.exchanges, 2 * max + 1);
    h.clock += LOGIN_GLOBAL_RATE_LIMIT.window_ms as f64 + 1.0;
    assert_eq!(h.call(&format!("203.0.113.{max}"), false), 302);
}

#[test]
fn many_junk_sources_cannot_lock_out_a_clean_one() {
    let mut h = LoginHarness::new();
    for src in 10..16 {
        for _ in 0..LOGIN_RATE_LIMIT.max {
            h.call(&format!("198.51.100.{src}"), false);
        }
    }
    assert_eq!(h.exchanges, 60);
    assert_eq!(h.call("203.0.113.77", true), 302);
    for src in 20..30 {
        for _ in 0..LOGIN_RATE_LIMIT.max {
            h.call(&format!("198.51.100.{src}"), false);
        }
    }
    assert!(h.exchanges - 61 <= LOGIN_GLOBAL_RATE_LIMIT.max + 10);
    assert_eq!(h.call("203.0.113.78", true), 302);
}

#[test]
fn an_ipv6_64_is_one_source() {
    let mut h = LoginHarness::new();
    let refused = (0..30).filter(|i| h.call(&format!("2001:db8:1:2::{:x}", i + 10), false) == 429).count();
    assert_eq!(refused, 20);
    assert_eq!(h.exchanges, LOGIN_RATE_LIMIT.max);
    assert_eq!(h.call("2001:DB8:1:2:abcd::1", false), 429);
    assert_eq!(h.call("2001:db8:9999::1", true), 302);
    assert_eq!(h.call("2001:db8:1:3::1", false), 302);
    h.clock += LOGIN_RATE_LIMIT.window_ms as f64 + 1.0;
    assert_eq!(h.call("2001:db8:1:2::99", false), 302);
}

#[test]
fn a_successful_login_clears_the_mark() {
    let mut h = LoginHarness::new();
    for _ in 0..2 {
        for i in 0..60 {
            h.call(&format!("203.0.113.{i}"), false);
        }
    }
    assert_eq!(h.call("198.51.100.9", false), 302, "a fresh source is free");
    assert_eq!(h.call("198.51.100.9", false), 429, "but its next callback is not");
    h.clock += LOGIN_GLOBAL_RATE_LIMIT.window_ms as f64 + 1.0;
    assert_eq!(h.call("198.51.100.10", true), 302);
    assert_eq!(h.call("198.51.100.10", false), 302, "junk after a success is a first callback again");
    assert_eq!(h.call("198.51.100.10", false), 302);
}

#[test]
fn concurrent_callbacks_of_a_fresh_source_cannot_all_skip_the_allowance() {
    // The mark is set by check() itself, before any exchange: 7 sources x 10 unresolved callbacks.
    let mut h = LoginHarness::new();
    let mut refused = 0;
    for src in 0..7 {
        for _ in 0..LOGIN_RATE_LIMIT.max {
            if h.call(&format!("198.51.100.{}", 40 + src), false) == 429 {
                refused += 1;
            }
        }
    }
    assert_eq!(h.exchanges, 7 + LOGIN_GLOBAL_RATE_LIMIT.max);
    assert_eq!(refused, 7 * LOGIN_RATE_LIMIT.max - (7 + LOGIN_GLOBAL_RATE_LIMIT.max));
}

#[test]
fn callbacks_racing_on_threads_get_one_first_callback_per_source() {
    // The JS checks and marks a source in one synchronous step; on a multi-threaded runtime two callbacks of a fresh
    // source must not both see it unmarked. The overall allowance is used up first, so a second "first" would pass.
    let limiter = std::sync::Arc::new(LoginLimiter::new());
    let now = 1_800_000_000_000.0;
    for round in 0..2 {
        for i in 0..=LOGIN_GLOBAL_RATE_LIMIT.max {
            let _ = round;
            limiter.check(&format!("203.0.113.{i}"), now);
        }
    }
    for round in 0..150 {
        let source = format!("198.51.{}.{}", 100 + round / 200, round % 200);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let passed: usize = (0..8)
            .map(|_| {
                let (l, b, s) = (limiter.clone(), barrier.clone(), source.clone());
                std::thread::spawn(move || {
                    b.wait();
                    usize::from(l.check(&s, now).decision.is_ok())
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum();
        assert_eq!(passed, 1, "round {round}: only the first callback of {source} may pass");
    }
}

#[test]
fn first_callbacks_have_their_own_ceiling_and_ports_do_not_make_new_sources() {
    let mut h = LoginHarness::new();
    let max = rate_limit::LOGIN_FIRST_ATTEMPT_RATE_LIMIT.max;
    let fresh = |i: usize| format!("2001:db8:{:x}::1", i + 1);
    for i in 0..max {
        assert_eq!(h.call(&fresh(i), false), 302, "source {}", i + 1);
    }
    assert_eq!(h.call(&fresh(max), false), 429);
    assert_eq!(h.exchanges, max);
    assert_eq!(h.call(&fresh(0), false), 302);
    h.clock += rate_limit::LOGIN_FIRST_ATTEMPT_RATE_LIMIT.window_ms as f64 + 1.0;
    assert_eq!(h.call(&fresh(max), true), 302);

    let mut h = LoginHarness::new();
    let mut statuses = [Vec::new(), Vec::new(), Vec::new()];
    for i in 0..12 {
        let forms = [
            format!("198.51.100.50:{}", 40000 + i),
            format!("[2001:db8:7:7::{}]:{}", i + 1, 50000 + i),
            format!("2001:db8:8:8::{}%eth0", i + 1),
        ];
        for (n, f) in forms.iter().enumerate() {
            statuses[n].push(h.call(f, false));
        }
    }
    for st in &statuses {
        assert_eq!(st.iter().filter(|&&x| x == 429).count(), 2);
    }
    assert_eq!(h.exchanges, 30);
    assert_eq!(h.call("[::ffff:198.51.100.50]:6000", true), 429);
}

// ── site parts, docs, changelog, command list ───────────────────────────────

#[test]
fn site_parts_match_the_js() {
    let g = golden("html.json");
    let site = &g["site"];
    let each = |name: &str, f: &dyn Fn(&Value) -> Value| {
        for c in site[name].as_array().unwrap() {
            assert_eq!(f(&c["input"]), c["output"], "{name} {}", c["input"]);
        }
    };
    each("escapeHtml", &|v| json!(site_parts::escape_html(s(v))));
    each("publicDescription", &|v| json!(site_parts::public_description(s(v))));
    each("publicNote", &|v| json!(site_parts::public_note(s(v))));
    each("inlineMarkdown", &|v| json!(site_parts::inline_markdown(s(v))));
    each("formatDefault", &|v| json!(site_parts::format_default(v)));
    each("flavourOf", &|v| {
        let f = site_parts::flavour_of(s(v));
        json!({ "cls": f.cls, "name": f.name })
    });
    for c in site["renderContact"].as_array().unwrap() {
        // The Rust contact is typed: kinds other than email/url and non-string values have no Contact form.
        let contact: Option<Contact> = serde_json::from_value(c["input"].clone()).ok();
        if contact.is_none() && !c["input"].is_null() {
            assert_eq!(c["output"], json!(""), "{}", c["input"]);
            continue;
        }
        assert_eq!(site_parts::render_contact(contact.as_ref()), s(&c["output"]), "{}", c["input"]);
    }
    for c in site["renderSupport"].as_array().unwrap() {
        let input = opt_s(&c["input"]);
        assert_eq!(site_parts::render_support(input, site_parts::SUPPORT_LABEL), s(&c["output"]), "{}", c["input"]);
        assert_eq!(site_parts::render_support(input, "Join <us>"), s(&c["labelled"]), "{}", c["input"]);
    }
    for (key, label) in site["WHO_LABEL"].as_object().unwrap() {
        assert_eq!(site_parts::who_label(key), Some(s(label)));
    }
    assert_eq!(site_parts::who_label("strange"), None);
    assert_eq!(s(&site["REPO_URL"]), site_parts::REPO_URL);
    assert_eq!(s(&site["ISSUES_URL"]), site_parts::ISSUES_URL);
}

fn commands_of(v: &Value) -> Vec<CommandSummary> {
    serde_json::from_value(v.clone()).unwrap()
}

#[test]
fn docs_html_is_byte_identical_to_the_js() {
    let g = golden("html.json");
    let d = &g["docs"];
    let commands = commands_of(&d["commands"]);
    assert_eq!(docs::render_docs_toc(&d["catalog"]), s(&d["toc"]));
    assert_eq!(docs::render_docs_modules(&d["catalog"], &commands), s(&d["modules"]));
    assert_eq!(docs::render_core_commands(&commands), s(&d["core"]));
    for m in d["scalar"].as_array().unwrap() {
        let module = &d["catalog"]["modules"][s(&m["key"])];
        let got: Vec<Value> = site_parts::scalar_settings(module)
            .into_iter()
            .map(|(f, v)| json!({ "field": f["field"], "value": v }))
            .collect();
        assert_eq!(Value::Array(got), m["settings"], "{}", m["key"]);
    }
    let odd = &d["odd"];
    let odd_commands = commands_of(&odd["commands"]);
    assert_eq!(docs::render_docs_toc(&odd["catalog"]), s(&odd["toc"]));
    assert_eq!(docs::render_docs_modules(&odd["catalog"], &odd_commands), s(&odd["modules"]));
    assert_eq!(docs::render_core_commands(&odd_commands), s(&odd["core"]));
    assert_eq!(docs::module_id("levels"), s(&d["ids"]["module"]));
    assert_eq!(docs::command_id("rank"), s(&d["ids"]["command"]));
}

#[test]
fn changelog_html_is_byte_identical_to_the_js() {
    let g = golden("html.json");
    let c = &g["changelog"];
    let parsed = changelog::parse_changelog(s(&c["markdown"]));
    assert_eq!(serde_json::to_value(&parsed).unwrap(), c["parsed"]);
    assert!(!parsed.entries.is_empty());
    assert_eq!(changelog::render_latest_changes(&parsed.entries, changelog::LATEST_COUNT), s(&c["latest"]));
    assert_eq!(changelog::render_latest_changes(&parsed.entries, 5), s(&c["latest5"]));
    assert_eq!(changelog::render_changelog_entries(&parsed.entries), s(&c["entries"]));
    let odd = changelog::parse_changelog(s(&c["oddMarkdown"]));
    assert_eq!(serde_json::to_value(&odd).unwrap(), c["oddParsed"]);
    assert_eq!(changelog::render_changelog_entries(&odd.entries), s(&c["oddEntries"]));
    assert_eq!(changelog::render_latest_changes(&odd.entries, 3), s(&c["oddLatest"]));
    let none: Vec<ChangelogEntry> = Vec::new();
    assert_eq!(changelog::render_changelog_entries(&none), s(&c["empty"]));
    assert_eq!(serde_json::to_value(changelog::parse_changelog("")).unwrap(), c["emptyParsed"]);
    assert_eq!(changelog::entry_id("2026-01-02"), "lot-2026-01-02");
}

#[test]
fn command_list_matches_the_js_and_the_committed_file() {
    let g = golden("command-list.json");
    let input = g["input"].as_array().unwrap();
    let entries: Vec<CommandEntry<'_>> = input
        .iter()
        .map(|c| CommandEntry {
            module: c["module"].as_str(),
            permission_level: c["permissionLevel"]
                .as_u64()
                .map(|n| bot_config::constants::PermissionLevel::from_u8(n as u8).unwrap()),
            payload: &c["payload"],
        })
        .collect();
    let summary = command_list::summarize_commands(&entries);
    assert_eq!(serde_json::to_value(&summary).unwrap(), g["summary"]);
    assert_eq!(g["summary"], g["committed"], "src/dashboard/commands.json is current");
    let names: Vec<Value> = g["names"].as_array().unwrap().clone();
    let payloads: Vec<Value> = names.iter().map(|n| json!({ "name": n, "description": "" })).collect();
    let entries: Vec<CommandEntry<'_>> =
        payloads.iter().map(|p| CommandEntry { module: None, permission_level: None, payload: p }).collect();
    let sorted: Vec<String> = command_list::summarize_commands(&entries).into_iter().map(|c| c.name).collect();
    assert_eq!(json!(sorted), g["sorted"]);
}

// ── live cross-language seal check ───────────────────────────────────────────

fn run_node(ops: &Value) -> Value {
    let mut child = Command::new("node")
        .arg(root().join("rust/fixtures/gen/dash-auth-seal.mjs"))
        .current_dir(root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("node is on PATH");
    child.stdin.take().unwrap().write_all(ops.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "node failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn cookies_sealed_by_either_side_open_on_the_other() {
    let now = 1_800_000_000_000_i64;
    let exp = now + 3_600_000;
    let secrets = [SECRET.to_string(), "pässwörd-😀-secret-with-unicode-32+".to_string()];
    let payloads = [
        payload(exp as f64),
        json!({ "uid": "222222222222222222", "username": "bób 😀 \"q\" \\ </script>\u{2028}", "globalName": null,
                "avatar": "a_0123", "accessToken": "t".repeat(512), "exp": exp, "nested": { "a": [1, 2.5, null, true] } }),
    ];
    // Rust seals, JS opens.
    let mut ops = Vec::new();
    let mut expected = Vec::new();
    for secret in &secrets {
        let key = auth::derive_session_key(secret).unwrap();
        for p in &payloads {
            let token = auth::seal_session(p, &key);
            ops.push(json!({ "op": "open", "secret": secret, "token": token, "now": now }));
            expected.push(json!({ "payload": p }));
            ops.push(json!({ "op": "open", "secret": secret, "token": token, "now": exp }));
            expected.push(json!({ "payload": null }));
            // The Set-Cookie value the dashboard sends parses back to the same token in the JS.
            let set = auth::serialize_cookie(auth::SESSION_COOKIE, &token, &CookieOptions::default());
            let header = set.split(';').next().unwrap().to_string();
            ops.push(json!({ "op": "cookie", "header": format!("other=1; {header}") }));
            expected.push(json!({ "entries": [["other", "1"], [auth::SESSION_COOKIE, token]] }));
        }
    }
    // JS seals, Rust opens.
    for secret in &secrets {
        for p in &payloads {
            ops.push(json!({ "op": "seal", "secret": secret, "payload": p }));
        }
    }
    let results = run_node(&Value::Array(ops.clone()));
    let results = results.as_array().unwrap();
    for (i, exp_v) in expected.iter().enumerate() {
        assert_eq!(&results[i], exp_v, "op {}", ops[i]);
    }
    let mut i = expected.len();
    for secret in &secrets {
        let key = auth::derive_session_key(secret).unwrap();
        for p in &payloads {
            let token = s(&results[i]["token"]);
            assert_eq!(auth::unseal_session(Some(token), &key, now as f64).map(Value::Object).as_ref(), Some(p));
            assert!(auth::unseal_session(Some(token), &key, exp as f64).is_none());
            i += 1;
        }
    }
}
