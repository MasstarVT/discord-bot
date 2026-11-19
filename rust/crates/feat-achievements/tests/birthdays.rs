//! Port of `test/birthdays.test.js` (and the birthday cases of `test/review-community2.test.js`), plus the database
//! and cross-language checks.
//!
//! Offline: date parsing, Feb 29, next occurrence, ages, the hour gate, role markers, tick times, tombstone TTLs,
//! announcement chunking, templates, formatting, a sweep against Node, the plugin and the payload. With
//! `TEST_DATABASE_URL` / `TEST_REDIS_URL`: the claim, saving and removing, the upcoming list, the job, the role
//! transfer, the tick against a fake Discord, `/birthday` through the router, and the JS round trips.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_core::command::option_type as ot;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, database_url, redis_url, run_node};
use chrono::{DateTime, Utc};
use feat_achievements::birthday_service::{self as svc, AnnounceEntry, BdMember, BirthdayDate, BirthdayDiscord, DiscordFail, LocalDate, Marker};
use serde_json::{Value, json};
use serenity::all::Permissions;

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_achievements::register(&mut reg);
    reg
}

fn d(iso: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(iso).map(|d| d.to_utc()).unwrap_or_else(|e| panic!("{iso}: {e}"))
}

fn iso(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const NOW: &str = "2026-10-10T12:00:00Z";

fn ld(year: i64, month: u32, day: u32) -> LocalDate {
    LocalDate { year, month, day }
}

// ── 1. parseBirthdayInput ─────────────────────────────────────────────────────

fn parse(raw: &str, require_year: bool) -> Result<BirthdayDate, svc::ParseFail> {
    svc::parse_birthday_input(raw, require_year, &d(NOW), "UTC")
}

#[test]
fn parse_birthday_input_accepted_forms() {
    assert_eq!(parse("03-14", false), Ok(BirthdayDate { month: 3, day: 14, year: None }));
    assert_eq!(parse(" 3-4 ", false), Ok(BirthdayDate { month: 3, day: 4, year: None }));
    assert_eq!(parse("1998-03-14", false), Ok(BirthdayDate { month: 3, day: 14, year: Some(1998) }));
    assert_eq!(parse("1998-3-4", false), Ok(BirthdayDate { month: 3, day: 4, year: Some(1998) }));
    assert!(parse("02-29", false).is_ok());
    assert!(parse("2000-02-29", false).is_ok());
    assert!(parse("2026-10-10", false).is_ok());
    assert!(parse("1998-03-14", true).is_ok());
}

#[test]
fn parse_birthday_input_rejections_carry_a_code_and_a_message() {
    let code = |raw: &str, req: bool| parse(raw, req).err().map(|f| f.code).unwrap_or("OK");
    let leap = parse("1999-02-29", false).expect_err("refused");
    assert_eq!(leap.code, "INVALID_DATE");
    assert_eq!(leap.message, "**1999-02-29** is not a real date: 1999 is not a leap year.");
    for raw in ["02-30", "13-01", "00-10", "04-31", "1998-00-10"] {
        assert_eq!(code(raw, false), "INVALID_DATE", "{raw}");
    }
    assert_eq!(code("1899-01-01", false), "YEAR_RANGE");
    assert_eq!(parse("2027-01-01", false).err().map(|f| f.message), Some("The year must be between 1900 and 2026.".into()));
    assert_eq!(code("2026-12-01", false), "FUTURE");
    assert_eq!(code("2026-10-11", false), "FUTURE");
    for raw in ["14/03", "", "March 14", "03.14", "1998/03/14", "12345-01-01", "03-14-1998"] {
        assert_eq!(code(raw, false), "FORMAT", "{raw}");
    }
    assert_eq!(code("03-14", true), "YEAR_REQUIRED");
    assert_eq!(code("02-30", true), "INVALID_DATE", "validity is checked before \"required\"");
}

#[test]
fn parse_birthday_input_year_range_follows_the_guild_local_year() {
    let now = d("2026-12-31T23:30:00Z");
    assert!(svc::parse_birthday_input("2027-01-01", false, &now, "Pacific/Kiritimati").is_ok());
    assert_eq!(svc::parse_birthday_input("2027-01-01", false, &now, "UTC").err().map(|f| f.code), Some("YEAR_RANGE"));
    assert_eq!(svc::parse_birthday_input("2027-01-02", false, &now, "Pacific/Kiritimati").err().map(|f| f.code), Some("FUTURE"));
}

// ── 2-5. Feb 29, nextOccurrence, ageOn, isEventEligible ───────────────────────

#[test]
fn feb_29_is_celebrated_on_feb_28_in_non_leap_years() {
    assert_eq!(svc::effective_day(2, 29, 2027), 28);
    assert_eq!(svc::effective_day(2, 29, 2028), 29);
    assert_eq!(svc::effective_day(3, 29, 2027), 29);
    assert!(svc::is_leap_year(2028));
    assert!(!svc::is_leap_year(2100));
    assert!(svc::is_birthday_on(2, 29, ld(2027, 2, 28)));
    assert!(!svc::is_birthday_on(2, 29, ld(2028, 2, 28)));
    assert!(svc::is_birthday_on(2, 29, ld(2028, 2, 29)));
    assert!(!svc::is_birthday_on(2, 29, ld(2027, 3, 1)));
    assert_eq!(svc::today_matchers(ld(2027, 2, 28)), vec![(2, 28), (2, 29)]);
    assert_eq!(svc::today_matchers(ld(2028, 2, 28)), vec![(2, 28)]);
    assert_eq!(svc::today_matchers(ld(2028, 2, 29)), vec![(2, 29)]);
    assert_eq!(svc::today_matchers(ld(2027, 10, 10)), vec![(10, 10)]);
}

#[test]
fn next_occurrence_age_and_eligibility() {
    let local = ld(2026, 10, 10);
    assert_eq!(svc::next_occurrence(3, 14, local), ld(2027, 3, 14));
    assert_eq!(svc::next_occurrence(10, 10, local), ld(2026, 10, 10));
    assert_eq!(svc::next_occurrence(12, 31, local), ld(2026, 12, 31));
    assert_eq!(svc::next_occurrence(10, 9, local), ld(2027, 10, 9));
    assert_eq!(svc::next_occurrence(2, 29, ld(2027, 3, 1)), ld(2028, 2, 29));
    assert_eq!(svc::next_occurrence(2, 29, ld(2026, 3, 1)), ld(2027, 2, 28));
    assert_eq!(svc::next_occurrence(2, 29, ld(2027, 2, 28)), ld(2027, 2, 28));
    assert_eq!(svc::age_on(Some(2000), 2026), Some(26));
    assert_eq!(svc::age_on(None, 2026), None);
    assert_eq!(svc::age_on(Some(2026), 2026), None);
    assert_eq!(svc::age_on(Some(2027), 2026), None);
    let e = |last: Option<i64>, hour: i64, cfg: f64| svc::is_event_eligible(10, 10, last, local, hour, Some(cfg));
    assert!(!e(None, 8, 9.0));
    assert!(e(None, 9, 9.0));
    assert!(e(None, 17, 9.0), "catch-up later the same day");
    assert!(e(Some(2025), 9, 9.0));
    assert!(!e(Some(2026), 9, 9.0));
    assert!(!svc::is_event_eligible(10, 11, None, local, 23, Some(9.0)));
    assert!(!e(None, 0, 99.0), "an invalid hour falls back to 9");
}

// ── 6. partitionMarkers ───────────────────────────────────────────────────────

fn marker(id: &str, iso_at: &str, month: u32, day: u32) -> Marker {
    Marker { id: id.into(), user_id: "u".into(), month, day, role_granted_at: d(iso_at) }
}

#[test]
fn partition_markers_keeps_only_todays_marker_of_an_enabled_module_with_a_role() {
    let local = ld(2026, 10, 10);
    let m = marker("m", "2026-10-10T09:00:00Z", 10, 10);
    assert_eq!(svc::partition_markers(std::slice::from_ref(&m), true, Some("r"), local, "UTC"), (vec![m.clone()], vec![]));
    assert_eq!(svc::partition_markers(std::slice::from_ref(&m), false, Some("r"), local, "UTC"), (vec![], vec![m.clone()]));
    assert_eq!(svc::partition_markers(std::slice::from_ref(&m), true, None, local, "UTC"), (vec![], vec![m.clone()]));
    let yesterday = marker("y", "2026-10-09T09:00:00Z", 10, 9);
    assert_eq!(svc::partition_markers(std::slice::from_ref(&yesterday), true, Some("r"), local, "UTC").1.len(), 1);
    let stale = marker("s", "2025-10-10T09:00:00Z", 10, 10);
    assert_eq!(svc::partition_markers(std::slice::from_ref(&stale), true, Some("r"), local, "UTC").1.len(), 1, "a marker from last year is stale");
    let edge = marker("e", "2026-10-09T23:30:00Z", 10, 10);
    assert_eq!(svc::partition_markers(std::slice::from_ref(&edge), true, Some("r"), local, "Pacific/Kiritimati").0.len(), 1);
    assert_eq!(svc::partition_markers(std::slice::from_ref(&edge), true, Some("r"), local, "UTC").0.len(), 0);
    let leapling = marker("l", "2027-02-28T09:00:00Z", 2, 29);
    assert_eq!(svc::partition_markers(&[leapling], true, Some("r"), ld(2027, 2, 28), "UTC").0.len(), 1);
}

// ── 7/8. nextTickAt, tombstoneTtlSeconds ──────────────────────────────────────

#[test]
fn next_tick_at_is_just_after_the_next_local_hour_boundary() {
    let at = |now: &str, tz: &str| iso(&svc::next_tick_at(&d(now), tz));
    assert_eq!(at("2026-10-10T10:20:00Z", "UTC"), "2026-10-10T11:00:02.000Z");
    assert_eq!(at("2026-10-10T10:00:01Z", "UTC"), "2026-10-10T11:00:02.000Z");
    assert_eq!(at("2026-10-10T10:20:00Z", "Asia/Kolkata"), "2026-10-10T10:30:02.000Z");
    assert_eq!(at("2026-10-25T01:30:00Z", "Europe/London"), "2026-10-25T02:00:02.000Z");
    assert_eq!(at("2026-03-29T00:30:00Z", "Europe/London"), "2026-03-29T01:00:02.000Z");
    assert_eq!(at("2026-10-10T10:20:00Z", "Not/AZone"), "2026-10-10T11:00:02.000Z");
    for tz in ["UTC", "Asia/Kolkata", "Asia/Kathmandu", "Europe/London", "America/New_York", "Pacific/Kiritimati", "Australia/Lord_Howe"] {
        let mut t = d("2026-03-28T00:00:00Z").timestamp_millis();
        let end = d("2026-04-01T00:00:00Z").timestamp_millis();
        while t < end {
            let now = DateTime::from_timestamp_millis(t).expect("date");
            let next = svc::next_tick_at(&now, tz).timestamp_millis();
            assert!(next > t, "{tz} {}: next must be after now", iso(&now));
            assert!(next <= t + 3_600_000 + 2_000, "{tz} {}: within one hour and 2 s", iso(&now));
            t += 17 * 60_000 + 13_000;
        }
    }
}

#[test]
fn tombstone_ttl_lasts_until_the_next_local_new_year_plus_a_day() {
    assert_eq!(svc::tombstone_ttl_seconds(&d("2026-12-31T23:00:00Z"), "UTC"), 3600 + 86_400);
    assert_eq!(svc::tombstone_ttl_seconds(&d("2026-01-01T00:00:00Z"), "UTC"), 365 * 86_400 + 86_400);
    for tz in ["UTC", "Pacific/Kiritimati", "America/New_York", "Nope"] {
        assert!(svc::tombstone_ttl_seconds(&d(NOW), tz) >= 60);
        assert!(svc::tombstone_ttl_seconds(&d("2026-12-31T23:59:59Z"), tz) >= 60);
    }
}

// ── 9. chunkAnnouncements ─────────────────────────────────────────────────────

fn entry(id: &str, user: &str, text: &str) -> AnnounceEntry {
    AnnounceEntry { id: id.into(), user_id: user.into(), text: text.into() }
}

#[test]
fn chunk_announcements_size_and_mention_caps_nothing_lost() {
    let entries: Vec<AnnounceEntry> = (0..120).map(|i| entry(&format!("id{i}"), &format!("u{i}"), &format!("{i:03} {}", "x".repeat(36)))).collect();
    let chunks = svc::chunk_announcements(&entries, 2000, 50);
    assert!(chunks.len() >= 3);
    assert!(chunks.iter().all(|c| c.content.len() <= 2000 && c.user_ids.len() <= 50));
    assert_eq!(chunks.iter().map(|c| c.content.clone()).collect::<Vec<_>>().join("\n"), entries.iter().map(|e| e.text.clone()).collect::<Vec<_>>().join("\n"));
    let short: Vec<AnnounceEntry> = (0..120).map(|i| entry(&format!("id{i}"), &format!("u{i}"), "hi")).collect();
    assert_eq!(svc::chunk_announcements(&short, 2000, 50).iter().map(|c| c.ids.len()).collect::<Vec<_>>(), vec![50, 50, 20]);
    let big = "y".repeat(2000);
    let chunks = svc::chunk_announcements(&[entry("a", "u1", "one"), entry("b", "u2", &big), entry("c", "u3", "three")], 2000, 50);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[1].content, big);
    let dup = svc::chunk_announcements(&[entry("a", "u1", "one"), entry("b", "u1", "two")], 2000, 50);
    assert_eq!((dup.len(), dup[0].user_ids.clone(), dup[0].ids.clone()), (1, vec!["u1".to_string()], vec!["a".to_string(), "b".to_string()]));
    assert!(svc::chunk_announcements(&[], 2000, 50).is_empty());
}

// ── 10/11. Templates and formatting ───────────────────────────────────────────

fn line_ctx() -> bot_util::placeholder_parser::RenderContext {
    use bot_util::placeholder_parser::*;
    let user = PlaceholderUser { id: "1".into(), username: Some("a".into()), ..Default::default() };
    RenderContext {
        member: Some(PlaceholderMember { display_name: Some("A".into()), user: Some(user.clone()), ..Default::default() }),
        user: Some(user),
        guild: Some(PlaceholderGuild { id: "g".into(), name: Some("Guild".into()), owner_id: Some("9".into()), member_count: Some(3.0), ..Default::default() }),
        channel: Some(PlaceholderChannel { id: "c".into(), name: Some("general".into()), kind: Some(0) }),
        vars: None,
        args: None,
    }
}

#[test]
fn pick_template_and_render_birthday_line() {
    assert_eq!(svc::pick_template(None, None), svc::DEFAULT_BIRTHDAY_MESSAGE);
    assert_eq!(svc::pick_template(None, Some(5)), svc::DEFAULT_BIRTHDAY_MESSAGE_AGE);
    let line = |tpl: &str, age| svc::render_birthday_line(tpl, line_ctx(), age);
    assert_eq!(line(svc::DEFAULT_BIRTHDAY_MESSAGE_AGE, Some(26)), "Happy birthday <@1>! They are now 26! 🎂");
    assert_eq!(line(svc::DEFAULT_BIRTHDAY_MESSAGE, None), "Happy birthday <@1>! 🎂");
    assert_eq!(line("age=[{age}]", None), "age=[]");
    assert_eq!(line("{server}: {user.name} in {channel.name}", None), "Guild: a in general");
    assert_eq!(line("   ", None), "Happy birthday <@1>! 🎂");
    assert_eq!(line("   ", Some(4)), "Happy birthday <@1>! They are now 4! 🎂");
    assert_eq!(bot_util::js::utf16_len(&line(&"z".repeat(5000), None)), 2000, "each rendered line is clamped to 2000");
}

#[test]
fn formatting_helpers() {
    assert_eq!(svc::format_birthday(3, 14, None, false, false), "March 14");
    assert_eq!(svc::format_birthday(3, 14, None, true, false), "Mar 14");
    assert_eq!(svc::format_birthday(3, 14, Some(1998), false, true), "March 14, 1998");
    assert_eq!(svc::format_birthday(3, 14, Some(1998), false, false), "March 14");
    assert_eq!(svc::format_birthday(3, 14, None, false, true), "March 14");
    assert_eq!(svc::format_birthday(12, 1, None, true, false), "Dec 1");
    assert_eq!(svc::format_hour(Some(9.0)), "09:00");
    assert_eq!(svc::format_hour(Some(24.0)), "09:00");
    assert_eq!(svc::format_hour(Some(0.0)), "00:00");
    for (h, want) in [(Some(24.0), 9), (Some(0.0), 0), (Some(23.0), 23), (Some(-1.0), 9), (Some(1.5), 9), (None, 9)] {
        assert_eq!(svc::clamp_hour(h), want);
    }
    for text in ["a```b", "``````", "````x`", "`", "```\n@everyone"] {
        let out = svc::code_block(text, 1000);
        assert!(out.starts_with("```\n") && out.ends_with("\n```"));
        assert!(!out[4..out.len() - 4].contains("```"), "{text:?}");
    }
    assert!(bot_util::js::utf16_len(&svc::code_block(&"x".repeat(5000), 1000)) <= 1008);
    assert_eq!(svc::tick_key("1"), "birthday.tick:1");
    assert_eq!(svc::tombstone_key("1", "2"), "birthday:announced:1:2");
}

#[tokio::test]
async fn resolve_time_zone_reads_the_settings() {
    let g = "961000000000000901";
    let t = TestApp::new().with_guilds([g]);
    let s = t.settings().update_settings(g, SettingsPatch::new().set("timezone", "Europe/Paris")).await.expect("memory store");
    assert_eq!(svc::resolve_time_zone(Some(&s)), "Europe/Paris");
    let s = t.settings().update_settings(g, SettingsPatch::new().set("timezone", "nope")).await.expect("memory store");
    assert_eq!(svc::resolve_time_zone(Some(&s)), "UTC");
    assert_eq!(svc::resolve_time_zone(None), "UTC");
    assert_eq!(svc::pick_template(Some(&s), Some(7)), svc::DEFAULT_BIRTHDAY_MESSAGE_AGE);
    let s = t.settings().update_settings(g, SettingsPatch::new().set("birthdayMessage", "plain").set("birthdayMessageAge", "x {age}")).await.expect("memory store");
    assert_eq!(svc::pick_template(Some(&s), None), "plain");
    assert_eq!(svc::pick_template(Some(&s), Some(30)), "x {age}");
}

// ── 12. Plugin and payload ────────────────────────────────────────────────────

#[test]
fn plugin_and_payload() {
    let reg = registry();
    bot_core::testing::assert_plugin_matches_golden(&reg, "birthdays");
    bot_core::testing::assert_commands_match_golden(&reg, &["achievement-config", "achievements", "birthday"]);
    let p = reg.plugin_named("birthdays").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Birthdays));
    let meta = feat_achievements::commands::birthday::command().metadata();
    assert_eq!(meta["gated"], false);
    assert_eq!(meta["permissionLevel"], 0);
    let subs = meta["subcommandPermissions"].as_object().cloned().unwrap_or_default();
    assert_eq!(subs.len(), 10);
    assert!(subs.values().all(|v| v == 2));
    let payload = feat_achievements::commands::birthday::command().payload();
    assert!(payload.get("default_member_permissions").is_none() || payload["default_member_permissions"].is_null());
    assert_eq!(payload["contexts"], json!([0]));
}

#[test]
fn raw_sql_has_no_bare_now() {
    for (name, sql) in svc::raw_sql() {
        assert!(!sql.replace(bot_db::SQL_NOW, "").to_lowercase().contains("now()"), "{name}");
    }
    assert!(svc::claim_sql(true).contains("\"roleGrantedAt\""));
    assert!(!svc::claim_sql(false).contains("\"roleGrantedAt\""));
}

// ── Sweep against the JS ──────────────────────────────────────────────────────

fn rust_pure(f: &str, a: &Value) -> Value {
    let s = |i: usize| a[i].as_str().unwrap_or_default().to_string();
    match f {
        "parseBirthdayInput" => match svc::parse_birthday_input(&s(0), a[1].as_bool().unwrap_or(false), &d(&s(2)), &s(3)) {
            Ok(r) => json!({ "ok": true, "month": r.month, "day": r.day, "year": r.year }),
            Err(e) => json!({ "ok": false, "code": e.code, "message": e.message }),
        },
        "nextTickAt" => json!(iso(&svc::next_tick_at(&d(&s(0)), &s(1)))),
        "tombstoneTtlSeconds" => json!(svc::tombstone_ttl_seconds(&d(&s(0)), &s(1))),
        "partitionMarkers" => {
            let markers: Vec<Marker> = a[0]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| Marker {
                    id: m["id"].as_str().unwrap_or("").into(),
                    user_id: "u".into(),
                    month: m["month"].as_u64().unwrap_or(0) as u32,
                    day: m["day"].as_u64().unwrap_or(0) as u32,
                    role_granted_at: d(m["roleGrantedAt"].as_str().unwrap_or("")),
                })
                .collect();
            let o = &a[1];
            let l = &o["local"];
            let (keep, expired) = svc::partition_markers(
                &markers,
                o["enabled"].as_bool().unwrap_or(false),
                o["roleId"].as_str(),
                ld(l["year"].as_i64().unwrap_or(0), l["month"].as_u64().unwrap_or(0) as u32, l["day"].as_u64().unwrap_or(0) as u32),
                o["tz"].as_str().unwrap_or("UTC"),
            );
            json!({ "keep": keep.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), "expired": expired.iter().map(|m| m.id.clone()).collect::<Vec<_>>() })
        }
        "chunkAnnouncements" => {
            let entries: Vec<AnnounceEntry> =
                a[0].as_array().into_iter().flatten().map(|e| entry(e["id"].as_str().unwrap_or(""), e["userId"].as_str().unwrap_or(""), e["text"].as_str().unwrap_or(""))).collect();
            Value::Array(svc::chunk_announcements(&entries, 2000, 50).into_iter().map(|c| json!({ "content": c.content, "userIds": c.user_ids, "ids": c.ids })).collect())
        }
        "codeBlock" => json!(svc::code_block(&s(0), 1000)),
        "formatBirthday" => {
            let b = &a[0];
            json!(svc::format_birthday(
                b["month"].as_u64().unwrap_or(0) as u32,
                b["day"].as_u64().unwrap_or(0) as u32,
                b["year"].as_i64(),
                a[1]["short"].as_bool().unwrap_or(false),
                a[1]["withYear"].as_bool().unwrap_or(false)
            ))
        }
        "nextOccurrence" => {
            let (r, l) = (&a[0], &a[1]);
            let o = svc::next_occurrence(r["month"].as_u64().unwrap_or(0) as u32, r["day"].as_u64().unwrap_or(0) as u32, ld(l["year"].as_i64().unwrap_or(0), l["month"].as_u64().unwrap_or(0) as u32, l["day"].as_u64().unwrap_or(0) as u32));
            json!({ "year": o.year, "month": o.month, "day": o.day })
        }
        other => panic!("no Rust twin for {other}"),
    }
}

#[test]
fn every_pure_helper_agrees_with_the_js() {
    let mut cases: Vec<Value> = Vec::new();
    let mut add = |f: &str, args: Value| cases.push(json!({ "fn": f, "args": args }));
    let raws = [
        "03-14", " 3-4 ", "1998-03-14", "1998-3-4", "02-29", "2000-02-29", "1999-02-29", "2026-10-10", "2026-10-11", "2026-12-01", "02-30", "13-01", "00-10", "04-31",
        "1998-00-10", "1899-01-01", "2027-01-01", "14/03", "", "March 14", "03.14", "12345-01-01", "03-14-1998", "1-1", "001-01", "2024-2-29", "١٢-٠١", "9-31", "12-31",
    ];
    for raw in raws {
        for (req, now, tz) in [(false, "2026-10-10T12:00:00.000Z", "UTC"), (true, "2026-12-31T23:30:00.000Z", "Pacific/Kiritimati"), (false, "2026-01-01T03:00:00.000Z", "America/New_York")] {
            add("parseBirthdayInput", json!([raw, req, now, tz]));
        }
    }
    for tz in ["UTC", "Asia/Kolkata", "Asia/Kathmandu", "Europe/London", "America/New_York", "Pacific/Kiritimati", "Australia/Lord_Howe", "Nope", ""] {
        for now in ["2026-10-10T10:20:00.000Z", "2026-10-25T01:30:00.000Z", "2026-03-29T00:30:00.000Z", "2026-11-01T05:59:59.999Z", "2026-04-05T15:45:00.000Z", "2026-12-31T23:59:59.000Z"] {
            add("nextTickAt", json!([now, tz]));
            add("tombstoneTtlSeconds", json!([now, tz]));
        }
    }
    let markers = json!([
        { "id": "a", "month": 10, "day": 10, "roleGrantedAt": "2026-10-10T09:00:00.000Z" },
        { "id": "b", "month": 10, "day": 9, "roleGrantedAt": "2026-10-09T09:00:00.000Z" },
        { "id": "c", "month": 10, "day": 10, "roleGrantedAt": "2026-10-09T23:30:00.000Z" },
        { "id": "d", "month": 2, "day": 29, "roleGrantedAt": "2027-02-28T09:00:00.000Z" },
    ]);
    for (enabled, role, local, tz) in [
        (true, json!("r"), json!({ "year": 2026, "month": 10, "day": 10 }), "UTC"),
        (true, json!("r"), json!({ "year": 2026, "month": 10, "day": 10 }), "Pacific/Kiritimati"),
        (false, json!("r"), json!({ "year": 2026, "month": 10, "day": 10 }), "UTC"),
        (true, json!(null), json!({ "year": 2026, "month": 10, "day": 10 }), "UTC"),
        (true, json!("r"), json!({ "year": 2027, "month": 2, "day": 28 }), "UTC"),
    ] {
        add("partitionMarkers", json!([markers, { "enabled": enabled, "roleId": role, "local": local, "tz": tz }]));
    }
    let entries: Vec<Value> = (0..75).map(|i| json!({ "id": format!("id{i}"), "userId": format!("u{}", i % 60), "text": "w".repeat((i * 37) % 300) })).collect();
    add("chunkAnnouncements", json!([entries]));
    add("chunkAnnouncements", json!([[{ "id": "a", "userId": "u", "text": "😀".repeat(1500) }, { "id": "b", "userId": "v", "text": "x" }]]));
    for t in ["a```b", "``````", "x".repeat(1200).as_str(), "`".repeat(600).as_str(), "plain", "😀`"] {
        add("codeBlock", json!([t]));
    }
    for (m, dd, y) in [(3, 14, json!(1998)), (12, 1, json!(null)), (13, 1, json!(null)), (5, 9, json!(0))] {
        for (short, with_year) in [(false, false), (true, false), (false, true)] {
            add("formatBirthday", json!([{ "month": m, "day": dd, "year": y }, { "short": short, "withYear": with_year }]));
        }
    }
    for (row, local) in [(json!({ "month": 2, "day": 29 }), json!({ "year": 2027, "month": 3, "day": 1 })), (json!({ "month": 1, "day": 1 }), json!({ "year": 2026, "month": 12, "day": 31 }))] {
        add("nextOccurrence", json!([row, local]));
    }
    let path = std::env::temp_dir().join(format!("feat-achievements-bd-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string(&cases).unwrap_or_default()).expect("temp file");
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["pure-file", &path.to_string_lossy()]);
    let _ = std::fs::remove_file(&path);
    let js = js.as_array().cloned().unwrap_or_else(|| panic!("the JS answered {js}"));
    assert_eq!(js.len(), cases.len());
    for (case, expected) in cases.iter().zip(js) {
        assert_eq!(rust_pure(case["fn"].as_str().unwrap_or(""), &case["args"]), expected, "{case}");
    }
}

// ── A fake Discord ────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeDiscord {
    unavailable: bool,
    members: Mutex<HashMap<String, BdMember>>,
    /// Channel id -> usable.
    channels: HashMap<String, bool>,
    /// Role id -> (assignable, assignable when dangerous roles are allowed).
    roles: HashMap<String, (bool, bool)>,
    sent: Mutex<Vec<(String, String, Vec<String>)>>,
    ops: Mutex<Vec<(String, String, String)>>,
    fail_send: Mutex<Option<u64>>,
    fail_add: Mutex<Option<u64>>,
    /// Delays the member fetch (to overlap two runs).
    slow_ms: u64,
}

impl FakeDiscord {
    fn member(self, id: &str, roles: &[&str]) -> Self {
        self.members.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.into(),
            BdMember {
                id: id.into(),
                roles: roles.iter().map(|r| r.to_string()).collect(),
                ctx_user: Some(bot_util::placeholder_parser::PlaceholderUser { id: id.into(), username: Some(format!("user{id}")), ..Default::default() }),
                ..Default::default()
            },
        );
        self
    }
    fn bot(self, id: &str) -> Self {
        let s = self.member(id, &[]);
        if let Some(m) = s.members.lock().unwrap_or_else(|e| e.into_inner()).get_mut(id) {
            m.bot = true;
        }
        s
    }
    fn channel(mut self, id: &str, usable: bool) -> Self {
        self.channels.insert(id.into(), usable);
        self
    }
    fn role(mut self, id: &str, grant: bool, remove: bool) -> Self {
        self.roles.insert(id.into(), (grant, remove));
        self
    }
    fn sent(&self) -> Vec<(String, String, Vec<String>)> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn ops(&self) -> Vec<(String, String, String)> {
        self.ops.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait::async_trait]
impl BirthdayDiscord for FakeDiscord {
    fn available(&self) -> bool {
        !self.unavailable
    }
    fn guild(&self) -> Option<bot_util::placeholder_parser::PlaceholderGuild> {
        Some(bot_util::placeholder_parser::PlaceholderGuild { id: "g".into(), name: Some("Guild".into()), ..Default::default() })
    }
    fn channel(&self, id: &str) -> Option<bot_util::placeholder_parser::PlaceholderChannel> {
        self.channels.contains_key(id).then(|| bot_util::placeholder_parser::PlaceholderChannel { id: id.into(), name: Some("birthdays".into()), kind: Some(0) })
    }
    fn channel_usable(&self, id: &str, _needs: &[&str]) -> bool {
        self.channels.get(id).copied().unwrap_or(false)
    }
    fn role_exists(&self, id: &str) -> bool {
        self.roles.contains_key(id)
    }
    fn role_assignable(&self, id: &str, allow_dangerous: bool) -> bool {
        self.roles.get(id).is_some_and(|(g, r)| if allow_dangerous { *r } else { *g })
    }
    async fn members(&self, ids: &[String]) -> Result<HashMap<String, BdMember>, DiscordFail> {
        if self.slow_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.slow_ms)).await;
        }
        let all = self.members.lock().unwrap_or_else(|e| e.into_inner());
        Ok(ids.iter().filter_map(|id| all.get(id).map(|m| (id.clone(), m.clone()))).collect())
    }
    async fn send(&self, channel_id: &str, content: &str, user_ids: &[String]) -> Result<(), DiscordFail> {
        if let Some(code) = *self.fail_send.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(DiscordFail { code: Some(code), message: "send failed".into() });
        }
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).push((channel_id.into(), content.into(), user_ids.to_vec()));
        Ok(())
    }
    async fn add_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), DiscordFail> {
        if let Some(code) = *self.fail_add.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(DiscordFail { code: Some(code), message: "add failed".into() });
        }
        self.ops.lock().unwrap_or_else(|e| e.into_inner()).push(("add".into(), user_id.into(), reason.into()));
        if let Some(m) = self.members.lock().unwrap_or_else(|e| e.into_inner()).get_mut(user_id) {
            m.roles.push(role_id.into());
        }
        Ok(())
    }
    async fn remove_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), DiscordFail> {
        self.ops.lock().unwrap_or_else(|e| e.into_inner()).push(("remove".into(), user_id.into(), reason.into()));
        if let Some(m) = self.members.lock().unwrap_or_else(|e| e.into_inner()).get_mut(user_id) {
            m.roles.retain(|r| r != role_id);
        }
        Ok(())
    }
}

// ── Database ──────────────────────────────────────────────────────────────────

const CH: &str = "811000000000000001";
const ROLE: &str = "821000000000000001";

async fn db_app(guilds: &[&'static str]) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds(guilds.iter().copied());
    t.reset_guilds(guilds).await;
    Some(t)
}

async fn configure(t: &App, g: &str, patch: SettingsPatch) -> Settings {
    t.settings().update_settings(g, patch.set("timezone", "UTC")).await.expect("settings")
}

#[allow(clippy::too_many_arguments)]
async fn insert_bd(t: &App, g: &str, user: &str, month: i32, day: i32, year: Option<i32>, last: Option<i32>, granted: Option<DateTime<Utc>>) -> String {
    t.settings().ensure_guild_settings(g).await.expect("settings row");
    let id = bot_db::new_cuid();
    sqlx::query(r#"INSERT INTO "Birthday" ("id", "guildId", "userId", "month", "day", "year", "lastAnnouncedYear", "roleGrantedAt", "updatedAt") VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() AT TIME ZONE 'UTC')"#)
        .bind(&id)
        .bind(g)
        .bind(user)
        .bind(month)
        .bind(day)
        .bind(year)
        .bind(last)
        .bind(granted.map(|d| d.naive_utc()))
        .execute(t.db())
        .await
        .expect("insert");
    id
}

async fn row(t: &App, g: &str, user: &str) -> Option<bot_db::Birthday> {
    svc::get_birthday(t.db(), g, user).await.expect("db")
}

#[tokio::test]
async fn db_raw_sql_and_the_claim_mark_rows_once() {
    const G: &str = "961000000000000001";
    let Some(t) = db_app(&[G]).await else { return };
    let mut tx = t.pool.begin().await.expect("tx");
    for (name, sql) in svc::raw_sql() {
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(2026_i32).bind("0").bind(vec!["sample".to_string()]).fetch_all(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(rows.is_empty(), "{name}");
    }
    tx.rollback().await.expect("rollback");

    let a = insert_bd(&t, G, "71001", 10, 10, Some(2000), None, None).await;
    let b = insert_bd(&t, G, "71002", 10, 10, None, Some(2025), None).await;
    let c = insert_bd(&t, G, "71003", 10, 10, None, Some(2026), None).await;
    let ids = vec![a.clone(), b.clone(), c.clone()];
    let claimed = svc::claim_birthdays(t.db(), G, &ids, 2026, true).await.expect("claim");
    let mut users: Vec<(String, Option<i64>)> = claimed.iter().map(|r| (r.user_id.clone(), r.year)).collect();
    users.sort();
    assert_eq!(users, vec![("71001".into(), Some(2000)), ("71002".into(), None)]);
    assert!(svc::claim_birthdays(t.db(), G, &ids, 2026, true).await.expect("claim").is_empty(), "a concurrent run never claims twice");
    let r = row(&t, G, "71001").await.expect("row");
    assert_eq!(r.last_announced_year, Some(2026));
    assert!(r.role_granted_at.is_some());
    assert!(row(&t, G, "71003").await.expect("row").role_granted_at.is_none());

    // revertClaims only for the claimed year; clearMarker is conditional.
    svc::revert_claims(t.db(), G, &ids, 2026).await;
    assert_eq!(row(&t, G, "71001").await.and_then(|r| r.last_announced_year), Some(2025));
    assert_eq!(row(&t, G, "71003").await.and_then(|r| r.last_announced_year), Some(2025), "every row of that year");
    let granted = row(&t, G, "71001").await.and_then(|r| r.role_granted_at).expect("marker");
    let stale = Marker { id: a.clone(), user_id: "71001".into(), month: 10, day: 10, role_granted_at: granted - chrono::Duration::seconds(5) };
    assert_eq!(svc::clear_marker(t.db(), &stale).await.expect("db"), 0);
    let fresh = Marker { role_granted_at: granted, ..stale };
    assert_eq!(svc::clear_marker(t.db(), &fresh).await.expect("db"), 1);
    assert_eq!(svc::count_birthdays(t.db(), G).await.expect("db"), (3, 2));
    assert!(svc::has_markers(t.db(), G).await.expect("db"), "71002 still has one");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_save_birthday_creates_updates_and_carries_the_guard_over() {
    const G: &str = "961000000000000002";
    let Some(t) = db_app(&[G]).await else { return };
    let s = configure(&t, G, SettingsPatch::new().set("birthdayHour", 9)).await;
    let morning = d("2026-10-10T08:00:00Z");
    let r = svc::save_birthday(&t, G, "71010", BirthdayDate { month: 3, day: 14, year: Some(1998) }, Some(&s), &morning).await.expect("saved");
    assert!(r.created && r.date_changed && !r.late && !r.already_celebrated);
    assert_eq!((r.row.month, r.row.day, r.row.year, r.row.last_announced_year), (3, 14, Some(1998), None));
    // Today, after the hour: late, counts from next year.
    let evening = d("2026-10-10T18:00:00Z");
    let r = svc::save_birthday(&t, G, "71010", BirthdayDate { month: 10, day: 10, year: None }, Some(&s), &evening).await.expect("saved");
    assert!(!r.created && r.late);
    assert_eq!(r.row.last_announced_year, Some(2026));
    // Changing the date again the same year: already celebrated.
    let r = svc::save_birthday(&t, G, "71010", BirthdayDate { month: 12, day: 1, year: None }, Some(&s), &evening).await.expect("saved");
    assert!(r.already_celebrated && !r.late);
    assert_eq!(r.row.last_announced_year, Some(2026), "carried over");
    // Today before the hour: nothing special.
    let r = svc::save_birthday(&t, G, "71011", BirthdayDate { month: 10, day: 10, year: None }, Some(&s), &morning).await.expect("saved");
    assert!(!r.late && !r.already_celebrated);

    // Forget-then-set: the tombstone restores the guard.
    t.cache().set(&svc::tombstone_key(G, "71012"), &2026, 3600).await;
    let r = svc::save_birthday(&t, G, "71012", BirthdayDate { month: 1, day: 5, year: None }, Some(&s), &evening).await.expect("saved");
    assert!(r.created && r.already_celebrated);
    assert_eq!(r.row.last_announced_year, Some(2026));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_remove_birthday_takes_the_role_off_first() {
    const G: &str = "961000000000000003";
    let Some(t) = db_app(&[G]).await else { return };
    let s = configure(&t, G, SettingsPatch::new().set("birthdayRoleId", ROLE)).await;
    insert_bd(&t, G, "71020", 10, 10, None, Some(2026), Some(Utc::now())).await;
    t.cache().set(&svc::tombstone_key(G, "71020"), &2026, 3600).await;
    let fake = FakeDiscord::default().member("71020", &[ROLE]).role(ROLE, true, true);
    assert!(svc::remove_birthday(&t, &fake, G, "71020", Some(&s)).await.expect("removed"));
    assert_eq!(fake.ops(), vec![("remove".into(), "71020".into(), "Birthday removed".into())]);
    assert!(row(&t, G, "71020").await.is_none());
    assert!(t.cache().get(&svc::tombstone_key(G, "71020")).await.is_some(), "the tombstone stays");
    assert!(!svc::remove_birthday(&t, &fake, G, "71020", Some(&s)).await.expect("nothing"));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_upcoming_birthdays_order_wrap_and_skip_bots_and_departed_members() {
    const G: &str = "961000000000000004";
    let Some(t) = db_app(&[G]).await else { return };
    let s = configure(&t, G, SettingsPatch::new()).await;
    for (u, m, dd) in [("71030", 10, 10), ("71031", 12, 31), ("71032", 1, 5), ("71033", 10, 9), ("71034", 11, 1), ("71035", 11, 1), ("71036", 10, 11)] {
        insert_bd(&t, G, u, m, dd, None, None, None).await;
    }
    let fake = FakeDiscord::default().member("71030", &[]).member("71031", &[]).member("71032", &[]).member("71033", &[]).member("71035", &[]).bot("71036");
    let list = svc::get_upcoming_birthdays(&t, &fake, G, Some(&s), 10, &d(NOW)).await.expect("list");
    let users: Vec<&str> = list.iter().map(|e| e.user_id.as_str()).collect();
    assert_eq!(users, vec!["71030", "71035", "71031", "71032", "71033"]);
    assert!(list[0].is_today);
    assert_eq!(list[4].occ, ld(2027, 10, 9));
    assert_eq!(iso(&list[2].at), "2026-12-31T00:00:00.000Z");
    let lines = feat_achievements::commands::birthday::next_lines(&list);
    assert_eq!(lines[0], "🎉 **Today** — <@71030>");
    assert!(lines[1].starts_with("**Nov 1** — <@71035> · <t:"));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_ensure_tick_job_schedules_keeps_replaces_and_cancels() {
    const G: &str = "961000000000000005";
    let Some(t) = db_app(&[G]).await else { return };
    let none = configure(&t, G, SettingsPatch::new()).await;
    svc::ensure_tick_job(&t, G, Some(&none), false).await;
    assert!(t.scheduler().get_job(&svc::tick_key(G)).await.is_none(), "nothing configured: no job");
    svc::ensure_tick_job(&t, G, None, false).await;
    let s = configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH)).await;
    svc::ensure_tick_job(&t, G, Some(&s), false).await;
    let job = t.scheduler().get_job(&svc::tick_key(G)).await.expect("scheduled");
    assert_eq!((job.r#type.as_str(), job.payload.clone(), job.status), ("birthday.tick", json!({}), bot_db::JobStatus::Pending));
    assert_eq!(job.run_at.timestamp_millis() % 3_600_000, 2_000, "just after the hour boundary");
    svc::ensure_tick_job(&t, G, Some(&s), false).await;
    assert_eq!(t.scheduler().get_job(&svc::tick_key(G)).await.map(|j| j.run_at), Some(job.run_at), "a pending job is kept");
    svc::ensure_tick_job(&t, G, Some(&s), true).await;
    let soon = t.scheduler().get_job(&svc::tick_key(G)).await.expect("scheduled");
    assert!(soon.run_at < job.run_at || soon.run_at.timestamp_millis() - Utc::now().timestamp_millis() <= svc::SOON_MS + 2000);
    // The guildReady hook revives a configured guild's job only.
    t.scheduler().cancel_jobs(bot_core::scheduler::CancelJobs { dedupe_key: Some(svc::tick_key(G)), ..Default::default() }).await;
    svc::on_guild_ready(&t, G, Some(&none)).await;
    assert!(t.scheduler().get_job(&svc::tick_key(G)).await.is_none());
    svc::on_guild_ready(&t, G, Some(&s)).await;
    assert!(t.scheduler().get_job(&svc::tick_key(G)).await.is_some());
    // Unconfigured and no marker left: cancelled.
    let off = configure(&t, G, SettingsPatch::new().set("birthdayChannelId", Value::Null)).await;
    svc::ensure_tick_job(&t, G, Some(&off), true).await;
    assert!(t.scheduler().get_job(&svc::tick_key(G)).await.is_none());
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_transfer_birthday_role_moves_todays_celebrants_off_the_old_role() {
    const G: &str = "961000000000000006";
    let Some(t) = db_app(&[G]).await else { return };
    insert_bd(&t, G, "71040", 10, 10, None, Some(2026), Some(Utc::now())).await;
    insert_bd(&t, G, "71041", 10, 10, None, Some(2026), Some(Utc::now())).await;
    let fake = Arc::new(FakeDiscord::default().member("71040", &[ROLE]).member("71041", &[]).role(ROLE, true, true));
    let (removed, failed) = svc::transfer_birthday_role(&t, fake.clone(), G, ROLE, Some("821000000000000002")).await;
    assert_eq!((removed, failed), (1, 0));
    assert!(svc::has_markers(t.db(), G).await.expect("db"), "a new role keeps the markers");
    let (removed, failed) = svc::transfer_birthday_role(&t, Arc::new(FakeDiscord::default().member("71040", &[ROLE]).role(ROLE, false, false)), G, ROLE, None).await;
    assert_eq!((removed, failed), (0, 2), "the bot can no longer manage the old role");
    assert!(!svc::has_markers(t.db(), G).await.expect("db"), "no new role: every marker is cleared");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_tick_announces_together_pings_only_celebrants_grants_once_and_never_twice() {
    const G: &str = "961000000000000007";
    let Some(t) = db_app(&[G]).await else { return };
    configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH).set("birthdayRoleId", ROLE).set("birthdayHour", 9)).await;
    insert_bd(&t, G, "71050", 10, 10, Some(2000), None, None).await;
    insert_bd(&t, G, "71051", 10, 10, None, None, None).await;
    insert_bd(&t, G, "71052", 10, 10, None, None, None).await; // departed
    insert_bd(&t, G, "71053", 10, 10, None, None, None).await; // a bot
    insert_bd(&t, G, "71054", 3, 1, None, None, None).await;
    let fake = FakeDiscord::default().member("71050", &[]).member("71051", &[]).bot("71053").member("71054", &[]).channel(CH, true).role(ROLE, true, true);

    // Before the hour: nothing.
    let next = svc::tick_guild(&t, &fake, G, &d("2026-10-10T08:30:00Z")).await.expect("tick");
    assert!(fake.sent().is_empty());
    assert!(next.is_some(), "configured: the job recurs");

    svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick");
    let sent = fake.sent();
    assert_eq!(sent.len(), 1, "announced together");
    assert_eq!(sent[0].0, CH);
    // The claim returns rows in the database's order (as in the JS): compare as sets.
    let mut lines: Vec<&str> = sent[0].1.split('\n').collect();
    lines.sort();
    assert_eq!(lines, vec!["Happy birthday <@71050>! They are now 26! 🎂", "Happy birthday <@71051>! 🎂"]);
    let mut pinged = sent[0].2.clone();
    pinged.sort();
    assert_eq!(pinged, vec!["71050".to_string(), "71051".to_string()]);
    assert_eq!(fake.ops().iter().filter(|o| o.0 == "add").count(), 2);
    assert_eq!(row(&t, G, "71050").await.and_then(|r| r.last_announced_year), Some(2026));
    assert!(row(&t, G, "71052").await.and_then(|r| r.last_announced_year).is_none(), "departed members are never claimed");
    assert_eq!(t.cache().get(&svc::tombstone_key(G, "71050")).await, Some(json!(2026)));

    // A second run the same day: nothing new, no second grant.
    svc::tick_guild(&t, &fake, G, &d("2026-10-10T13:00:00Z")).await.expect("tick");
    assert_eq!(fake.sent().len(), 1);
    assert_eq!(fake.ops().iter().filter(|o| o.0 == "add").count(), 2);

    // The next day the role comes off and the marker is cleared.
    svc::tick_guild(&t, &fake, G, &d("2026-10-11T10:00:00Z")).await.expect("tick");
    let removes: Vec<(String, String, String)> = fake.ops().into_iter().filter(|o| o.0 == "remove").collect();
    assert_eq!(removes.len(), 2);
    assert_eq!(removes[0].2, "Birthday is over");
    assert!(!svc::has_markers(t.db(), G).await.expect("db"));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_tick_module_off_failed_send_silent_completion_and_unmanaged_roles() {
    const G: &str = "961000000000000008";
    let Some(t) = db_app(&[G]).await else { return };
    // Module off: no celebration, but expired roles are still removed.
    configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH).set("birthdayRoleId", ROLE).set("birthdayHour", 0).set("birthdaysEnabled", false)).await;
    insert_bd(&t, G, "71060", 10, 10, None, None, None).await;
    insert_bd(&t, G, "71061", 10, 9, None, Some(2026), Some(d("2026-10-09T09:00:00Z"))).await;
    let fake = FakeDiscord::default().member("71060", &[]).member("71061", &[ROLE]).channel(CH, true).role(ROLE, true, true);
    svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick");
    assert!(fake.sent().is_empty());
    assert_eq!(fake.ops(), vec![("remove".to_string(), "71061".to_string(), "Birthday is over".to_string())]);

    // A failed send reverts the claim; the next run announces.
    configure(&t, G, SettingsPatch::new().set("birthdaysEnabled", true)).await;
    *fake.fail_send.lock().unwrap_or_else(|e| e.into_inner()) = Some(50013);
    svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick");
    assert_eq!(row(&t, G, "71060").await.and_then(|r| r.last_announced_year), Some(2025), "reverted");
    assert_eq!(fake.ops().iter().filter(|o| o.0 == "add").count(), 1, "the role is still granted");
    *fake.fail_send.lock().unwrap_or_else(|e| e.into_inner()) = None;
    svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick");
    assert_eq!(fake.sent().len(), 1);
    assert_eq!(fake.ops().iter().filter(|o| o.0 == "add").count(), 1, "already held");

    // Without a usable channel the celebration is completed silently; a role that is dangerous is never granted.
    insert_bd(&t, G, "71062", 10, 10, None, None, None).await;
    let quiet = FakeDiscord::default().member("71062", &[]).channel(CH, false).role(ROLE, false, true);
    svc::tick_guild(&t, &quiet, G, &d(NOW)).await.expect("tick");
    assert!(quiet.sent().is_empty());
    assert_eq!(row(&t, G, "71062").await.and_then(|r| r.last_announced_year), Some(2026));
    assert!(quiet.ops().is_empty(), "never granted");

    // Nothing configured and no marker: the job ends.
    configure(&t, G, SettingsPatch::new().set("birthdayChannelId", Value::Null).set("birthdayRoleId", Value::Null)).await;
    sqlx::query(r#"UPDATE "Birthday" SET "roleGrantedAt" = NULL WHERE "guildId" = $1"#).bind(G).execute(t.db()).await.expect("db");
    assert_eq!(svc::tick_guild(&t, &quiet, G, &d(NOW)).await.expect("tick"), None);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_tick_claims_at_most_fifty_feb_29_and_time_zones() {
    const G: &str = "961000000000000009";
    let Some(t) = db_app(&[G]).await else { return };
    configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH).set("birthdayHour", 0)).await;
    let mut fake = FakeDiscord::default().channel(CH, true);
    for i in 0..55 {
        let u = format!("7107{i:02}");
        insert_bd(&t, G, &u, 10, 10, None, None, None).await;
        fake = fake.member(&u, &[]);
    }
    let next = svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick").expect("continues");
    assert!(next.timestamp_millis() - Utc::now().timestamp_millis() <= svc::CONTINUE_MS + 5_000, "a continuation run in a minute");
    let n: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Birthday" WHERE "guildId" = $1 AND "lastAnnouncedYear" = 2026"#).bind(G).fetch_one(t.db()).await.expect("db");
    assert_eq!(n, 50);
    svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick");
    let n: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Birthday" WHERE "guildId" = $1 AND "lastAnnouncedYear" = 2026"#).bind(G).fetch_one(t.db()).await.expect("db");
    assert_eq!(n, 55);

    // Feb 29 in a non-leap year: celebrated on Feb 28.
    insert_bd(&t, G, "71080", 2, 29, None, None, None).await;
    let fake = FakeDiscord::default().member("71080", &[]).channel(CH, true);
    svc::tick_guild(&t, &fake, G, &d("2027-02-28T12:00:00Z")).await.expect("tick");
    assert_eq!(fake.sent().len(), 1);
    // The guild time zone decides the date and the hour.
    insert_bd(&t, G, "71081", 10, 11, None, None, None).await;
    configure(&t, G, SettingsPatch::new()).await;
    t.settings().update_settings(G, SettingsPatch::new().set("timezone", "Pacific/Kiritimati").set("birthdayHour", 9)).await.expect("settings");
    let fake = FakeDiscord::default().member("71081", &[]).channel(CH, true);
    svc::tick_guild(&t, &fake, G, &d("2026-10-10T18:30:00Z")).await.expect("tick"); // 08:30 on Oct 11 there
    assert!(fake.sent().is_empty());
    svc::tick_guild(&t, &fake, G, &d("2026-10-10T19:30:00Z")).await.expect("tick"); // 09:30
    assert_eq!(fake.sent().len(), 1);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_run_birthday_tick_running_guard_and_unavailable_guild() {
    const G: &str = "961000000000000010";
    let Some(t) = db_app(&[G]).await else { return };
    configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH)).await;
    let before = Utc::now();
    let out = svc::run_birthday_tick_with(&t, G, Arc::new(FakeDiscord { unavailable: true, ..Default::default() })).await;
    match out {
        JobOutcome::Reschedule { next_run_at, .. } => assert!((next_run_at - before).num_milliseconds() >= svc::ERROR_RETRY_MS - 1000),
        other => panic!("{other:?}"),
    }
    let out = svc::run_birthday_tick_with(&t, G, Arc::new(FakeDiscord::default().channel(CH, true))).await;
    assert!(matches!(out, JobOutcome::Reschedule { .. }));
    // Offline (no Discord connection): the production handler waits five minutes.
    assert!(matches!(svc::run_birthday_tick(&t, G).await, JobOutcome::Reschedule { .. }));
    t.reset_guilds(&[G]).await;
}

// ── /birthday through the router ──────────────────────────────────────────────

fn member_cmd(sub: &str, g: &str, user: &str) -> FakeInteraction {
    FakeInteraction::slash("birthday").guild(g).user(user).subcommand(sub)
}

fn admin_cmd(sub: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash("birthday").guild(g).user("600000000000000009").roles(&["820000000000000002"]).permissions(Permissions::MANAGE_GUILD).group("config", sub)
}

fn last(t: &DbTestApp) -> (String, String) {
    let c = t.responses.calls().last().cloned();
    (c.as_ref().and_then(|c| c.embed_title().map(String::from)).unwrap_or_default(), c.and_then(|c| c.embed_description().map(String::from)).unwrap_or_default())
}

#[tokio::test]
async fn db_birthday_member_and_config_subcommands() {
    const G: &str = "961000000000000020";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    configure(&t, G, SettingsPatch::new().set("birthdaysEnabled", true)).await;
    let run = |i: FakeInteraction| {
        t.responses.clear();
        route_interaction(&t, i.build())
    };

    run(member_cmd("set", G, "71100").option("date", ot::STRING, json!("02-30"))).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(t.responses.calls()[0].ephemeral(), "set answers privately");
    assert_eq!(last(&t), ("❌  Invalid Date".into(), "**02-30** is not a real date.".into()));
    run(member_cmd("set", G, "71100").option("date", ot::STRING, json!("1998-03-14"))).await;
    assert_eq!(last(&t), ("✅  Birthday Saved".into(), "I'll celebrate your birthday on **March 14, 1998**.\n\nBirthday announcements aren't set up on this server yet.".into()));
    run(member_cmd("set", G, "71100").option("date", ot::STRING, json!("03-15"))).await;
    assert_eq!(last(&t).0, "✅  Birthday Updated");
    run(member_cmd("show", G, "71100")).await;
    assert!(!t.responses.calls()[0].ephemeral(), "show is public");
    let (title, desc) = last(&t);
    assert_eq!(title, "🎂 Birthday");
    assert!(desc.starts_with("<@71100>'s birthday is **March 15**.\nNext: <t:"), "{desc}");
    run(member_cmd("next", G, "71100")).await;
    let (title, desc) = last(&t);
    assert_eq!(title, "🎂 Upcoming Birthdays");
    assert!(desc.starts_with("**Mar 15** — <@71100> · <t:"), "{desc}");
    run(member_cmd("forget", G, "71100")).await;
    assert_eq!(last(&t), ("✅  Birthday Forgotten".into(), "I've removed your birthday from this server.".into()));
    run(member_cmd("forget", G, "71100")).await;
    assert_eq!(last(&t).0, "ℹ️  Nothing to Forget");
    run(member_cmd("next", G, "71100")).await;
    assert_eq!(last(&t).0, "ℹ️  No Birthdays Yet");

    // Config.
    run(admin_cmd("hour", G).option("hour", ot::INTEGER, json!(7))).await;
    assert!(!t.responses.calls()[0].ephemeral());
    assert_eq!(last(&t), ("✅  Announcement Hour Set".into(), "Birthdays will be announced at **07:00** (UTC).".into()));
    run(admin_cmd("require-year", G).option("enabled", ot::BOOLEAN, json!(true))).await;
    assert_eq!(last(&t), ("✅  Birth Year Required".into(), "Members must now use **YYYY-MM-DD**.".into()));
    run(member_cmd("set", G, "71101").option("date", ot::STRING, json!("03-14"))).await;
    assert_eq!(last(&t).1, "This server asks for your birth year too: use **YYYY-MM-DD** (like `1998-03-14`).");
    run(admin_cmd("message", G).option("text", ot::STRING, json!("Yay {user}! {age} {nope}"))).await;
    let (title, desc) = last(&t);
    assert_eq!(title, "✅  Birthday Message Updated");
    assert!(desc.contains("⚠️ Unknown variables (sent as typed): `{nope}`") && desc.contains("`{age}` is always empty"), "{desc}");
    run(admin_cmd("message-age", G)).await;
    assert_eq!(last(&t), ("✅  Birthday Message Reset".into(), format!("Back to the default: `{}`", svc::DEFAULT_BIRTHDAY_MESSAGE_AGE)));
    run(admin_cmd("unset-user", G).option("member", ot::USER, json!("71102"))).await;
    assert_eq!(last(&t), ("ℹ️  Nothing to Remove".into(), "<@71102> has no saved birthday.".into()));
    run(admin_cmd("channel", G)).await;
    assert_eq!(last(&t), ("✅  Birthday Announcements Off".into(), "Birthdays will no longer be announced. Saved birthdays are kept.".into()));
    run(admin_cmd("role", G)).await;
    assert_eq!(last(&t), ("✅  Birthday Role Off".into(), "No role will be given on birthdays.".into()));
    run(admin_cmd("show", G)).await;
    let call = t.responses.calls().last().cloned().expect("reply");
    assert_eq!(call.embed_title(), Some("🎂 Birthday Settings"));
    let fields: Vec<(String, String)> = call.body["embeds"][0]["fields"].as_array().into_iter().flatten().map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into())).collect();
    assert_eq!(fields[0], ("Module".into(), "✅ On".into()));
    assert_eq!(fields[1], ("Channel".into(), "Not set (no announcements)".into()));
    assert_eq!(fields[3], ("Announcement hour".into(), "**07:00** (UTC)".into()));
    assert_eq!(fields[4], ("Birth year".into(), "Required".into()));
    assert_eq!(fields[6], ("Next check".into(), "Off (no channel or role set)".into()));
    assert_eq!(fields[7].1, "```\nYay {user}! {age} {nope}\n```");
    assert_eq!(fields[8].1, format!("```\n{}\n```\n(default)", svc::DEFAULT_BIRTHDAY_MESSAGE_AGE));
    run(admin_cmd("test", G)).await;
    assert!(t.responses.calls()[0].ephemeral(), "test is a private preview");
    assert_eq!(last(&t).0, "🎂 Birthday Preview");

    // The module is off: member subcommands refuse with the router wording, config still answers with a note.
    configure(&t, G, SettingsPatch::new().set("birthdaysEnabled", false)).await;
    for sub in ["set", "forget", "show", "next"] {
        let mut i = member_cmd(sub, G, "71100");
        if sub == "set" {
            i = i.option("date", ot::STRING, json!("1998-03-14"));
        }
        run(i).await;
        assert_eq!(
            last(&t),
            ("⚠️  Module Disabled".into(), "The **Birthdays** module is turned off on this server. An admin can enable it with `/module enable birthdays`.".into()),
            "{sub}"
        );
    }
    run(admin_cmd("hour", G).option("hour", ot::INTEGER, json!(8))).await;
    assert!(last(&t).1.ends_with("\n\n⚠️ The **Birthdays** module is off. Turn it on with `/module enable birthdays`."));
    t.reset_guilds(&[G]).await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_share_birthdays_tombstones_and_the_tick_job() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else { return };
    const G: &str = "961000000000000030";
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([G]);
    t.reset_guilds(&[G]).await;
    let s = configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH).set("birthdayHour", 9)).await;

    // JS saves, Rust reads (the same JSON through Prisma and Rust).
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-save", G, "71200", r#"{"month":10,"day":10,"year":1990}"#, "2026-10-10T18:00:00.000Z"]);
    assert_eq!(js["late"], true, "{js}");
    let ours = row(&t, G, "71200").await.expect("row");
    assert_eq!(serde_json::to_value(&ours).unwrap_or(Value::Null), js["row"]);
    assert_eq!(ours.last_announced_year, Some(2026));

    // Rust saves, JS reads.
    let r = svc::save_birthday(&t, G, "71201", BirthdayDate { month: 2, day: 29, year: None }, Some(&s), &d("2026-10-10T08:00:00Z")).await.expect("saved");
    let back = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-get", G, "71201"]);
    assert_eq!(back, serde_json::to_value(&r.row).unwrap_or(Value::Null));

    // Tombstones: the JS tick's value guards a Rust save; a Rust tick's value reads in JS.
    t.cache().del_one(&svc::tombstone_key(G, "71202")).await;
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-tomb-set", G, "71202", "2026", "2026-10-10T12:00:00.000Z"]);
    assert_eq!(js["ok"], true, "{js}");
    assert_eq!(js["ttl"], svc::tombstone_ttl_seconds(&d("2026-10-10T12:00:00Z"), "UTC"));
    let r = svc::save_birthday(&t, G, "71202", BirthdayDate { month: 1, day: 1, year: None }, Some(&s), &d("2026-10-10T12:00:00Z")).await.expect("saved");
    assert!(r.already_celebrated, "the JS tombstone carries the guard over");
    let id = insert_bd(&t, G, "71203", 10, 10, None, None, None).await;
    let fake = FakeDiscord::default().member("71203", &[]).channel(CH, true);
    svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick");
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-tomb-get", G, "71203"]);
    assert_eq!(js["value"], 2026);
    let pttl = t.cache().get(&svc::tombstone_key(G, "71203")).await;
    assert_eq!(pttl, Some(json!(2026)));

    // The claim: a JS claim on a row Rust claimed returns nothing, and the reverse.
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-claim", G, &json!([id]).to_string(), "2026", "false"]);
    assert_eq!(js, json!([]));
    let id2 = insert_bd(&t, G, "71204", 10, 10, None, None, None).await;
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-claim", G, &json!([id2]).to_string(), "2026", "true"]);
    assert_eq!(js[0]["userId"], "71204");
    assert!(svc::claim_birthdays(t.db(), G, &[id2], 2026, true).await.expect("claim").is_empty());

    // The recurring job: JS schedules, Rust sees it; Rust replaces, JS reads it.
    t.scheduler().cancel_jobs(bot_core::scheduler::CancelJobs { dedupe_key: Some(svc::tick_key(G)), ..Default::default() }).await;
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-ensure", G]);
    assert_eq!((js["type"].clone(), js["dedupeKey"].clone(), js["payload"].clone(), js["status"].clone()), (json!("birthday.tick"), json!(svc::tick_key(G)), json!({}), json!("PENDING")), "{js}");
    let job = t.scheduler().get_job(&svc::tick_key(G)).await.expect("job");
    assert_eq!(serde_json::to_value(&job).unwrap_or(Value::Null)["runAt"], js["runAt"]);
    svc::ensure_tick_job(&t, G, Some(&s), true).await;
    let js = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["bd-job", G]);
    let ours = t.scheduler().get_job(&svc::tick_key(G)).await.expect("job");
    assert_eq!(js, serde_json::to_value(&ours).unwrap_or(Value::Null));
    t.reset_guilds(&[G]).await;
}

// ── review-community2: departed members cannot starve the members behind them ──

async fn departed_world(t: &App, g: &'static str, departed: usize, present: usize) -> FakeDiscord {
    configure(t, g, SettingsPatch::new().set("birthdayChannelId", CH).set("birthdayHour", 0)).await;
    let mut fake = FakeDiscord::default().channel(CH, true);
    // Departed members sort first by user id, so they fill the first pages.
    for i in 0..departed {
        insert_bd(t, g, &format!("7130{i:03}"), 10, 10, None, None, None).await;
    }
    for i in 0..present {
        let u = format!("7190{i:03}");
        insert_bd(t, g, &u, 10, 10, None, None, None).await;
        fake = fake.member(&u, &[]);
    }
    fake
}

async fn celebrated(t: &App, g: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Birthday" WHERE "guildId" = $1 AND "lastAnnouncedYear" = 2026"#).bind(g).fetch_one(t.db()).await.expect("db")
}

#[tokio::test]
async fn db_tick_pages_past_departed_members() {
    const G: &str = "961000000000000011";
    let Some(t) = db_app(&[G]).await else { return };
    let fake = departed_world(&t, G, 60, 3).await;
    let next = svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick").expect("recurs");
    assert_eq!(celebrated(&t, G).await, 3, "the 3 present members behind 60 departed ones are celebrated");
    assert_eq!(fake.sent().len(), 1);
    assert_eq!(next.timestamp_millis() % 3_600_000, 2_000, "no continuation run: the next hour boundary");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_tick_only_a_51st_present_member_asks_for_a_continuation_run() {
    const G: &str = "961000000000000012";
    const H: &str = "961000000000000013";
    let Some(t) = db_app(&[G, H]).await else { return };
    let fake = departed_world(&t, G, 55, 51).await;
    let next = svc::tick_guild(&t, &fake, G, &d(NOW)).await.expect("tick").expect("recurs");
    assert_eq!(celebrated(&t, G).await, 50);
    assert!(next.timestamp_millis() - Utc::now().timestamp_millis() <= svc::CONTINUE_MS + 5_000, "a continuation run");
    let fake = departed_world(&t, H, 55, 50).await;
    let next = svc::tick_guild(&t, &fake, H, &d(NOW)).await.expect("tick").expect("recurs");
    assert_eq!(celebrated(&t, H).await, 50);
    assert_eq!(next.timestamp_millis() % 3_600_000, 2_000, "exactly 50: no continuation run");
    t.reset_guilds(&[G, H]).await;
}

#[tokio::test]
async fn db_tick_retries_a_failed_grant_and_skips_a_pending_member() {
    const G: &str = "961000000000000014";
    let Some(t) = db_app(&[G]).await else { return };
    configure(&t, G, SettingsPatch::new().set("birthdayRoleId", ROLE).set("birthdayHour", 0)).await;
    // The claim stamps the marker with the database clock, so the run happens today.
    let now = Utc::now();
    let today = svc::LocalDate::from(bot_util::time::zoned_parts(&now, "UTC"));
    insert_bd(&t, G, "71400", today.month as i32, today.day as i32, None, None, None).await;
    insert_bd(&t, G, "71401", today.month as i32, today.day as i32, None, None, None).await;
    let fake = FakeDiscord::default().member("71400", &[]).member("71401", &[]).role(ROLE, true, true);
    if let Some(m) = fake.members.lock().unwrap_or_else(|e| e.into_inner()).get_mut("71401") {
        m.pending = true;
    }
    *fake.fail_add.lock().unwrap_or_else(|e| e.into_inner()) = Some(50013);
    svc::tick_guild(&t, &fake, G, &now).await.expect("tick");
    assert!(fake.ops().is_empty(), "the grant failed");
    assert!(row(&t, G, "71400").await.and_then(|r| r.role_granted_at).is_some(), "the marker is set at the claim");
    *fake.fail_add.lock().unwrap_or_else(|e| e.into_inner()) = None;
    svc::tick_guild(&t, &fake, G, &now).await.expect("tick");
    assert_eq!(fake.ops(), vec![("add".to_string(), "71400".to_string(), "Birthday".to_string())], "retried by maintenance; the pending member is skipped");
    t.reset_guilds(&[G]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_run_birthday_tick_running_guard() {
    const G: &str = "961000000000000015";
    let Some(t) = db_app(&[G]).await else { return };
    configure(&t, G, SettingsPatch::new().set("birthdayChannelId", CH).set("birthdayHour", 0)).await;
    let today = svc::LocalDate::from(bot_util::time::zoned_parts(&Utc::now(), "UTC"));
    insert_bd(&t, G, "71500", today.month as i32, today.day as i32, None, None, None).await;
    let slow = Arc::new(FakeDiscord { slow_ms: 300, ..FakeDiscord::default() }.member("71500", &[]).channel(CH, true));
    let (app, d1) = (t.app.clone(), slow.clone());
    let first = tokio::spawn(async move { svc::run_birthday_tick_with(&app, G, d1).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let before = Utc::now();
    let second = svc::run_birthday_tick_with(&t, G, slow.clone()).await;
    match second {
        JobOutcome::Reschedule { next_run_at, .. } => {
            let ms = (next_run_at - before).num_milliseconds();
            assert!((svc::CONTINUE_MS - 1000..=svc::CONTINUE_MS + 1000).contains(&ms), "{ms}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(first.await.expect("joined"), JobOutcome::Reschedule { .. }));
    assert_eq!(slow.sent().len(), 1, "only the first run announced");
    t.reset_guilds(&[G]).await;
}

// ── Review fixes ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn serenity_members_use_one_gateway_request_and_fail_on_a_timeout() {
    // birthdayService.js fetchMembers: the cache first, then `guild.members.fetch({ user, time: 10 s })` per 100 ids;
    // a rejection propagates, an id Discord does not return has left.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    let g = "960100000000000031";
    let cached = "960100000000000311";
    let gone = "960100000000000319";
    let guild = FakeGuild::new(g, "960100000000000999").member(TEST_BOT_ID, &[]).member(cached, &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(std::slice::from_ref(&guild), &rest);
    let requests = Arc::new(AtomicUsize::new(0));
    let (app, count) = (t.app.clone(), requests.clone());
    bot_core::guild_members::set_chunk_sender(
        &t,
        Some(Arc::new(move |gid, ids, nonce| {
            count.fetch_add(1, Ordering::SeqCst);
            let members: Vec<Value> = ids
                .iter()
                .filter(|id| id.to_string() != gone)
                .map(|id| json!({ "user": { "id": id.to_string(), "username": format!("u{id}"), "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 }))
                .collect();
            let event: serenity::all::GuildMembersChunkEvent =
                serde_json::from_value(json!({ "guild_id": gid.to_string(), "members": members, "chunk_index": 0, "chunk_count": 1, "nonce": nonce, "not_found": [] }))
                    .expect("chunk json");
            bot_core::guild_members::deliver_members_chunk(&app.ext::<bot_core::guild_members::MemberChunks>(), &event);
            Box::pin(async { true })
        })),
    );
    let d = svc::SerenityBirthdays::new(&t, serenity::all::GuildId::new(g.parse().unwrap_or(1)));
    let ids: Vec<String> = [cached, "960100000000000312", gone].iter().map(|s| s.to_string()).collect();
    let got = d.members(&ids).await.expect("members");
    let mut found: Vec<&str> = got.keys().map(String::as_str).collect();
    found.sort_unstable();
    assert_eq!(found, vec![cached, "960100000000000312"], "the departed member is absent");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(rest.calls_to("GET", "/guilds/*/members/*").is_empty(), "no REST call per member");
    // A gateway request that never answers rejects (the tick then changes nothing).
    bot_core::guild_members::set_chunk_sender(&t, Some(Arc::new(|_, _, _| Box::pin(async { false }))));
    assert!(d.members(&["960100000000000313".to_string()]).await.is_err());
}
