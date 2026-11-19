//! Behaviour ports of the JS tests that check what happens rather than a value: async helpers (foundation.test.js),
//! the logger line format, safeRegex timeouts and quarantine, the restart brake's state file, the health server, and
//! the constants other crates compare against.

mod common;

use bot_util::async_util::{self, KeyedMutex};
use bot_util::logger::{self, LogArg};
use bot_util::safe_regex::{self, Quarantine, SafeRegex};
use bot_util::{fatal_gateway, gateway_features, placeholder_parser as ph, restart_brake, time, timezones as tzs};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── async (foundation.test.js §6) ──────────────────────────────────────────────

#[test]
fn ack_budget_ms() {
    let now = 1_700_000_000_000.0;
    let a = async_util::ack_budget_ms_at(now - 1000.0, 400.0, now);
    assert!((1100..=1200).contains(&a), "got {a}");
    assert_eq!(async_util::ack_budget_ms_at(now - 5000.0, 400.0, now), 0);
    let b = async_util::ack_budget_ms_at(now - 1000.0, 0.0, now);
    assert!((1500..=1600).contains(&b), "got {b}");
    let live = async_util::ack_budget_ms(bot_util::js::now_ms() - 1000.0, async_util::ACK_RESERVE_MS);
    assert!((1100..=1200).contains(&live), "got {live}");
}

#[derive(Debug)]
struct E(&'static str);
impl std::fmt::Display for E {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[tokio::test]
async fn with_timeout_resolves_rejects_and_logs_a_late_failure() {
    let ok = async_util::with_timeout(async { Ok::<_, E>(7) }, 50, "fast").await;
    assert_eq!(ok.unwrap(), 7);
    let rej = async_util::with_timeout(async { Err::<i32, _>(E("inner")) }, 50, "rej").await;
    assert!(matches!(rej, Err(async_util::WithTimeoutError::Inner(E("inner")))));

    let lines = capture_logs();
    let slow = async {
        tokio::time::sleep(Duration::from_millis(40)).await;
        Err::<i32, _>(E("late"))
    };
    let r = async_util::with_timeout(slow, 5, "slow thing").await;
    match r {
        Err(async_util::WithTimeoutError::Timeout(t)) => assert_eq!(t.to_string(), "slow thing timed out"),
        other => panic!("expected a timeout, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(90)).await;
    let logged = lines.lines().join("\n");
    drop(lines);
    assert!(logged.contains("slow thing failed after timeout late"), "the late failure is logged: {logged}");
}

#[tokio::test]
async fn map_limit_keeps_order_respects_the_limit_and_never_rejects() {
    let running = Arc::new(Mutex::new((0usize, 0usize)));
    let r = running.clone();
    let out = async_util::map_limit(vec![1u64, 2, 3, 4, 5, 6], 2, move |n, _i| {
        let r = r.clone();
        async move {
            {
                let mut g = r.lock().unwrap();
                g.0 += 1;
                g.1 = g.1.max(g.0);
            }
            tokio::time::sleep(Duration::from_millis(5 * (7 - n))).await;
            r.lock().unwrap().0 -= 1;
            if n == 3 { Err("three") } else { Ok(n * 10) }
        }
    })
    .await;
    assert_eq!(running.lock().unwrap().1, 2, "peak concurrency");
    let status: Vec<bool> = out.iter().map(Result::is_ok).collect();
    assert_eq!(status, vec![true, true, false, true, true, true]);
    let values: Vec<u64> = out.iter().filter_map(|r| r.as_ref().ok().copied()).collect();
    assert_eq!(values, vec![10, 20, 40, 50, 60]);
    assert_eq!(out[2], Err("three"));
    let empty = async_util::map_limit(Vec::<u8>::new(), 3, |_, _| async { Ok::<u8, ()>(1) }).await;
    assert!(empty.is_empty());
}

#[tokio::test]
async fn keyed_mutex_serialises_per_key_links_at_call_time_and_isolates_failures() {
    let mutex = KeyedMutex::new();
    let order = Arc::new(Mutex::new(Vec::<&str>::new()));
    let (o1, o2, o3) = (order.clone(), order.clone(), order.clone());
    let first = mutex.run("k", move || async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        o1.lock().unwrap().push("first");
    });
    let second = mutex.run("k", move || async move { o2.lock().unwrap().push("second") });
    let other = mutex.run("other", move || async move { o3.lock().unwrap().push("other") });
    futures::join!(first, second, other);
    let o = order.lock().unwrap().clone();
    let pos = |s: &str| o.iter().position(|x| *x == s).unwrap();
    assert!(pos("first") < pos("second"));
    assert!(pos("other") < pos("first"), "a different key does not wait");

    let bad = mutex.run("k", || async { Err::<&str, &str>("bad") });
    let good = mutex.run("k", || async { Ok::<&str, &str>("good") });
    assert_eq!(bad.await, Err("bad"));
    assert_eq!(good.await, Ok("good"));

    // Linked into the chain when run() is called, even when the caller awaits later (or never).
    let seen = Arc::new(Mutex::new(Vec::<&str>::new()));
    let (s1, s2) = (seen.clone(), seen.clone());
    let a = mutex.run("sync", move || async move {
        s1.lock().unwrap().push("a");
        tokio::time::sleep(Duration::from_millis(10)).await;
        s1.lock().unwrap().push("a-end");
    });
    let b = mutex.run("sync", move || async move { s2.lock().unwrap().push("b") });
    b.await;
    a.await;
    assert_eq!(*seen.lock().unwrap(), vec!["a", "a-end", "b"]);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(mutex.is_empty(), "idle keys are removed");
}

#[tokio::test]
async fn keyed_mutex_releases_the_key_after_a_panic() {
    let mutex = KeyedMutex::new();
    let boom = mutex.run("p", || async { panic!("job panicked") });
    let after = mutex.run("p", || async { 5 });
    let joined = tokio::spawn(boom).await;
    assert!(joined.is_err());
    assert_eq!(after.await, 5);
}

#[tokio::test]
async fn semaphore_limits_and_queues_in_order() {
    let sem = async_util::create_semaphore(2);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut futs = Vec::new();
    for i in 0..5 {
        let l = log.clone();
        futs.push(sem.run(move || async move {
            l.lock().unwrap().push(format!("start {i}"));
            tokio::time::sleep(Duration::from_millis(10)).await;
            l.lock().unwrap().push(format!("end {i}"));
            i
        }));
    }
    assert_eq!(sem.queued(), 3, "callers take their place when run() is called");
    let out = futures::future::join_all(futs).await;
    assert_eq!(out, vec![0, 1, 2, 3, 4]);
    assert_eq!(sem.active(), 0);
    let l = log.lock().unwrap();
    assert_eq!(&l[..2], &["start 0".to_string(), "start 1".to_string()]);
}

#[tokio::test(start_paused = true)]
async fn throttle_allows_one_call_per_interval_per_key() {
    let allow = async_util::create_throttle(30);
    assert!(allow.allow("a"));
    assert!(!allow.allow("a"));
    assert!(allow.allow("b"));
    tokio::time::advance(Duration::from_millis(45)).await;
    assert!(allow.allow("a"));
    assert!(!allow.allow("a"));
}

// ── logger (foundation.test.js §8) ─────────────────────────────────────────────

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Captures logger lines until dropped (one capture at a time: the logger is global).
struct Capture {
    lines: Arc<Mutex<Vec<String>>>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Capture {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        logger::set_sink(None);
        logger::set_shard_id(None);
    }
}

fn capture_logs() -> Capture {
    let guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let lines = Arc::new(Mutex::new(Vec::new()));
    let l = lines.clone();
    logger::set_sink(Some(Box::new(move |_lvl, line| l.lock().unwrap().push(line.to_string()))));
    Capture { lines, _guard: guard }
}

#[test]
fn logger_inspects_context_objects_and_prefixes_the_shard_id() {
    let lines = capture_logs();
    logger::error(&["boom".into(), LogArg::Json(json!({"guildId": "1", "nested": {"a": [1, 2]}}))]);
    logger::info(&["plain".into(), 5.into(), LogArg::Null]);
    logger::set_shard_id(Some("3".into()));
    logger::info(&["with shard".into()]);
    logger::set_shard_id(None);
    logger::warn(&["line\nbreak\r and \u{1b}[31m escape \u{7f}".into()]);
    bot_util::log_success!("ok", 1.5, true);
    let l = lines.lines();
    drop(lines);
    assert!(l[0].ends_with("boom { guildId: '1', nested: { a: [ 1, 2 ] } }"), "{}", l[0]);
    assert!(!l[0].contains("[object Object]"));
    assert!(l[0].contains("[ERROR]"));
    assert!(l[1].ends_with("[INFO ] plain 5 null"), "{}", l[1]);
    assert!(!l[1].contains("Shard"));
    assert!(l[2].contains("[Shard 3] with shard"), "{}", l[2]);
    assert!(l[3].contains("line\\nbreak\\r and \\x1b[31m escape \\x7f"), "{}", l[3]);
    assert!(l[4].contains("[OK   ] ok 1.5 true"), "{}", l[4]);
    // [YYYY-MM-DD HH:MM:SS] [LEVEL] ...
    let re = bot_util::jsre::JsRegex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\] \[INFO \] plain", "");
    assert!(re.test(&l[1]), "{}", l[1]);
}

#[test]
fn logger_error_prints_the_stack_and_debug_respects_node_env() {
    let lines = capture_logs();
    logger::error(&["failed".into(), LogArg::Error { message: "bad thing".into(), stack: Some("Error: bad thing\n    at here".into()) }]);
    let before = std::env::var("NODE_ENV").ok();
    // SAFETY: the logger tests hold LOG_LOCK, and nothing else in this binary reads NODE_ENV concurrently.
    unsafe { std::env::set_var("NODE_ENV", "production") };
    logger::debug(&["hidden".into()]);
    unsafe { std::env::set_var("NODE_ENV", "development") };
    logger::debug(&["shown".into()]);
    match before {
        Some(v) => unsafe { std::env::set_var("NODE_ENV", v) },
        None => unsafe { std::env::remove_var("NODE_ENV") },
    }
    let l = lines.lines();
    drop(lines);
    assert!(l[0].ends_with("failed bad thing"));
    assert_eq!(l[1], "Error: bad thing\n    at here");
    assert_eq!(l.len(), 3);
    assert!(l[2].ends_with("[DEBUG] shown"));
}

#[test]
fn inspect_matches_node() {
    // Expected strings are Node's util.inspect(value, { depth: 3, breakLength: Infinity }).
    let cases = [
        (json!({"guildId": "1", "nested": {"a": [1, 2]}}), "{ guildId: '1', nested: { a: [ 1, 2 ] } }"),
        (json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]), "[\n  1, 2, 3, 4,  5,\n  6, 7, 8, 9, 10\n]"),
        (json!(["a", "b"]), "[ 'a', 'b' ]"),
        (json!([[1, [2, [3, [4, [5]]]]]]), "[\n  [ 1, [ 2, [ 3, [Array] ] ] ]\n]"),
        (json!({"x-y": "it's", "q": "say \"hi\" it's", "s": "a\nb\\", "e": [], "o": {}, "n": null, "$ok": 1, "_u": 2}),
            "{ 'x-y': \"it's\", q: `say \"hi\" it's`, s: 'a\\nb\\\\', e: [], o: {}, n: null, '$ok': 1, _u: 2 }"),
        (json!({"a": {"b": {"c": {"d": {"e": 1}}}}}), "{\n  a: { b: { c: { d: [Object] } } }\n}"),
        (json!({"a": 1, "b": {"c": {"d": {}}}}), "{ a: 1, b: { c: { d: {} } } }"),
        (json!({"a": [{"b": {"c": 1}}]}), "{\n  a: [ { b: { c: 1 } } ]\n}"),
        (json!([1, 2, 3, 4, 5, 6, 7]), "[\n  1, 2, 3, 4,\n  5, 6, 7\n]"),
        (json!(["a", "bb", "ccc", "dddd", "e", "f", "g"]), "[\n  'a',   'bb',\n  'ccc', 'dddd',\n  'e',   'f',\n  'g'\n]"),
        (json!({"a": {"b": {"c": {"d": 1}}}}), "{\n  a: { b: { c: { d: 1 } } }\n}"),
        (json!([{"a": 1}, {"b": 2}, {"c": 3}, {"d": 4}, {"e": 5}, {"f": 6}, {"g": 7}]), "[\n  { a: 1 }, { b: 2 },\n  { c: 3 }, { d: 4 },\n  { e: 5 }, { f: 6 },\n  { g: 7 }\n]"),
        (json!({"t": "\u{7}\u{7f}\u{85}", "k": "tab\there"}), "{ t: '\\x07\\x7F\\x85', k: 'tab\\there' }"),
        (json!(["xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "y", "z", "w", "v", "u", "t"]), "[ 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx', 'y', 'z', 'w', 'v', 'u', 't' ]"),
    ];
    let hundred_one: Vec<u64> = (0..101).map(|i| i % 3).collect();
    let want = format!("[\n{}  0, 1, 2, 0,\n  ... 1 more item\n]", "  0, 1, 2, 0, 1, 2, 0, 1, 2, 0, 1, 2,\n".repeat(8));
    assert_eq!(logger::inspect(&json!(hundred_one)), want);
    for (v, want) in cases {
        let got = logger::inspect(&v);
        // Objects with nested containers three levels down break over lines, as Node does.
        if got != want {
            panic!("inspect({v})\n got: {got:?}\nwant: {want:?}");
        }
    }
}

// ── safeRegex (utils.test.js) ──────────────────────────────────────────────────

#[test]
fn safe_regex_rejects_dangerous_or_disallowed_patterns() {
    for bad in ["(a+)+", "(a|aa)*", r"(\w+\s?)*", "(.*a){5}", "((a+))+", "(a{2})+", r"\1", r"(a)\k<x>"] {
        assert!(safe_regex::validate_regex(bad, "i").is_err(), "{bad}");
    }
    assert!(safe_regex::validate_regex("abc", "g").is_err());
    assert!(safe_regex::validate_regex("abc", "y").is_err());
    assert!(safe_regex::validate_regex("abc", "v").is_err());
    assert!(safe_regex::validate_regex(&"a".repeat(201), "i").is_err());
    assert!(safe_regex::validate_regex("", "i").is_err());
    assert!(safe_regex::validate_regex("(", "i").is_err());
    let r = safe_regex::validate_regex("[unclosed", "i").unwrap_err();
    assert!(r.starts_with("Invalid regular expression: /[unclosed/i: "), "{r}");
    assert!(!r.contains("Invalid regular expression: Invalid regular expression"));
    assert_eq!(safe_regex::validate_regex(&"a".repeat(201), "i").unwrap_err(), "Pattern is 201 characters (max 200).");
    assert_eq!(safe_regex::validate_regex("abc", "g").unwrap_err(), "Flag \"g\" is not allowed (use i, m, s or u).");
    assert!(safe_regex::validate_regex("(a+)+", "i").unwrap_err().starts_with("Nested repetition"));
    assert!(safe_regex::validate_regex(r"\1", "i").unwrap_err().starts_with("Backreferences"));
    for good in [r"^hello\s+world$", r"[a-z]+@[a-z]+\.com", "(foo|bar)baz", "(ab)+", "(a|b)?", "[(]+", r"\\1", "a+?b", "(?:ab)+c", "(?<n>ab)+"] {
        assert_eq!(safe_regex::validate_regex(good, "i"), Ok(()), "{good}");
    }
    assert_eq!(safe_regex::validate_regex("abc", "iimm"), Ok(()));
}

#[test]
fn compile_safe_regex_validates_compiles_and_caches() {
    let a = safe_regex::compile_safe_regex("^hello$", "i").unwrap();
    assert_eq!(a.flags(), "i");
    assert!(Arc::ptr_eq(&a, &safe_regex::compile_safe_regex("^hello$", "i").unwrap()));
    assert!(safe_regex::compile_safe_regex("(a+)+", "i").is_none());
    assert!(safe_regex::compile_safe_regex("x", "g").is_none());
}

#[test]
fn safe_regex_test_matches_normally_and_survives_catastrophic_backtracking() {
    let q = Quarantine::new();
    let re = |p: &str, f: &str| Arc::new(SafeRegex::new(p, f).unwrap());
    assert!(safe_regex::safe_regex_test_in(&q, &re("^hello", "i"), "Hello there", None));
    assert!(!safe_regex::safe_regex_test_in(&q, &re("^hello", "i"), "nope", None));
    assert!(safe_regex::safe_regex_test_in(&q, &re("a", "g"), "aaa", None));
    let slow = re("(a+)+$", "");
    let t0 = Instant::now();
    assert!(!safe_regex::safe_regex_test_in(&q, &slow, &format!("{}!", "a".repeat(40)), None));
    assert!(t0.elapsed() < Duration::from_millis(500), "timed out quickly: {:?}", t0.elapsed());
    assert!(q.is_quarantined(&slow));
    assert!(!safe_regex::safe_regex_test_in(&q, &slow, "aaa", None), "quarantined: skipped");
    assert!(q.is_quarantined(&re("(a+)+$", "")), "keyed by source and flags");
    assert!(!q.is_quarantined(&re("never-used-regex", "")));
    assert!(safe_regex::runaway_matches() >= 1, "the abandoned match is counted");
}

#[test]
fn safe_regex_first_match_returns_the_first_index_and_releases_innocent_regexes() {
    let q = Quarantine::new();
    let re = |p: &str, f: &str| Some(Arc::new(SafeRegex::new(p, f).unwrap()));
    assert_eq!(safe_regex::safe_regex_first_match_in(&q, &[re("x", "i"), re("hello", "i"), re("hel", "i")], "say hello", None), 1);
    assert_eq!(safe_regex::safe_regex_first_match_in(&q, &[re("x", "i")], "nothing here", None), -1);
    assert_eq!(safe_regex::safe_regex_first_match_in(&q, &[], "x", None), -1);
    let innocent = re("x", "i");
    let slow = re("(b+)+$", "");
    let t0 = Instant::now();
    let idx = safe_regex::safe_regex_first_match_in(&q, &[innocent.clone(), slow.clone(), re("hello", "i")], &format!("{}! hello", "b".repeat(40)), None);
    assert_eq!(idx, 2);
    assert!(t0.elapsed() < Duration::from_millis(1000));
    assert!(!q.is_quarantined(innocent.as_ref().unwrap()));
    assert!(q.is_quarantined(slow.as_ref().unwrap()));
}

#[test]
fn escape_regex_output_compiles_under_the_u_flag() {
    let escaped = safe_regex::escape_regex("foo-bar/baz");
    assert!(SafeRegex::new(&escaped, "iu").is_ok());
    assert_eq!(escaped, "foo-bar/baz");
    assert_eq!(safe_regex::escape_regex("a.b*c"), r"a\.b\*c");
    for ch in ".*+?^${}()|[]\\".chars() {
        let re = SafeRegex::new(&safe_regex::escape_regex(&ch.to_string()), "u").unwrap();
        assert!(re.test_str(&ch.to_string()), "{ch}");
    }
}

#[test]
fn contains_word_matches_whole_words_only() {
    assert!(!safe_regex::contains_word("Scunthorpe", "cunt", false));
    assert!(safe_regex::contains_word("you are a jerk!", "jerk", false));
    assert!(safe_regex::contains_word("a foo-bar!", "foo-bar", false));
    assert!(safe_regex::contains_word("JERK", "jerk", false));
    assert!(!safe_regex::contains_word("JERK", "jerk", true));
    assert!(!safe_regex::contains_word("x", "", false));
    assert!(!safe_regex::contains_word("café au lait", "caf", false));
    assert!(safe_regex::contains_word("a (b) c", "(b)", false));
}

// ── restartBrake: the state file the JS deployment wrote ───────────────────────

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bot-util-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn restart_brake_reads_and_writes_the_js_state_file() {
    assert_eq!(restart_brake::brake_delay_ms(0), 0);
    assert_eq!(restart_brake::brake_delay_ms(5), 0);
    assert_eq!(restart_brake::brake_delay_ms(6), 120_000);
    assert_eq!(restart_brake::brake_delay_ms(9), 960_000);
    assert_eq!(restart_brake::brake_delay_ms(10), 1_800_000);
    assert_eq!(restart_brake::brake_delay_ms(50), 1_800_000);

    let dir = temp_dir("brake");
    std::fs::create_dir_all(&dir).unwrap();
    let now = 1_700_000_000_000.0;
    // What JSON.stringify({ starts }) wrote: integers, an old entry, a future one, and junk.
    std::fs::write(dir.join("restart-brake.json"), format!("{{\"starts\":[{},{},{},\"x\",null,{}]}}", now - 4_000_000.0, now - 60_000.0, now - 1.5, now + 10.0)).unwrap();
    let r = restart_brake::record_start(&dir, now);
    assert_eq!(r.starts, 3, "the old, future and junk entries are dropped, this start is added");
    assert_eq!(r.delay_ms, 0);
    assert_eq!(r.file.as_deref(), Some(dir.join("restart-brake.json").as_path()));
    assert!(r.warning.is_none());
    let written = std::fs::read_to_string(dir.join("restart-brake.json")).unwrap();
    assert_eq!(written, "{\"starts\":[1699999940000,1699999999998.5,1700000000000]}");
    assert!(!dir.join("restart-brake.json.tmp").exists());

    for k in 1..=5 {
        restart_brake::record_start(&dir, now + k as f64);
    }
    let r = restart_brake::record_start(&dir, now + 6.0);
    assert_eq!(r.starts, 9);
    assert_eq!(r.delay_ms, 960_000);

    // An unreadable file starts a fresh history.
    std::fs::write(dir.join("restart-brake.json"), "not json").unwrap();
    assert_eq!(restart_brake::record_start(&dir, now).starts, 1);
    std::fs::write(dir.join("restart-brake.json"), "null").unwrap();
    assert_eq!(restart_brake::record_start(&dir, now).starts, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_brake_falls_back_and_then_disables_itself() {
    let base = temp_dir("brake-fallback");
    std::fs::create_dir_all(&base).unwrap();
    // A file where a directory should be: neither location can be created.
    let blocked = base.join("blocked");
    std::fs::write(&blocked, "file").unwrap();
    let state_dir = blocked.join("state");
    let fallback = base.join("fallback");
    let r = restart_brake::record_start_with_fallback(&state_dir, &fallback, 1_000.0);
    assert_eq!(r.file.as_deref(), Some(fallback.join("restart-brake.json").as_path()));
    let w = r.warning.unwrap();
    assert!(w.starts_with("Could not write "), "{w}");

    let r = restart_brake::record_start_with_fallback(&state_dir, &blocked.join("also"), 1_000.0);
    assert_eq!(r.starts, 1);
    assert_eq!(r.delay_ms, 0);
    assert!(r.file.is_none());
    assert!(r.warning.unwrap().ends_with("; restart brake disabled"));
    let _ = std::fs::remove_dir_all(&base);
}

// ── healthServer ───────────────────────────────────────────────────────────────

async fn http(addr: std::net::SocketAddr, raw: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", addr.port())).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).to_string()
}

#[tokio::test]
async fn health_server_endpoints() {
    use bot_util::health_server::{start_health_server, Readiness, ReadinessCheck};
    let state = Arc::new(Mutex::new(0u8));
    let s2 = state.clone();
    let check: ReadinessCheck = Arc::new(move || {
        let v = *s2.lock().unwrap();
        Box::pin(async move {
            match v {
                0 => Ok(Readiness::ready()),
                1 => Ok(Readiness::not_ready("redis-down")),
                _ => Err("boom".to_string()),
            }
        })
    });
    let server = start_health_server(0, check, |e| panic!("{e}")).await.expect("listening");
    let addr = server.local_addr;
    let get = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");

    let r = http(addr, &get("/healthz")).await;
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
    assert!(r.contains("Content-Type: application/json\r\n") && r.contains("Cache-Control: no-store\r\n"));
    assert!(r.ends_with("\r\n\r\n{\"status\":\"alive\"}"), "{r}");

    let r = http(addr, &get("/readyz?x=1")).await;
    assert!(r.starts_with("HTTP/1.1 200 OK") && r.ends_with("{\"status\":\"ready\"}"), "{r}");
    *state.lock().unwrap() = 1;
    let r = http(addr, &get("/readyz")).await;
    assert!(r.starts_with("HTTP/1.1 503 Service Unavailable") && r.ends_with("{\"status\":\"not-ready\",\"reason\":\"redis-down\"}"), "{r}");
    *state.lock().unwrap() = 2;
    let r = http(addr, &get("/readyz")).await;
    assert!(r.ends_with("{\"status\":\"not-ready\",\"reason\":\"shard-not-ready\",\"error\":\"boom\"}"), "{r}");

    let r = http(addr, &get("/nope")).await;
    assert!(r.starts_with("HTTP/1.1 404 Not Found") && r.ends_with("{\"error\":\"not found\"}"), "{r}");
    let r = http(addr, "POST /healthz HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").await;
    assert!(r.starts_with("HTTP/1.1 405 Method Not Allowed") && r.ends_with("{\"error\":\"method not allowed\"}"), "{r}");
    let r = http(addr, "HEAD /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 200 OK") && r.ends_with("\r\n\r\n"), "HEAD has no body: {r}");

    // Keep-alive: two requests on one connection.
    let r = http(addr, "GET /healthz HTTP/1.1\r\nHost: x\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
    assert_eq!(r.matches("HTTP/1.1 ").count(), 2, "{r}");
    server.close();
}

#[tokio::test]
async fn health_server_reports_a_bind_error() {
    use bot_util::health_server::{start_health_server, Readiness, ReadinessCheck};
    let busy = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let errors = Arc::new(Mutex::new(0));
    let e2 = errors.clone();
    let check: ReadinessCheck = Arc::new(|| Box::pin(async { Ok(Readiness::ready()) }));
    let r = start_health_server(port, check, move |_| *e2.lock().unwrap() += 1).await;
    // On some systems the dual-stack socket can still bind next to an IPv4-only one.
    if r.is_none() {
        assert_eq!(*errors.lock().unwrap(), 1);
    }
}

// ── fatalGateway, gatewayFeatures ──────────────────────────────────────────────

#[test]
fn fatal_gateway_codes() {
    assert_eq!(fatal_gateway::fatal_code_from_error(Some("TokenInvalid"), None), Some(4004));
    assert_eq!(fatal_gateway::fatal_code_from_error(None, Some("Authentication failed")), Some(4004));
    assert_eq!(fatal_gateway::fatal_code_from_error(None, Some("Used invalid intents")), Some(4013));
    assert_eq!(fatal_gateway::fatal_code_from_error(None, Some("Used disallowed intents")), Some(4014));
    assert_eq!(fatal_gateway::fatal_code_from_error(None, Some("other")), None);
    assert_eq!(fatal_gateway::fatal_close_code(4014).unwrap().name, "Disallowed intents");
}

#[test]
fn presences_switch() {
    for (v, on) in [(None, true), (Some(""), true), (Some("0"), true), (Some("1"), false), (Some(" TRUE "), false), (Some("yes"), false), (Some("no"), true)] {
        assert_eq!(gateway_features::presences_enabled_from(v), on, "{v:?}");
    }
}

// ── Constants against the JS exports ───────────────────────────────────────────

#[test]
fn constants_match_the_js() {
    let c = common::cases("constants");
    let k = &c[0].1;
    let placeholders: serde_json::Map<String, Value> = ph::placeholders().into_iter().map(|(a, d)| (a.to_string(), json!(d))).collect();
    assert_eq!(Value::Object(placeholders), k["PLACEHOLDERS"]);
    assert_eq!(json!(ph::placeholder_list()), k["PLACEHOLDER_LIST"]);
    let abbr: serde_json::Map<String, Value> = time::ZONE_ABBREVIATIONS.iter().map(|(a, z)| (a.to_string(), json!(z))).collect();
    assert_eq!(Value::Object(abbr), k["ZONE_ABBREVIATIONS"]);
    assert_eq!(json!(time::WEEKDAY_NAMES), k["WEEKDAY_NAMES"]);
    let legacy: serde_json::Map<String, Value> = tzs::LEGACY_TZ_NAMES.iter().map(|(a, z)| (a.to_string(), json!(z))).collect();
    assert_eq!(Value::Object(legacy), k["LEGACY_TZ_NAMES"]);
    assert_eq!(json!(tzs::POPULAR_TIMEZONES), k["POPULAR_TIMEZONES"]);
    let templates: serde_json::Map<String, Value> = tzs::TIMEZONE_ROLE_TEMPLATES
        .iter()
        .map(|t| {
            let roles: Vec<Value> = t.roles.iter().map(|r| json!({"name": r.name, "zone": r.zone, "label": r.label, "description": r.description})).collect();
            (t.key.to_string(), json!({"label": t.label, "roles": roles}))
        })
        .collect();
    assert_eq!(Value::Object(templates), k["TIMEZONE_ROLE_TEMPLATES"]);
    let past: serde_json::Map<String, Value> = tzs::TIMEZONE_TEMPLATE_PAST_ZONES.iter().map(|(k, sets)| (k.to_string(), json!(sets))).collect();
    assert_eq!(Value::Object(past), k["TIMEZONE_TEMPLATE_PAST_ZONES"]);
    let p = tzs::TIMEZONE_PANEL;
    assert_eq!(json!({"type": p.kind, "layout": p.layout, "title": p.title, "description": p.description, "placeholder": p.placeholder}), k["TIMEZONE_PANEL"]);
    let fatal: serde_json::Map<String, Value> =
        fatal_gateway::FATAL_CLOSE_CODES.iter().map(|c| (c.code.to_string(), json!({"name": c.name, "fix": c.fix}))).collect();
    assert_eq!(Value::Object(fatal), k["FATAL_CLOSE_CODES"]);
    assert_eq!(json!(fatal_gateway::FATAL_MESSAGE_KEY), k["FATAL_MESSAGE_KEY"]);
    assert_eq!(json!(bot_util::parse_duration::MAX_DURATION_MS), k["MAX_DURATION_MS"]);
    assert_eq!(json!(bot_util::image_size::DEFAULT_MAX_SIDE), k["DEFAULT_MAX_SIDE"]);
    assert_eq!(json!(bot_util::image_size::DEFAULT_MAX_PIXELS), k["DEFAULT_MAX_PIXELS"]);
    assert_eq!(json!(bot_util::calendar::LINK_URL_MAX), k["LINK_URL_MAX"]);
    assert_eq!(json!(safe_regex::MAX_PATTERN_LENGTH), k["MAX_PATTERN_LENGTH"]);
    assert_eq!(json!(safe_regex::DEFAULT_INPUT_CAP), k["DEFAULT_INPUT_CAP"]);
}

#[test]
fn placeholder_vars_that_fail_are_logged_and_render_empty() {
    let lines = capture_logs();
    let ctx = ph::RenderContext {
        vars: Some(vec![("bad".into(), ph::VarValue::Lazy(Box::new(|| Err("nope".into())))), ("n".into(), 3.into())]),
        ..Default::default()
    };
    let out = ph::render_placeholders("[{bad}] {n}", &ctx, None);
    let l = lines.lines();
    drop(lines);
    assert_eq!(out, "[] 3");
    assert!(l.iter().any(|x| x.contains("Placeholder var {bad} threw: nope")));
}

#[test]
fn random_placeholders_stay_in_range() {
    let ctx = ph::RenderContext::default();
    for _ in 0..200 {
        let n: i64 = ph::render_placeholders("{random}", &ctx, None).parse().unwrap();
        assert!((0..=10).contains(&n));
        let m: i64 = ph::render_placeholders("{random:5:1}", &ctx, None).parse().unwrap();
        assert!((1..=5).contains(&m));
        let big: i64 = ph::render_placeholders("{random:-9999999999:9999999999}", &ctx, None).parse().unwrap();
        assert!((-1_000_000_000..=1_000_000_000).contains(&big));
    }
}
