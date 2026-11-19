//! Additions for the second wave: a throttle on any clock, scoped log capture, V8's legacy `Date.parse`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use bot_util::async_util::Throttle;
use bot_util::logger::{self, Level};

#[test]
fn throttle_on_a_clock() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let n = now.clone();
    let th = Throttle::with_clock(500, Arc::new(move || n.load(Ordering::SeqCst)));
    assert!(th.allow("a"));
    assert!(!th.allow("a"));
    now.fetch_add(499, Ordering::SeqCst);
    assert!(!th.allow("a"));
    now.fetch_add(1, Ordering::SeqCst);
    assert!(th.allow("a"));
    // Explicit times.
    let plain = Throttle::new(100);
    assert!(plain.allow_at("k", 10));
    assert!(!plain.allow_at("k", 109));
    assert!(plain.allow_at("k", 110));
}

#[tokio::test]
async fn capture_is_scoped_to_the_task() {
    let (out, lines) = logger::capture(async {
        bot_util::log_warn!("inside", 1);
        // Another task's lines are not captured here.
        tokio::spawn(async { bot_util::log_info!("elsewhere") }).await.unwrap();
        7
    })
    .await;
    assert_eq!(out, 7);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, Level::Warn);
    assert!(lines[0].1.ends_with("inside 1"), "{}", lines[0].1);
    let ((), sync_lines) = logger::capture_sync(|| bot_util::log_error!("sync line"));
    assert_eq!(sync_lines.len(), 1);
    assert!(sync_lines[0].1.contains("sync line"));
}

#[test]
fn date_parse_matches_v8() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/feat-feeds/parsers.json");
    let g: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let dates = g["dates"].as_object().unwrap();
    assert!(dates.len() > 30);
    for (s, expected) in dates {
        for (which, t) in [("v8_date", bot_util::v8_date::date_parse(s)), ("js", bot_util::js::date_parse(s))] {
            let ours = if t.is_finite() { serde_json::json!(t as i64) } else { serde_json::Value::Null };
            assert_eq!(&ours, expected, "{which}::date_parse({s:?})");
        }
    }
    assert_eq!(bot_util::v8_date::parse_date("Fri, 09 Oct 2026 10:00:00 EST").map(|d| d.timestamp()), Some(1_791_558_000));
}

/// `Intl.DateTimeFormat` throws a RangeError for these (an invalid zone); Rust must say "invalid", never panic on a
/// multi-byte character inside an offset id.
#[test]
fn offset_ids_with_non_ascii_digits_are_invalid_not_a_panic() {
    for tz in ["+1é1", "-é12", "+0é", "+é"] {
        assert!(!bot_util::time::is_valid_time_zone(tz), "{tz}");
    }
    assert!(bot_util::time::is_valid_time_zone("+05:30"));
    assert!(bot_util::time::is_valid_time_zone("\u{2212}0500"));
}
