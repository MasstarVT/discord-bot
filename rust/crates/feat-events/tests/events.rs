//! Port of `test/events.test.js` and `test/calendar.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: every pure rule against JS-produced fixtures (cards, buttons, reminders, the calendar entry and payload, the
//! edit panel and forms, the field parsers at DST edges), the plugin declaration and the `/event` payload.
//! With `TEST_DATABASE_URL`: the service against the real tables (create, RSVP capacity under concurrency, roles,
//! end, cancel, edit, the jobs, the hooks, autocomplete), the command and buttons through the router with a fake
//! Discord REST, and both directions of the JS round trip.

mod common;

use std::collections::HashMap;

use bot_db::{GuildEventStatus, RsvpStatus};
use bot_util::timezones::{ResolvedTimezone, ZoneSource};
use common::*;
use feat_events::event_service::{self as svc, CurrentEvent, EventFields, RsvpOut, RsvpResult};
use serde_json::{Value, json};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn now_of(g: &Value) -> i64 {
    chrono::DateTime::parse_from_rfc3339(g["now"].as_str().unwrap_or_default()).map(|d| d.timestamp_millis()).unwrap_or(0)
}

fn iso_ms(v: &Value) -> i64 {
    chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap_or_default()).map(|d| d.timestamp_millis()).unwrap_or(0)
}

fn iso(d: &chrono::DateTime<chrono::Utc>) -> String {
    bot_db::format_js_date(d)
}

fn resolved_of(v: &Value) -> ResolvedTimezone {
    let source = match v["source"].as_str().unwrap_or("") {
        "personal" => ZoneSource::Personal,
        "role" => ZoneSource::Role,
        "server" => ZoneSource::Server,
        _ => ZoneSource::Default,
    };
    ResolvedTimezone {
        zone: s(&v["zone"]),
        source,
        role_id: v["roleId"].as_str().map(String::from),
        role_count: v["roleCount"].as_u64().unwrap_or(0) as usize,
    }
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["event", "rolepersist"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "events");
    bot_core::testing::assert_plugin_matches_golden(&reg, "role-persist");
    assert!(bot_core::testing::persistent_handler_problems_for(&reg, &["events", "role-persist"]).is_empty());
}

fn char_count(node: &Value) -> usize {
    let len = |v: &Value| v.as_str().map(|s| s.encode_utf16().count()).unwrap_or(0);
    let mut n = len(&node["name"]) + len(&node["description"]);
    for o in node["options"].as_array().into_iter().flatten() {
        n += char_count(o);
    }
    n
}

#[test]
fn payload_budgets_and_metadata() {
    let event = feat_events::commands::event::command();
    let json = event.payload();
    assert!(char_count(&json) <= 2000, "/event is {} chars", char_count(&json));
    assert_eq!(json.get("default_member_permissions"), None, "everyone sees /event");
    let meta = event.metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("events"), json!(true), json!(0)));
    let rp = feat_events::commands::rolepersist::command();
    assert!(char_count(&rp.payload()) <= 1000);
    let meta = rp.metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("rolepersist"), json!(false), json!(2)));
    // The /event subcommands, in the JS order.
    let subs: Vec<String> = json["options"].as_array().into_iter().flatten().map(|o| s(&o["name"])).collect();
    assert_eq!(subs, ["create", "edit", "cancel", "list", "info", "attendees"]);
}

// ── Golden: cards, buttons, reminders, lines ───────────────────────────────────

#[test]
fn cards_and_buttons_match_the_js() {
    let g = golden();
    let now = now_of(&g);
    for c in g["cards"].as_array().into_iter().flatten() {
        let name = s(&c["name"]);
        let event = event_of(&c["event"]);
        let t = &c["tally"];
        let tally = svc::Tally {
            going: t["going"].as_i64().unwrap_or(0),
            maybe: t["maybe"].as_i64().unwrap_or(0),
            declined: t["declined"].as_i64().unwrap_or(0),
            going_ids: t["goingIds"].as_array().into_iter().flatten().map(s).collect(),
        };
        let settings = settings_of(&c["settings"]);
        let names: Option<HashMap<String, String>> = c["names"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), s(v))).collect());
        let embed = svc::build_event_embed(&event, &tally, settings.as_ref(), now, names.as_ref());
        assert_json_eq(&to_json(&embed), &c["embed"], &format!("card {name}"));
        let rows = svc::build_event_components(&event, &tally, None);
        assert_json_eq(&to_json(&rows), &c["components"], &format!("buttons {name}"));
    }
}

#[test]
fn buttons_full_and_closed_like_the_js_test() {
    let g = golden();
    let mut e = event_of(&g["cards"][0]["event"]);
    e.capacity = Some(2);
    let row = |e: &bot_db::GuildEvent, going: i64| {
        let rows = to_json(&svc::build_event_components(e, &svc::Tally { going, ..Default::default() }, None));
        rows[0]["components"].as_array().cloned().unwrap_or_default()
    };
    let open = row(&e, 1);
    let ids: Vec<String> = open.iter().map(|b| s(&b["custom_id"])).collect();
    assert_eq!(ids, [
        "event:cevent000000000000000001:going",
        "event:cevent000000000000000001:maybe",
        "event:cevent000000000000000001:declined",
        "event:cevent000000000000000001:calendar"
    ]);
    let disabled = |r: &[Value]| r.iter().map(|b| b["disabled"].as_bool().unwrap_or(false)).collect::<Vec<_>>();
    assert_eq!(disabled(&open), [false, false, false, false]);
    let full = row(&e, 2);
    assert_eq!(disabled(&full), [true, false, false, false], "a full event can still be added to a calendar");
    assert_eq!(full[0]["label"], "Going (full)");
    for status in [GuildEventStatus::Ended, GuildEventStatus::Cancelled] {
        e.status = status;
        assert_eq!(disabled(&row(&e, 0)), [true, true, true, true]);
    }
    assert!(bot_config::CUSTOM_ID_PREFIXES.iter().any(|p| p.prefix == "event"));
}

#[test]
fn reminders_and_lines_match_the_js() {
    let g = golden();
    let now = now_of(&g);
    for r in g["reminders"].as_array().into_iter().flatten() {
        let event = event_of(&r["event"]);
        let ids: Vec<String> = r["userIds"].as_array().into_iter().flatten().map(s).collect();
        let out = svc::reminder_messages(&event, &ids, now);
        let got: Vec<Value> = out.iter().map(|m| json!({ "content": m.content, "userIds": m.user_ids })).collect();
        assert_eq!(Value::Array(got), r["out"], "reminders {}", r["name"]);
        for m in &out {
            assert!(m.content.encode_utf16().count() < 2000 && m.user_ids.len() <= 100);
        }
    }
    for l in g["lines"]["list"].as_array().into_iter().flatten() {
        assert_eq!(svc::list_line(&event_of(&l["event"]), l["going"].as_i64().unwrap_or(0)), s(&l["out"]));
    }
    let rsvps = vec![
        ("A".to_string(), RsvpStatus::Maybe),
        ("B".to_string(), RsvpStatus::Going),
        ("C".to_string(), RsvpStatus::Declined),
        ("D".to_string(), RsvpStatus::Going),
    ];
    assert_eq!(json!(svc::attendee_lines(&rsvps)), g["lines"]["attendees"]);
    let base = event_of(&g["cards"][0]["event"]);
    for r in g["lines"]["replies"].as_array().into_iter().flatten() {
        let result = match r["out"]["result"].as_str().unwrap_or("") {
            "gone" => RsvpResult::Gone,
            "closed" => RsvpResult::Closed,
            "full" => RsvpResult::Full,
            "same" => RsvpResult::Same,
            _ => RsvpResult::Set,
        };
        let status = match r["status"].as_str().unwrap_or("") {
            "GOING" => RsvpStatus::Going,
            "MAYBE" => RsvpStatus::Maybe,
            _ => RsvpStatus::Declined,
        };
        let out = RsvpOut { result, previous: None, note: r["out"]["note"].as_str().map(String::from) };
        let mut e = base.clone();
        if r["withChat"] == json!(true) {
            e.text_channel_id = Some("800000000000000002".into());
        }
        let ev = if r["noEvent"] == json!(true) { None } else { Some(&e) };
        assert_eq!(svc::rsvp_reply_text(&out, ev, status), s(&r["text"]), "{r}");
    }
}

// ── Golden: calendar ──────────────────────────────────────────────────────────

fn name_maps(v: &Value) -> svc::NameMaps {
    let map = |k: &str| v[k].as_object().map(|m| m.iter().map(|(a, b)| (a.clone(), s(b))).collect()).unwrap_or_default();
    svc::NameMaps { members: map("members"), users: map("users"), roles: map("roles"), channels: map("channels") }
}

#[test]
fn calendar_entries_and_payloads_match_the_js() {
    let g = golden();
    let now = now_of(&g);
    let cal = &g["calendar"];
    let names = name_maps(&cal["guildNames"]);
    for p in cal["plain"].as_array().into_iter().flatten() {
        let text = p["text"].as_str();
        assert_eq!(svc::plain_discord_text(text, &names), s(&p["withGuild"]), "{text:?}");
        assert_eq!(svc::plain_discord_text(text, &svc::NoNames), s(&p["without"]), "{text:?}");
    }
    for c in cal["entries"].as_array().into_iter().flatten() {
        let e = svc::calendar_entry(&event_of(&c["event"]), &names);
        let want = &c["entry"];
        assert_eq!(e.uid, s(&want["uid"]));
        assert_eq!(e.title, s(&want["title"]));
        assert_eq!(e.description.unwrap_or_default(), s(&want["description"]));
        assert_eq!(e.location.unwrap_or_default(), s(&want["location"]));
        assert_eq!(iso(&e.start), s(&want["start"]));
        assert_eq!(iso(&e.end), s(&want["end"]));
        assert_eq!(e.url.unwrap_or_default(), s(&want["url"]));
        assert_eq!(e.sequence, want["sequence"].as_f64());
    }
    for c in cal["payloads"].as_array().into_iter().flatten() {
        let p = svc::build_calendar_payload(&event_of(&c["event"]), &names, now, None);
        assert_eq!(p.content.clone().unwrap_or_default(), s(&c["content"]));
        assert_json_eq(&to_json(&p.components.clone().unwrap_or_default()), &c["components"], "calendar buttons");
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.files[0].filename, s(&c["fileName"]));
        assert_eq!(String::from_utf8_lossy(&p.files[0].data), s(&c["ics"]));
        for b in c["components"][0]["components"].as_array().into_iter().flatten() {
            assert!(s(&b["url"]).len() <= 512);
        }
        assert!(to_json(&p.allowed_mentions)["parse"] == json!([]), "no mentions");
    }
    let custom = svc::build_calendar_payload(&event_of(&cal["payloads"][0]["event"]), &svc::NoNames, now, Some("hello".into()));
    assert_eq!(custom.content.as_deref(), cal["customContent"].as_str());
}

fn cal_entry(v: &Value) -> feat_events::calendar::CalendarEntry {
    let opt = |k: &str| v[k].as_str().map(String::from);
    let date = |k: &str| chrono::DateTime::parse_from_rfc3339(v[k].as_str().unwrap_or_default()).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_default();
    feat_events::calendar::CalendarEntry {
        uid: s(&v["uid"]),
        title: s(&v["title"]),
        description: opt("description"),
        location: opt("location"),
        start: date("start"),
        end: date("end"),
        url: opt("url"),
        sequence: v["sequence"].as_f64(),
    }
}

/// `test/calendar.test.js`, against the JS output for the same entries.
#[test]
fn calendar_links_ics_and_text_rules_match_the_js() {
    use feat_events::calendar as cal;
    let g = golden();
    let c = &g["calendar"];
    for l in c["links"].as_array().into_iter().flatten() {
        let e = cal_entry(&l["entry"]);
        let max = l["max"].as_u64().map(|m| m as usize).unwrap_or(cal::LINK_URL_MAX);
        assert_eq!(cal::google_calendar_url(&e, max), s(&l["google"]), "google {}", l["name"]);
        assert_eq!(cal::outlook_calendar_url(&e, max), s(&l["outlook"]), "outlook {}", l["name"]);
        assert!(s(&l["google"]).len() <= max);
        let now = chrono::DateTime::from_timestamp_millis(1_792_670_400_000).unwrap_or_default();
        assert_eq!(cal::ics(&e, now), s(&l["ics"]), "ics {}", l["name"]);
    }
    for f in c["fit"].as_array().into_iter().flatten() {
        assert_eq!(cal::fit_encoded(&s(&f["text"]), f["budget"].as_i64().unwrap_or(0)), s(&f["out"]), "{f}");
    }
    for e in c["escape"].as_array().into_iter().flatten() {
        assert_eq!(cal::ics_escape(&s(&e["text"])), s(&e["out"]));
    }
    for f in c["fold"].as_array().into_iter().flatten() {
        let out = cal::ics_fold(&s(&f["line"]));
        assert_eq!(out, s(&f["out"]));
        for (i, part) in out.split("\r\n").enumerate() {
            assert!(part.len() <= 75, "line {i} is {} octets", part.len());
            if i > 0 {
                assert!(part.starts_with(' '));
            }
        }
        assert_eq!(out.replace("\r\n ", ""), s(&f["line"]));
    }
    // The JS test's own checks.
    let bare = cal::ics(
        &cal::CalendarEntry { description: Some(String::new()), location: Some(String::new()), url: Some(String::new()), sequence: None, ..cal_entry(&c["links"][0]["entry"]) },
        chrono::Utc::now(),
    );
    assert!(!bare.contains("\r\nDESCRIPTION:") && !bare.contains("\r\nLOCATION:") && !bare.contains("\r\nURL:"));
    assert!(bare.contains("\r\nSEQUENCE:0\r\n"));
    assert_eq!(cal::fit_encoded("abcdef", 3), "");
    assert_eq!(cal::ics_escape(""), "");
}

// ── Golden: edit panel, forms, parsers ─────────────────────────────────────────

#[test]
fn edit_panel_and_forms_match_the_js() {
    let g = golden();
    for p in g["panels"].as_array().into_iter().flatten() {
        let settings = settings_of(&p["settings"]);
        let panel = svc::build_edit_panel(&event_of(&p["event"]), settings.as_ref());
        assert_json_eq(&to_json(&panel.embeds.clone().unwrap_or_default()), &p["out"]["embeds"], "panel embeds");
        assert_json_eq(&to_json(&panel.components.clone().unwrap_or_default()), &p["out"]["components"], "panel buttons");
    }
    for m in g["modals"].as_array().into_iter().flatten() {
        let modal = svc::build_edit_modal(&event_of(&m["event"]), &s(&m["form"]), &s(&m["zone"]));
        assert_json_eq(&to_json(&modal), &m["out"], &format!("modal {} {}", m["form"], m["zone"]));
    }
}

#[test]
fn start_formatting_at_dst_edges_matches_the_js() {
    let g = golden();
    for c in g["formatStart"].as_array().into_iter().flatten() {
        let at = chrono::DateTime::parse_from_rfc3339(&s(&c["at"])).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_default();
        let zone = s(&c["zone"]);
        let text = svc::format_start_for_edit(&at, &zone);
        assert_eq!(text, s(&c["text"]), "{zone} {}", c["at"]);
        let (day, time) = svc::format_start_fields(&at, &zone);
        assert_eq!((day, time), (s(&c["fields"]["day"]), s(&c["fields"]["time"])));
        // `formatStartForEdit reads back through parseWhen in any zone` (the JS test's instant; a time in a DST
        // overlap reads back as its earlier instant, in the JS too).
        if c["at"] != "2026-11-05T01:30:00Z" {
            continue;
        }
        let back = bot_util::time::parse_when(&text, "UTC", &chrono::Utc::now()).and_then(|p| p.date());
        assert_eq!(back.map(|d| d.timestamp_millis()), Some(at.timestamp_millis() / 60_000 * 60_000), "{text}");
    }
}

fn when_out(r: &Result<svc::When, svc::FieldError>) -> Value {
    match r {
        Ok(w) => json!({ "ok": true, "values": {
            "startsAt": iso(&w.starts_at), "endsAt": w.ends_at.as_ref().map(iso), "zone": w.zone, "zoneFromInput": w.zone_from_input
        }}),
        Err(f) => json!({ "ok": false, "title": f.title, "text": f.text }),
    }
}

#[test]
fn start_parsing_at_dst_edges_matches_the_js() {
    let g = golden();
    for c in g["whenCases"].as_array().into_iter().flatten() {
        let resolved = ResolvedTimezone { zone: s(&c["zone"]), source: ZoneSource::Default, role_id: None, role_count: 0 };
        let r = svc::parse_event_when(&s(&c["start"]), Some("90m"), &resolved, None, c["now"].as_i64().unwrap_or(0), None, svc::CREATE_EXAMPLES);
        assert_eq!(when_out(&r), c["out"], "{} in {}", c["start"], c["zone"]);
    }
}

#[test]
fn field_parsing_matches_the_js() {
    let g = golden();
    for c in g["fieldCases"].as_array().into_iter().flatten() {
        let f = &c["fields"];
        let fields = EventFields {
            title: s(&f["title"]),
            description: f["description"].as_str().map(String::from),
            location: f["location"].as_str().map(String::from),
            start: s(&f["start"]),
            duration: f["duration"].as_str().map(String::from),
        };
        let resolved = resolved_of(&c["resolved"]);
        let current = c["current"].as_object().map(|cur| CurrentEvent {
            starts_at: chrono::DateTime::from_timestamp_millis(iso_ms(&cur["startsAt"])).unwrap_or_default(),
            discord_event_id: cur["discordEventId"].as_str().map(String::from),
        });
        let examples = c["examples"].as_str().unwrap_or(svc::CREATE_EXAMPLES);
        let now = c["now"].as_i64().unwrap_or(0);
        let out = svc::parse_event_details(&fields, current.as_ref()).and_then(|d| {
            svc::parse_event_when(&fields.start, fields.duration.as_deref(), &resolved, c["zone"].as_str(), now, current.as_ref(), examples).map(|w| (d, w))
        });
        let got = match &out {
            Ok((d, w)) => json!({ "ok": true, "values": {
                "title": d.title, "description": d.description, "location": d.location,
                "startsAt": iso(&w.starts_at), "endsAt": w.ends_at.as_ref().map(iso), "zone": w.zone, "zoneFromInput": w.zone_from_input
            }}),
            Err(e) => json!({ "ok": false, "title": e.title, "text": e.text }),
        };
        assert_eq!(got, c["out"], "case {}", c["name"]);
        if c["current"].is_null() && c["zone"].is_null() && c["examples"].is_null() {
            let direct = svc::parse_event_fields(&fields, &resolved, now);
            assert_eq!(direct.is_ok(), got["ok"] == json!(true));
        }
    }
    for l in g["limits"].as_array().into_iter().flatten() {
        let got = match svc::parse_event_limits(&s(&l["capacity"]), &s(&l["reminder"])) {
            Ok((c, r)) => json!({ "ok": true, "values": { "capacity": c, "reminderMinutes": r } }),
            Err(e) => json!({ "ok": false, "title": e.title, "text": e.text }),
        };
        assert_eq!(got, l["out"], "{l}");
    }
    for d in g["durations"].as_array().into_iter().flatten() {
        let got = match svc::parse_duration_input(d["raw"].as_str()) {
            Ok(ms) => json!({ "ms": ms }),
            Err(e) => json!({ "error": e.as_str() }),
        };
        assert_eq!(got, d["out"], "{d}");
    }
}

#[test]
fn small_rules_match_the_js() {
    let g = golden();
    let r = &g["rules"];
    let now = now_of(&g);
    let c = &r["constants"];
    use svc::consts as E;
    assert_eq!(c["ACTIVE_MAX"], json!(E::ACTIVE_MAX));
    assert_eq!(c["HOST_MAX"], json!(E::HOST_MAX));
    assert_eq!(c["CAPACITY_MAX"], json!(E::CAPACITY_MAX));
    assert_eq!(c["REMINDER_MAX"], json!(E::REMINDER_MAX));
    assert_eq!(c["DURATION_MIN_MS"], json!(E::DURATION_MIN_MS));
    assert_eq!(c["DURATION_MAX_MS"], json!(E::DURATION_MAX_MS));
    assert_eq!(c["HORIZON_MS"], json!(E::HORIZON_MS));
    assert_eq!(c["STRIP_BATCH"], json!(E::STRIP_BATCH));
    assert_eq!(c["STRIP_RUNS_MAX"], json!(E::STRIP_RUNS_MAX));
    assert_eq!(c["CLEANUP_RECHECK_MS"], json!(E::CLEANUP_RECHECK_MS));
    assert_eq!(c["EMPTY_GRACE_MS"], json!(E::EMPTY_GRACE_MS));
    assert_eq!(c["REMIND_CHUNK_USERS"], json!(E::REMIND_CHUNK_USERS));
    assert_eq!(c["GOING_SHOWN"], json!(E::GOING_SHOWN));
    for k in ["invalid", "too-short", "too-long"] {
        let e = [svc::DurationError::Invalid, svc::DurationError::TooShort, svc::DurationError::TooLong].into_iter().find(|e| e.as_str() == k);
        assert_eq!(e.map(|e| e.text()), r["durationErrors"][k].as_str());
    }
    for st in [RsvpStatus::Going, RsvpStatus::Maybe, RsvpStatus::Declined] {
        assert_eq!(r["rsvpLabels"][st.as_str()], svc::rsvp_label(st));
        assert_eq!(r["rsvpEmoji"][st.as_str()], svc::rsvp_emoji(st));
    }
    assert_eq!(r["notYours"], svc::NOT_YOURS_TEXT);
    assert_eq!(r["editForms"], json!(svc::EDIT_FORMS));
    for k in r["keys"].as_array().into_iter().flatten() {
        let id = s(&k["id"]);
        assert_eq!(
            (svc::remind_job_key(&id), svc::end_job_key(&id), svc::voice_job_key(&id), svc::cleanup_job_key(&id)),
            (s(&k["remind"]), s(&k["end"]), s(&k["voice"]), s(&k["cleanup"]))
        );
    }
    assert_eq!(r["jobTypes"], json!([svc::REMIND_JOB_TYPE, svc::END_JOB_TYPE, svc::VOICE_JOB_TYPE, svc::CLEANUP_JOB_TYPE]));
    for t in [svc::REMIND_JOB_TYPE, svc::END_JOB_TYPE, svc::VOICE_JOB_TYPE, svc::CLEANUP_JOB_TYPE] {
        assert!(bot_config::JobType::from_name(t).is_some(), "{t} is registered");
    }
    let ids = [
        bot_config::custom_id!("event", "cabc", "going").unwrap_or_default(),
        bot_config::custom_id!("event-edit", "cabc", "when").unwrap_or_default(),
        bot_config::custom_id!("event-modal", "cabc", "limits").unwrap_or_default(),
    ];
    assert_eq!(json!(ids), r["customIds"]);
    for v in r["voiceLeadText"].as_array().into_iter().flatten() {
        assert_eq!(svc::voice_lead_text(v["m"].as_i64().unwrap_or(0)), s(&v["out"]));
    }
    let mut st = bot_db::GuildSettings::default();
    assert_eq!(svc::voice_lead(None), 30);
    st.event_voice_lead_minutes = 45;
    assert_eq!(svc::voice_lead(Some(&st)), 45);
    for v in r["voiceAt"].as_array().into_iter().flatten() {
        let start = chrono::DateTime::from_timestamp_millis(iso_ms(&v["startsAt"])).unwrap_or_default();
        assert_eq!(iso(&svc::voice_at(&start, v["lead"].as_i64().unwrap_or(0), now)), s(&v["out"]));
    }
    for e in r["ends"].as_array().into_iter().flatten() {
        let ev = event_of(&e["event"]);
        assert_eq!(iso(&svc::event_end(&ev)), s(&e["end"]));
        assert_eq!(iso(&svc::shown_end(&ev)), s(&e["shown"]));
        assert_eq!(svc::remind_at(&ev).map(|d| iso(&d)), e["remind"].as_str().map(String::from));
    }
    for v in r["remindAt"].as_array().into_iter().flatten() {
        let start = chrono::DateTime::from_timestamp_millis(now).unwrap_or_default();
        assert_eq!(svc::remind_at_of(&start, v["m"].as_i64().unwrap_or(0)).map(|d| iso(&d)), v["out"].as_str().map(String::from));
    }
    for v in r["startProblem"].as_array().into_iter().flatten() {
        let got = match svc::start_problem(v["ms"].as_i64(), now) {
            None => Value::Null,
            Some(svc::StartProblem::Past) => json!("past"),
            Some(svc::StartProblem::Far) => json!("far"),
        };
        assert_eq!(got, v["out"]);
    }
    assert_eq!(svc::start_problem(None, now), Some(svc::StartProblem::Past), "an invalid date is in the past");
    for v in r["textChannelName"].as_array().into_iter().flatten() {
        assert_eq!(svc::text_channel_name(&s(&v["title"])), s(&v["out"]));
    }
    for v in r["calendarFileName"].as_array().into_iter().flatten() {
        assert_eq!(svc::calendar_file_name(&s(&v["title"])), s(&v["out"]));
    }
    for v in r["jumpUrl"].as_array().into_iter().flatten() {
        assert_eq!(svc::jump_url(&event_of(&v["event"])), s(&v["out"]));
    }
    let t = svc::tally_from_groups(&[("GOING".into(), 20), ("MAYBE".into(), 3), ("DECLINED".into(), 2)], vec!["a".into()]);
    assert_eq!(json!({ "going": t.going, "maybe": t.maybe, "declined": t.declined, "goingIds": t.going_ids }), r["tallyFromGroups"]);
    for v in r["discordEventOptions"].as_array().into_iter().flatten() {
        let o = svc::discord_event_options(&event_of(&v["event"]));
        let want = &v["out"];
        assert_eq!(o.name, s(&want["name"]));
        assert_eq!(o.description, want["description"].as_str().map(String::from));
        assert_eq!(iso(&o.scheduled_start_time), s(&want["scheduledStartTime"]));
        assert_eq!(iso(&o.scheduled_end_time), s(&want["scheduledEndTime"]));
        assert_eq!((json!(o.privacy_level), json!(o.entity_type)), (want["privacyLevel"].clone(), want["entityType"].clone()));
        assert_eq!(o.location, s(&want["entityMetadata"]["location"]));
    }
    for (kind, allow) in [("text", &svc::TEXT_ALLOW), ("voice", &svc::VOICE_ALLOW)] {
        let ows = svc::event_overwrites("900000000000000777", "810000000000000077", "100000000000000002", allow);
        for (o, js) in ows.iter().zip(r["overwrites"][kind].as_array().into_iter().flatten()) {
            assert_eq!(o.id, s(&js["id"]));
            assert_eq!(json!(o.kind), js["type"]);
            assert_eq!(o.allow.bits().to_string(), js["allow"].as_str().unwrap_or("0"));
            assert_eq!(o.deny.bits().to_string(), js["deny"].as_str().unwrap_or("0"));
        }
    }
    // RAW_SQL: the same statements (Prisma prints `?` where Rust binds `$1`).
    let raw = svc::raw_sql();
    for (rs, js) in raw.iter().zip(r["rawSql"].as_array().into_iter().flatten()) {
        assert_eq!(rs.0, s(&js["name"]));
        assert_eq!(rs.1.replace("$1", "?"), s(&js["sql"]));
        assert_eq!(json!(rs.2), js["values"]);
    }
}

mod events_db;

// ── Offline: the render throttle ───────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn request_render_renders_at_once_collapses_a_burst_and_cancel_drops_it() {
    use std::sync::{Arc, Mutex};
    let t = bot_core::testing::TestApp::new();
    let renders: Arc<Mutex<Vec<String>>> = Arc::default();
    let r2 = renders.clone();
    svc::set_render_impl(
        &t,
        Some(Arc::new(move |_app, id| {
            let r = r2.clone();
            Box::pin(async move {
                r.lock().unwrap_or_else(|e| e.into_inner()).push(id);
                Ok(svc::RenderOutcome::Ok)
            })
        })),
    );
    let count = || renders.lock().unwrap_or_else(|e| e.into_inner()).len();
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }
    svc::request_render(&t, "E1");
    settle().await;
    assert_eq!(count(), 1, "the first request renders at once");
    for _ in 0..5 {
        svc::request_render(&t, "E1");
    }
    settle().await;
    assert_eq!(count(), 1, "a burst waits for the window");
    t.clock.advance(5_000);
    tokio::time::advance(std::time::Duration::from_millis(5_000)).await;
    settle().await;
    assert_eq!(count(), 2, "and collapses into one trailing render");
    t.clock.advance(1_000);
    svc::request_render(&t, "E1");
    svc::cancel_render(&t, "E1");
    t.clock.advance(5_000);
    tokio::time::advance(std::time::Duration::from_millis(5_000)).await;
    settle().await;
    assert_eq!(count(), 2, "cancel drops it");
    svc::render_now(&t, "E2").await.expect("render");
    assert_eq!(count(), 3);
}
