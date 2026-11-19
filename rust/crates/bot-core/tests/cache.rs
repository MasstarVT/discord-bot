//! redis.js: the never-failing cache API, its fallbacks, the in-memory backend's Redis semantics, globToRegExp and
//! invalidateAny; and, with TEST_REDIS_URL, the same API against a Redis server (and JS-written values read back).

use std::sync::Arc;
use std::time::Duration;

use bot_core::cache::{Cache, CacheBackend, MemoryBackend, RedisBackend, RedisOptions, glob_to_regex};
use bot_core::clock::{ManualClock, SharedClock};
use serde_json::json;

fn memory_cache() -> (Cache, Arc<MemoryBackend>, ManualClock) {
    let clock = ManualClock::new(1_700_000_000_000);
    let shared: SharedClock = Arc::new(clock.clone());
    let backend = Arc::new(MemoryBackend::new(shared.clone()));
    (Cache::new(backend.clone(), shared), backend, clock)
}

#[tokio::test]
async fn cache_never_fails_and_returns_the_documented_fallbacks() {
    let (cache, backend, _) = memory_cache();
    backend.set_available(false);
    assert!(!cache.available());
    assert_eq!(cache.get("k").await, None);
    assert!(!cache.set("k", &json!({ "a": 1 }), 10).await);
    assert_eq!(cache.set_nx("k", &1, 10).await, None);
    assert!(!cache.del(&["k"]).await);
    assert!(!cache.del(&["a", "b"]).await);
    assert_eq!(cache.incr("c", 10).await, None);
    assert_eq!(cache.sliding_window("w", 1000, None).await, None);
    assert_eq!(cache.push_recent("r", "x", 5, 10).await, None);
    assert_eq!(cache.hget("h", "f").await, None);
    assert!(!cache.hset("h", "f", &1).await);
    assert!(!cache.hdel("h", "f").await);
    assert_eq!(cache.invalidate("guild:*:settings").await, 0);
    // Nothing to delete is a success without a command.
    assert!(cache.del::<&str>(&[]).await);
    assert!(cache.del(&[""]).await);
    // A value serde cannot write (a map with non-string keys) is refused like a BigInt or a cycle in the JS.
    backend.set_available(true);
    let mut bad = std::collections::HashMap::new();
    bad.insert(vec![1u8], 1);
    assert!(!cache.set("big", &bad, 10).await);
    assert_eq!(cache.set_nx("big", &bad, 10).await, None);
}

#[tokio::test]
async fn values_are_json_and_non_json_is_the_raw_string() {
    let (cache, backend, _) = memory_cache();
    assert!(cache.set("t:a", &json!({ "x": 1, "when": "1970-01-01T00:00:00.000Z" }), 60).await);
    assert_eq!(backend.raw("t:a").as_deref(), Some(r#"{"x":1,"when":"1970-01-01T00:00:00.000Z"}"#));
    assert_eq!(cache.get("t:a").await, Some(json!({ "x": 1, "when": "1970-01-01T00:00:00.000Z" })));
    assert!(backend.pttl("t:a") > 59_000);
    backend.put_raw("t:raw", "not json {", None);
    assert_eq!(cache.get("t:raw").await, Some(json!("not json {")));
    assert!(cache.set("t:s", "text", 0).await);
    assert_eq!(backend.raw("t:s").as_deref(), Some("\"text\""));
    assert_eq!(backend.pttl("t:s"), -1, "no TTL when the ttl is falsy");
    assert!(cache.set("t:f", &1, 1.2).await);
    assert_eq!(backend.pttl("t:f"), 2000, "Math.ceil(ttl)");
    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Shape {
        x: i64,
    }
    assert_eq!(cache.get_as::<Shape>("t:a").await, Some(Shape { x: 1 }));
    assert_eq!(cache.get_as::<Shape>("t:raw").await, None, "unreadable is a miss");
}

#[tokio::test]
async fn ttl_expiry_nx_counters_windows_lists_and_hashes() {
    let (cache, backend, clock) = memory_cache();
    assert_eq!(cache.set_nx("t:nx", &1, 30).await, Some(true));
    assert_eq!(cache.set_nx("t:nx", &1, 30).await, Some(false));
    clock.advance(30_000);
    assert_eq!(cache.set_nx("t:nx", &1, 30).await, Some(true), "expired");

    assert_eq!(cache.incr("t:n", 30).await, Some(1));
    assert_eq!(cache.incr("t:n", 30).await, Some(2));
    assert!(backend.pttl("t:n") > 25_000);
    backend.put_raw("t:str", "abc", None);
    assert_eq!(cache.incr("t:str", 30).await, None, "INCR on a non-integer falls back");
    // A failed EXPIRE never leaves a counter without a TTL.
    backend.fail_ops(&["expire"]);
    assert_eq!(cache.incr("t:new", 30).await, None);
    backend.fail_ops(&[]);
    assert_eq!(cache.get("t:new").await, None);

    assert_eq!(cache.sliding_window("t:w", 5000, None).await, Some(1));
    clock.advance(1000);
    assert_eq!(cache.sliding_window("t:w", 5000, None).await, Some(2));
    assert_eq!(cache.sliding_window("t:w", 5000, Some("same")).await, Some(3));
    assert_eq!(cache.sliding_window("t:w", 5000, Some("same")).await, Some(3), "a member counts once");
    clock.advance(4500);
    assert_eq!(cache.sliding_window("t:w", 5000, None).await, Some(3), "the first hit left the window");

    assert_eq!(cache.push_recent("t:l", "a", 2, 30).await, Some(vec!["a".to_string()]));
    assert_eq!(cache.push_recent("t:l", "b", 2, 30).await, Some(vec!["b".to_string(), "a".to_string()]));
    assert_eq!(cache.push_recent("t:l", "c", 2, 30).await, Some(vec!["c".to_string(), "b".to_string()]));
    assert_eq!(cache.get("t:l").await, None, "GET on a list is a WRONGTYPE error: the fallback");

    assert!(cache.hset("t:h", "f", &json!({ "y": 2 })).await);
    assert_eq!(cache.hget("t:h", "f").await, Some(json!({ "y": 2 })));
    assert!(cache.hdel("t:h", "f").await);
    assert_eq!(cache.hget("t:h", "f").await, None);
}

#[test]
fn glob_to_regexp() {
    assert!(glob_to_regex("guild:1.2:*").is_match("guild:1.2:settings"));
    assert!(!glob_to_regex("guild:1.2:*").is_match("guild:102:settings"), "a dot is literal");
    assert!(glob_to_regex("a?c").is_match("abc"));
}

#[tokio::test]
async fn invalidate_any_is_one_scan_pass_with_count_1000_deleting_only_the_listed_patterns() {
    let (cache, backend, _) = memory_cache();
    for k in ["guild:9:settings", "tag:cd:9:t1:u1", "xp:cooldown:9:u1", "voice:start:9:u2", "guild:99:settings", "guild:43:x"] {
        backend.put_raw(k, "1", None);
    }
    backend.clear_calls();
    let n = cache.invalidate_any("*:9:*", &["guild:9:*", "xp:cooldown:9:*", "voice:start:9:*"]).await;
    assert_eq!(n, 3);
    let mut left = backend.keys();
    left.sort();
    assert_eq!(left, vec!["guild:43:x", "guild:99:settings", "tag:cd:9:t1:u1"]);
    let calls = backend.calls();
    let scans: Vec<_> = calls.iter().filter(|c| c.op == "scan").collect();
    assert_eq!(scans.len(), 1);
    assert_eq!(scans[0].args, vec!["0", "MATCH", "*:9:*", "COUNT", "1000"]);
    assert!(calls.iter().any(|c| c.op == "unlink"), "UNLINK, never DEL");
    assert!(!calls.iter().any(|c| c.op == "del"));
}

#[tokio::test]
async fn invalidate_follows_the_cursor_and_returns_what_it_deleted_before_a_failure() {
    let (cache, backend, _) = memory_cache();
    for i in 0..2500 {
        backend.put_raw(&format!("guild:{i}:settings"), "1", None);
    }
    assert_eq!(cache.invalidate("guild:*:settings").await, 2500);
    assert_eq!(backend.calls().iter().filter(|c| c.op == "scan").count(), 3);
    for i in 0..10 {
        backend.put_raw(&format!("guild:{i}:x"), "1", None);
    }
    backend.fail_ops(&["unlink"]);
    assert_eq!(cache.invalidate("guild:*").await, 0);
}

#[tokio::test]
async fn redis_backend_starts_disconnected_and_falls_back_fast() {
    let backend = RedisBackend::connect(RedisOptions::new(Some("redis://127.0.0.1:1"))).unwrap();
    let cache = Cache::new(Arc::new(backend.clone()), Arc::new(bot_core::clock::SystemClock));
    let started = std::time::Instant::now();
    assert!(!cache.available());
    assert_eq!(cache.get("k").await, None);
    assert!(!cache.set("k", &1, 10).await);
    assert_eq!(cache.set_nx("k", &1, 10).await, None);
    assert_eq!(cache.incr("c", 10).await, None);
    assert!(started.elapsed() < Duration::from_millis(500), "fails fast instead of queueing");
    backend.disconnect();
}

fn redis_url() -> Option<String> {
    match std::env::var("TEST_REDIS_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            println!("skipped: TEST_REDIS_URL not set");
            None
        }
    }
}

#[tokio::test]
async fn redis_backend_against_a_server() {
    let Some(url) = redis_url() else { return };
    let backend = RedisBackend::connect(RedisOptions::new(Some(&url))).unwrap();
    assert!(backend.wait_ready(5000).await, "connects");
    let cache = Cache::new(Arc::new(backend.clone()), Arc::new(bot_core::clock::SystemClock));
    let p = format!("bot-core-test:{}:", std::process::id());
    let k = |s: &str| format!("{p}{s}");
    assert!(cache.set(&k("a"), &json!({ "x": 1, "when": "1970-01-01T00:00:00.000Z" }), 60).await);
    assert_eq!(cache.get(&k("a")).await, Some(json!({ "x": 1, "when": "1970-01-01T00:00:00.000Z" })));
    assert_eq!(cache.set_nx(&k("nx"), &1, 30).await, Some(true));
    assert_eq!(cache.set_nx(&k("nx"), &1, 30).await, Some(false));
    assert_eq!(cache.incr(&k("n"), 30).await, Some(1));
    assert_eq!(cache.incr(&k("n"), 30).await, Some(2));
    assert_eq!(cache.sliding_window(&k("w"), 5000, None).await, Some(1));
    assert_eq!(cache.sliding_window(&k("w"), 5000, None).await, Some(2));
    assert_eq!(cache.push_recent(&k("l"), "a", 2, 30).await, Some(vec!["a".into()]));
    assert_eq!(cache.push_recent(&k("l"), "b", 2, 30).await, Some(vec!["b".into(), "a".into()]));
    assert_eq!(cache.push_recent(&k("l"), "c", 2, 30).await, Some(vec!["c".into(), "b".into()]));
    assert!(cache.hset(&k("h"), "f", &json!({ "y": 2 })).await);
    assert_eq!(cache.hget(&k("h"), "f").await, Some(json!({ "y": 2 })));
    assert!(cache.hdel(&k("h"), "f").await);
    assert!(cache.set(&format!("guild:{p}42:x"), &1, 60).await);
    assert!(cache.set(&format!("xp:cooldown:{p}42:7"), &1, 60).await);
    assert!(cache.set(&format!("guild:{p}43:x"), &1, 60).await);
    let n = cache.invalidate_any(&format!("*:{p}42:*"), &[format!("guild:{p}42:*"), format!("xp:cooldown:{p}42:*")]).await;
    assert_eq!(n, 2);
    assert_eq!(cache.get(&format!("guild:{p}43:x")).await, Some(json!(1)));
    assert!(cache.del(&[k("a"), k("nx"), k("n"), k("w"), k("l"), format!("guild:{p}43:x")]).await);
    assert!(backend.get(&k("a")).await.unwrap().is_none());
    backend.disconnect();
    assert!(!cache.available());
}

/// redis.js sends a negative TTL as is (`if (ttl)` is true for it): `SET ... EX -n` is refused by Redis, so nothing is
/// written (set false, setNX null), and `EXPIRE key -n` after the first INCR deletes the counter. A negative TTL must
/// never become a key without an expiry.
#[tokio::test]
async fn a_negative_ttl_is_refused_like_redis_refuses_it() {
    let (cache, backend, _) = memory_cache();
    assert!(!cache.set("neg:a", &1, -5).await);
    assert_eq!(backend.raw("neg:a"), None);
    assert!(!cache.set("neg:f", &1, -0.5).await);
    assert_eq!(backend.raw("neg:f"), None);
    assert_eq!(cache.set_nx("neg:b", &1, -5i64).await, None);
    assert_eq!(backend.raw("neg:b"), None);
    assert_eq!(cache.incr("neg:c", -5).await, Some(1));
    assert_eq!(backend.raw("neg:c"), None, "EXPIRE with a negative TTL deletes the key");
    assert_eq!(cache.incr("neg:c", -5).await, Some(1));
    // Zero and NaN stay falsy: written without a TTL.
    assert!(cache.set("neg:z", &1, 0).await);
    assert_eq!(backend.pttl("neg:z"), -1);
    assert!(cache.set("neg:n", &1, f64::NAN).await);
    assert_eq!(backend.pttl("neg:n"), -1);
    assert_eq!(cache.incr("neg:d", 0).await, Some(1));
    assert_eq!(backend.pttl("neg:d"), -1);
}
