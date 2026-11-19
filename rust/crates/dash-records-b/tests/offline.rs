//! Offline tests: the registration (every key, the actions of each collection), the refusals a store answers before it
//! reads the database, and the small helpers ported from the JS stores.

use bot_core::testing::TestApp;
use dash_collections::{Collections, RecordErrorCode, RecordStore, StoreCtx, StoreError, StoreRegistry};
use dash_records_b::common::{custom_emoji_id, js_slice, locale_int, sentence};
use serde_json::{Map, Value, json};

fn record(e: StoreError) -> (RecordErrorCode, String, Option<String>) {
    match e {
        StoreError::Record(r) => (r.code, r.message, r.path),
        other => panic!("expected a RecordError, got {other:?}"),
    }
}

#[tokio::test]
async fn register_fills_exactly_its_eleven_collections() {
    let t = TestApp::new();
    let mut reg = StoreRegistry::new();
    dash_records_b::register(&mut reg, &t).expect("every key is a collection");
    let mut expected: Vec<String> = Collections::builtin().keys().filter(|k| !dash_records_b::KEYS.contains(k)).map(str::to_string).collect();
    expected.sort();
    let mut missing = reg.missing();
    missing.sort();
    assert_eq!(missing, expected, "the other crate serves exactly the rest");
    assert_eq!(missing.len(), Collections::builtin().len() - 11);
    for key in dash_records_b::KEYS {
        assert!(reg.store(key).is_ok(), "{key}");
    }
}

#[tokio::test]
async fn register_refuses_a_registry_without_the_collections() {
    let t = TestApp::new();
    let mut reg = StoreRegistry::with_collections(Collections::new(Vec::new()));
    assert_eq!(dash_records_b::register(&mut reg, &t), Err("reactionRoles is not a collection".to_string()));
}

#[tokio::test]
async fn every_store_offers_the_actions_its_collection_declares() {
    let t = TestApp::new();
    for (key, store) in dash_records_b::stores(&t) {
        let c = Collections::builtin().get(key).expect("collection");
        let mut declared: Vec<&str> = c.actions.iter().map(|a| a.raw().get("id").and_then(Value::as_str).unwrap_or("")).collect();
        declared.sort_unstable();
        let mut offered = store.action_ids().to_vec();
        offered.sort_unstable();
        assert_eq!(offered, declared, "{key}");
    }
}

fn ctx(guild: &str) -> StoreCtx {
    StoreCtx { guild_id: guild.into(), user_id: "955017000000000010".into(), ..Default::default() }
}

fn store(t: &TestApp, key: &str) -> std::sync::Arc<dyn RecordStore> {
    dash_records_b::stores(t).into_iter().find(|(k, _)| *k == key).map(|(_, s)| s).expect("store")
}

#[tokio::test]
async fn translate_stats_is_read_only() {
    let t = TestApp::new();
    let s = store(&t, "translateStats");
    let c = ctx("1");
    let text = "Translation activity is counted by the bot and cannot be changed here.";
    for e in [
        s.create("1", Map::new(), &c).await.expect_err("create"),
        s.update("1", "2", Map::new(), &c).await.expect_err("update"),
    ] {
        assert_eq!(record(e), (RecordErrorCode::Invalid, text.into(), None));
    }
    assert_eq!(record(s.remove("1", "2", &c).await.expect_err("remove")), (RecordErrorCode::Invalid, text.into(), None));
}

#[tokio::test]
async fn panels_need_the_task_runner() {
    let t = TestApp::new();
    let c = ctx("1");
    let rr = store(&t, "reactionRoles");
    let msg = "Panels cannot be posted from the dashboard right now.";
    assert_eq!(record(rr.create("1", Map::new(), &c).await.expect_err("create")), (RecordErrorCode::Unavailable, msg.into(), None));
    assert_eq!(record(rr.update("1", "x", Map::new(), &c).await.expect_err("update")), (RecordErrorCode::Unavailable, msg.into(), None));
    assert_eq!(record(rr.action("1", "x", "repost", Map::new(), &c).await.expect_err("repost")), (RecordErrorCode::Unavailable, msg.into(), None));
    let tp = store(&t, "ticketPanels");
    let msg = "Panels cannot be published from the dashboard right now.";
    assert_eq!(record(tp.update("1", "x", Map::new(), &c).await.expect_err("update")), (RecordErrorCode::Unavailable, msg.into(), None));
    let mut data = Map::new();
    data.insert("channelId".into(), json!("955017000000000021"));
    assert_eq!(record(tp.create("1", data, &c).await.expect_err("create")), (RecordErrorCode::Unavailable, msg.into(), None));
    for action in ["publish", "refresh"] {
        assert_eq!(record(tp.action("1", "x", action, Map::new(), &c).await.expect_err(action)), (RecordErrorCode::Unavailable, msg.into(), None));
    }
}

#[tokio::test]
async fn the_reminder_test_needs_a_discord_client() {
    let t = TestApp::new();
    let s = store(&t, "reminders");
    let e = s.action("1", "x", "test", Map::new(), &ctx("1")).await.expect_err("test");
    assert_eq!(record(e), (RecordErrorCode::Unavailable, "Posting messages from the dashboard is not available right now.".into(), None));
}

#[tokio::test]
async fn unknown_actions_are_not_found() {
    let t = TestApp::new();
    for (key, s) in dash_records_b::stores(&t) {
        let e = s.action("1", "x", "nope", Map::new(), &ctx("1")).await.expect_err(key);
        assert_eq!(record(e), (RecordErrorCode::NotFound, "Unknown action.".into(), None), "{key}");
    }
}

/// reminders.store.js scheduleError: option names become labels; the first matching rule names the field.
#[test]
fn schedule_errors_name_the_field() {
    let cases = [
        (
            "With a daily window the interval must be under 24 hours. For daily or weekly reminders use the Time of day mode.",
            "With a daily window the interval must be under 24 hours. For daily or weekly reminders use the Time of day mode.",
            Some("intervalMinutes"),
        ),
        ("Time mode needs `time`, e.g. `09:00`.", "Time mode needs time of day, e.g. 09:00.", Some("timeOfDay")),
        ("Once mode needs `at`, e.g. `18:00` or `2h`.", "Once mode needs date and time, e.g. 18:00 or 2h.", Some("runAt")),
        ("The window start and end can't be the same time.", "The window start and end can't be the same time.", Some("windowEnd")),
        ("This schedule never fires. Check the weekdays and the time window.", "This schedule never fires. Check the weekdays and the time window.", Some("windowEnd")),
        ("Weekdays can only be used with intervals of 1 day or less.", "Weekdays can only be used with intervals of 1 day or less.", Some("intervalMinutes")),
        ("That time has passed or is less than a minute away. Pick a later `at`.", "That time has passed or is less than a minute away. Pick a later date and time.", Some("runAt")),
        (
            "These options don't apply to **Once** mode: `weekdays`, `window_start`.",
            "These options don't apply to **Once** mode: days, daily start.",
            Some("windowEnd"),
        ),
        ("Pick a `mode`.", "Pick a repeat mode.", None),
        ("I couldn't read those days. Use names like `mon,wed,fri`, or `weekdays`, `weekends`, `all`.", "I couldn't read those days. Use names like mon,wed,fri, or days, weekends, all.", Some("weekdays")),
    ];
    for (message, text, path) in cases {
        let (code, m, p) = record(dash_records_b::reminders_store::schedule_error(message));
        assert_eq!(code, RecordErrorCode::Invalid);
        assert_eq!(m, text, "{message}");
        assert_eq!(p.as_deref(), path, "{message}");
    }
}

#[test]
fn helpers() {
    assert_eq!(locale_int(0), "0");
    assert_eq!(locale_int(999), "999");
    assert_eq!(locale_int(1000), "1,000");
    assert_eq!(locale_int(1_234_567), "1,234,567");
    assert_eq!(locale_int(-1500), "-1,500");
    assert_eq!(js_slice("abc", 2), "ab");
    assert_eq!(js_slice("😀😀", 2), "😀");
    assert_eq!(custom_emoji_id("<:blob:955017000000000040>"), Some("955017000000000040"));
    assert_eq!(custom_emoji_id("<a:blob:12345678901234567>"), Some("12345678901234567"));
    assert_eq!(custom_emoji_id("<:blob:1234>"), None);
    assert_eq!(custom_emoji_id("⭐"), None);
    assert_eq!(sentence("Done"), "Done.");
    assert_eq!(sentence("Done."), "Done.");
}
