//! Port of `test/reminders.test.js` and the reminder part of `test/review-community2.test.js`: the pure helpers against
//! JS-produced fixtures (`rust/fixtures/golden/feat-feeds/service.json`), then (TEST_DATABASE_URL) the rows, the job,
//! the `reminder.fire` handler, the form, the command through the router and both directions of the JS round trip.

mod common;

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeInteraction, database_url, offline_discord_as, run_node};
use bot_db::{Reminder, ReminderMode};
use chrono::{DateTime, Utc};
use common::*;
use feat_feeds::net::{MessageLite, RestError};
use feat_feeds::reminder_service::{self as svc, Schedule, ScheduleInput};
use serde_json::{Value, json};
use serenity::all::Permissions;

fn golden(name: &str) -> Value {
    // Fails loudly on a missing or unreadable fixture (a Null would make every check over it a no-op).
    bot_core::testing::load_golden(&format!("feat-feeds/{name}"))
}

fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc))
}

fn iso(d: Option<DateTime<Utc>>) -> Value {
    d.map(|d| json!(bot_db::format_js_date(&d))).unwrap_or(Value::Null)
}

fn mode_of(v: &Value) -> ReminderMode {
    match v.as_str() {
        Some("INTERVAL") => ReminderMode::Interval,
        Some("ONE_SHOT") => ReminderMode::OneShot,
        _ => ReminderMode::TimeOfDay,
    }
}

/// A Reminder row from the JS `rem(...)` object.
fn rem_of(v: &Value) -> Reminder {
    let now = Utc::now();
    Reminder {
        id: v["id"].as_str().unwrap_or("r1").into(),
        guild_id: v["guildId"].as_str().unwrap_or("g").into(),
        name: v["name"].as_str().unwrap_or("x").into(),
        channel_id: v["channelId"].as_str().unwrap_or("c").into(),
        template: v["template"].clone(),
        mode: mode_of(&v["mode"]),
        weekdays: v["weekdays"].as_array().into_iter().flatten().filter_map(|d| d.as_i64().map(|d| d as i32)).collect(),
        time_of_day: v["timeOfDay"].as_str().map(String::from),
        interval_minutes: v["intervalMinutes"].as_i64().map(|m| m as i32),
        window_start: v["windowStart"].as_str().map(String::from),
        window_end: v["windowEnd"].as_str().map(String::from),
        run_at: date(&v["runAt"]),
        require_activity: v["requireActivity"].as_bool().unwrap_or(false),
        ping_role_id: v["pingRoleId"].as_str().map(String::from),
        last_sent_at: date(&v["lastSentAt"]),
        last_message_id: v["lastMessageId"].as_str().map(String::from),
        enabled: v["enabled"].as_bool().unwrap_or(true),
        created_by: "u".into(),
        created_at: date(&v["createdAt"]).unwrap_or(now),
        updated_at: now,
    }
}

fn fields_json(f: &Schedule) -> Value {
    json!({
        "mode": f.mode.as_str(), "timeOfDay": f.time_of_day, "weekdays": f.weekdays, "intervalMinutes": f.interval_minutes,
        "windowStart": f.window_start, "windowEnd": f.window_end, "runAt": iso(f.run_at), "requireActivity": f.require_activity,
    })
}

// ── Pure helpers against the JS ────────────────────────────────────────────────

#[test]
fn validate_reminder_input_matches_the_js() {
    let g = golden("service.json");
    let now = date(&json!("2026-10-12T10:00:00.000Z")).unwrap_or_default();
    for c in g["validate"].as_array().into_iter().flatten() {
        let i = &c[0];
        let s = |k: &str| i[k].as_str().map(String::from);
        let input = ScheduleInput {
            mode: s("mode"),
            time: s("time"),
            weekdays: s("weekdays"),
            interval: s("interval"),
            window_start: s("windowStart"),
            window_end: s("windowEnd"),
            at: s("at"),
            require_activity: i["requireActivity"].as_bool(),
        };
        let base = if c[1].is_null() { None } else { Some(rem_of(&c[1])) };
        let ours = match svc::validate_reminder_input(&input, "Europe/Berlin", now, base.as_ref()) {
            Ok(f) => json!({ "ok": true, "fields": fields_json(&f) }),
            Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(ours, c[2], "{}", c[0]);
    }
}

#[test]
fn next_run_due_slot_and_descriptions_match_the_js() {
    let g = golden("service.json");
    for c in g["schedules"].as_array().into_iter().flatten() {
        let r = rem_of(&c[0]);
        let s = Schedule::from(&r);
        assert_eq!(svc::describe_schedule(&s), c[1].as_str().unwrap_or(""), "{}", c[0]);
        assert_eq!(json!(svc::late_tolerance_ms(&s)), c[2]);
        for run in c[3].as_array().into_iter().flatten() {
            let tz = run[0].as_str().unwrap_or("UTC");
            let at = date(&run[1]).unwrap_or_default();
            assert_eq!(iso(svc::compute_next_run(&s, tz, at)), run[2], "computeNextRun {} {run}", c[0]);
            assert_eq!(iso(svc::due_slot(&s, tz, at)), run[3], "dueSlot {} {run}", c[0]);
            assert_eq!(iso(svc::job_run_at(&s, tz, at)), run[4], "jobRunAt {} {run}", c[0]);
        }
    }
}

#[test]
fn activity_templates_retries_and_keys_match_the_js() {
    let g = golden("service.json");
    for c in g["activity"].as_array().into_iter().flatten() {
        let s = &c[0];
        let lite = |v: &Value| if v.is_object() { Some(MessageLite { bot: v["bot"].as_bool().unwrap_or(false), system: v["system"].as_bool().unwrap_or(false) }) } else { None };
        let state = svc::ActivityState {
            require_activity: s["requireActivity"].as_bool().unwrap_or(false),
            last_message_id: s["lastMessageId"].as_str().map(String::from),
            channel_last_message_id: s["channelLastMessageId"].as_str().map(String::from),
            recent: s["recent"].as_array().map(|a| a.iter().filter_map(lite).collect()),
            cached_last: lite(&s["cachedLast"]),
        };
        assert_eq!(json!(svc::activity_allows_send(&state)), c[1], "{s}");
    }
    for c in g["emptyTemplate"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::is_empty_template(&c[0])), c[1], "{}", c[0]);
    }
    for c in g["userTokens"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::has_user_tokens(&c[0])), c[1], "{}", c[0]);
    }
    for c in g["mayRetry"].as_array().into_iter().flatten() {
        let r = svc::may_retry(c[0].as_i64().map(|a| a as i32), c[1].as_i64().map(|a| a as i32), c[2].as_i64().unwrap_or(0), c[3].as_i64().unwrap_or(0));
        assert_eq!(json!(r), c[4], "{c}");
    }
    for c in g["retryDelay"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::scheduler_retry_delay_ms(c[0].as_i64().unwrap_or(0) as i32)), c[1]);
    }
    let k = &g["keys"];
    assert_eq!(svc::job_key("clx0rem0001"), k["reminderJobKey"]);
    for c in k["nonce"].as_array().into_iter().flatten() {
        let at = DateTime::from_timestamp_millis(c[1].as_i64().unwrap_or(0)).unwrap_or_default();
        assert_eq!(svc::occurrence_nonce(c[0].as_str().unwrap_or(""), at), c[2]);
    }
    assert_eq!(bot_config::build_custom_id("reminder-modal", &["clx0rem0001"]).ok(), k["reminderModal"].as_str().map(String::from));
    assert_eq!(bot_config::limits::REMINDERS, 100);
    assert_eq!(svc::DEFAULT_CONTENT, "Hey, I'm a reminder!");
    assert_eq!(svc::default_template(), json!({ "content": "Hey, I'm a reminder!", "embed": null, "buttons": [] }));
    assert_eq!(svc::mode_from_choice("once"), Some(ReminderMode::OneShot));
    assert_eq!(svc::resolve_time_zone(Some("Nope/Zone")), "UTC");
    assert_eq!(svc::resolve_time_zone(Some("Europe/London")), "Europe/London");
    assert_eq!(svc::REMINDER_CHANNEL_TYPES, [0, 5]);
}

#[test]
fn transient_send_errors_retry_and_permanent_ones_do_not() {
    let api = |code: u64, status: u16| RestError { code: Some(code), status: Some(status), message: format!("err {code}"), timeout: false };
    assert!(svc::is_transient_send_error(&api(0, 500)));
    assert!(svc::is_transient_send_error(&api(0, 503)));
    assert!(svc::is_transient_send_error(&api(0, 429)));
    assert!(svc::is_transient_send_error(&RestError::timed_out("send")));
    assert!(svc::is_transient_send_error(&RestError { message: "socket hang up".into(), ..Default::default() }));
    for code in [50001, 50013, 10003, 10013, 50007, 50035] {
        assert!(!svc::is_transient_send_error(&api(code, 403)), "code {code}");
    }
    assert!(!svc::is_transient_send_error(&api(0, 404)));
}

fn embed_core(e: &Value) -> Value {
    json!({ "title": e["title"], "description": e["description"], "color": e["color"] })
}

#[tokio::test]
async fn send_payloads_and_summaries_match_the_js() {
    let t = bot_core::testing::TestApp::new().with_guilds(["955140000000000090", "955140000000000091", "955140000000000092"]);
    let g = golden("service.json");
    let colour = t.settings().update_settings("955140000000000090", SettingsPatch::new().set("embedColor", 0x123456)).await.expect("memory store");
    for c in g["sendPayloads"].as_array().into_iter().flatten() {
        let r = rem_of(&c[0]);
        let p = svc::build_send_payload(&t, &r, None, None, Some(&colour), c[1].as_bool().unwrap_or(true));
        let j = &c[2];
        assert_eq!(json!(p.content), j["content"], "{}", c[0]);
        assert_eq!(p.allowed_mentions, j["allowedMentions"]);
        assert_eq!(p.embeds.iter().map(embed_core).collect::<Vec<_>>(), j["embeds"].as_array().into_iter().flatten().map(embed_core).collect::<Vec<_>>());
        assert_eq!(json!(p.components), j["components"]);
    }
    for (i, c) in g["summary"].as_array().into_iter().flatten().enumerate() {
        let r = rem_of(&c[0]);
        let gid = if i % 2 == 0 { "955140000000000091" } else { "955140000000000092" };
        let settings = match c[1].as_object() {
            Some(s) => Some(
                t.settings()
                    .update_settings(
                        gid,
                        SettingsPatch::new()
                            .set("timezone", s.get("timezone").and_then(Value::as_str).unwrap_or("UTC"))
                            .set("remindersEnabled", s.get("remindersEnabled").and_then(Value::as_bool).unwrap_or(true)),
                    )
                    .await
                    .expect("memory store"),
            ),
            None => None,
        };
        let settings = settings.as_deref();
        let warnings: Vec<String> = c[3].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(String::from)).collect();
        assert_eq!(json!(svc::summary_lines(&r, settings, date(&c[2]), &warnings)), c[4], "{}", c[0]);
    }
}

#[test]
fn reminder_payload_options() {
    let json = feat_feeds::commands::reminder::command().payload();
    let names = |n: &str| -> Vec<String> {
        json["options"].as_array().into_iter().flatten().find(|o| o["name"] == n).map(|o| o["options"].as_array().into_iter().flatten().map(|x| x["name"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default()
    };
    assert_eq!(names("create"), ["name", "channel", "mode", "time", "weekdays", "interval", "window_start", "window_end", "at", "require_activity", "ping_role", "message"]);
    assert_eq!(names("edit"), ["name", "channel", "mode", "time", "weekdays", "interval", "window_start", "window_end", "at", "require_activity", "ping_role", "clear_ping_role", "message"]);
    let meta = feat_feeds::commands::reminder::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("reminders"), json!(false), json!(2)));
    let mut reg = bot_core::Registry::new();
    feat_feeds::register(&mut reg);
    let p = reg.plugin_named("reminders").expect("declared");
    assert_eq!(p.hooks.iter().map(|h| (h.event().as_str(), h.id())).collect::<Vec<_>>(), vec![("channelDelete", "reminders"), ("guildReady", "reminders")]);
}

#[path = "reminders/db.rs"]
mod db;
