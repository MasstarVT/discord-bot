//! Replays every `[args, result]` pair the JS produced (rust/fixtures/golden/bot-util/tables.json from
//! bot-util-tables.mjs, recorded.json from running the JS test suite with bot-util-record-hooks.mjs) against the Rust
//! port. Every pair must match exactly; the only cases skipped are ones a Rust `String` cannot represent (a lone
//! surrogate in the input or the expected output) and genuinely random placeholder output.

mod common;

use bot_util::calendar::{self, CalendarEntry};
use bot_util::placeholder_parser::{self as ph, PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser, RenderContext, VarValue};
use bot_util::safe_regex::{self, MatchOptions, Quarantine, SafeRegex};
use bot_util::{fatal_gateway, image_size, parse_duration as dur, restart_brake, safe_fetch, text, time, time_mentions as tm, timezones as tzs};
use common::*;
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn s(v: &Value) -> String {
    string_of(v)
}

/// `String(v ?? '')`.
fn s_or_empty(v: &Value) -> String {
    if v.is_null() || is_undef(v) { String::new() } else { string_of(v) }
}

fn opt_s(v: Option<Value>) -> Option<String> {
    v.filter(|x| !x.is_null() && !is_undef(x)).map(|x| string_of(&x))
}

fn opt_num_typed(v: Option<Value>) -> Option<f64> {
    v.filter(is_number).map(|x| num(&x))
}

fn date_of(ms: f64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms as i64).expect("date in range")
}

fn opts_obj(args: &[Value], i: usize) -> Map<String, Value> {
    args.get(i).and_then(Value::as_object).cloned().unwrap_or_default()
}

fn now_of(o: &Map<String, Value>, key: &str) -> f64 {
    o.get(key).map(ms_of).unwrap_or(f64::NAN)
}

// ── per-module dispatch ────────────────────────────────────────────────────────

fn text_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "clampText" => {
            let v = arg(a, 0);
            if v.is_null() || is_undef(&v) {
                return Some(json!(""));
            }
            let ell = match a.get(2) {
                None => "…".to_string(),
                Some(e) if is_undef(e) => "…".to_string(),
                Some(e) if e.is_null() => String::new(),
                Some(e) => s(e),
            };
            json!(text::clamp_text_f64(&s(&v), num(&arg(a, 1)), &ell))
        }
        "chunkLines" => {
            let Some(lines) = arg(a, 0).as_array().cloned() else { return Some(json!([])) };
            let max = if a.len() > 1 && !is_undef(&a[1]) { num(&a[1]) as usize } else { 1024 };
            let strs: Vec<String> = lines.iter().map(s).collect();
            json!(text::chunk_lines(&strs, max))
        }
        "joinWithLimit" => {
            let items = arg(a, 0);
            let list: Vec<String> = match &items {
                Value::Array(x) => x.iter().map(s).collect(),
                Value::String(st) => st.chars().map(|c| c.to_string()).collect(),
                _ => Vec::new(),
            };
            let o = opts_obj(a, 1);
            let sep = o.get("sep").map(s).unwrap_or_else(|| ", ".into());
            let max = o.get("max").map(|m| num(m) as usize).unwrap_or(1024);
            json!(text::join_with_limit(&list, &sep, max, text::default_more))
        }
        "formatNumber" => {
            let v = arg(a, 0);
            if let Some(b) = v.get("$big").and_then(Value::as_str) {
                return Some(json!(text::format_bigint(b.parse().ok()?)));
            }
            json!(text::format_number(num(&v)))
        }
        "ordinal" => {
            let v = arg(a, 0);
            let n = num(&v);
            if !n.trunc().is_finite() {
                return Some(json!(s(&v)));
            }
            json!(text::ordinal(n))
        }
        "compactNumber" => json!(text::compact_number(num(&arg(a, 0)))),
        "progressBar" => {
            let width = if a.len() > 1 && !is_undef(&a[1]) { num(&a[1]) } else { 10.0 };
            let o = opts_obj(a, 2);
            let filled = o.get("filled").filter(|v| !is_undef(v)).map(s).unwrap_or_else(|| "▰".into());
            let empty = o.get("empty").filter(|v| !is_undef(v)).map(s).unwrap_or_else(|| "▱".into());
            json!(text::progress_bar(num(&arg(a, 0)), width, &filled, &empty))
        }
        "sanitizeName" => {
            let o = opts_obj(a, 1);
            let max = o.get("max").map(|m| num(m) as usize).unwrap_or(32);
            let fallback = o.get("fallback").map(s).unwrap_or_else(|| "item".into());
            json!(text::sanitize_name(&s_or_empty(&arg(a, 0)), max, &fallback))
        }
        "percentages" => {
            let list: Vec<f64> = arg(a, 0).as_array().map(|x| x.iter().map(num).collect()).unwrap_or_default();
            json!(text::percentages(&list))
        }
        _ => return None,
    })
}

fn duration_call(f: &str, a: &[Value]) -> Option<Value> {
    let v = arg(a, 0);
    Some(match f {
        "parseDuration" => match as_string(&v) {
            None => Value::Null,
            Some(st) => dur::parse_duration(&st).map_or(Value::Null, |n| json!(n)),
        },
        "formatDuration" => json!(if truthy(&v) { dur::format_duration(num(&v)) } else { "Permanent".into() }),
        "formatDurationLong" => json!(if truthy(&v) && is_number(&v) { dur::format_duration_long(num(&v)) } else { "Permanent".into() }),
        _ => return None,
    })
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    use base64::Engine;
    let b = v.get("$buf")?.as_str()?;
    base64::engine::general_purpose::STANDARD.decode(b).ok()
}

fn size_json(sz: &image_size::ImageSize) -> Value {
    json!({"type": sz.kind.as_str(), "width": sz.width, "height": sz.height})
}

fn image_call(f: &str, a: &[Value]) -> Option<Value> {
    let buf = bytes_of(&arg(a, 0));
    Some(match f {
        "readImageSize" => buf.and_then(|b| image_size::read_image_size(&b)).map_or(Value::Null, |sz| size_json(&sz)),
        "assertImageDimensions" => {
            let o = opts_obj(a, 1);
            let side = o.get("maxSide").map_or(image_size::DEFAULT_MAX_SIDE as f64, num);
            let px = o.get("maxPixels").map_or(image_size::DEFAULT_MAX_PIXELS as f64, num);
            match buf.and_then(|b| image_size::assert_image_dimensions(&b, side as u64, px as u64).ok()) {
                Some(sz) => size_json(&sz),
                None => json!({"$throw": "IMAGE_TOO_LARGE_OR_UNKNOWN"}),
            }
        }
        _ => return None,
    })
}

fn entry_of(v: &Value) -> CalendarEntry {
    CalendarEntry {
        uid: s_or_empty(&v["uid"]),
        title: s_or_empty(v.get("title").unwrap_or(&Value::Null)),
        description: opt_s(opt_field(v, "description")),
        location: opt_s(opt_field(v, "location")),
        start: date_of(ms_of(&v["start"])),
        end: date_of(ms_of(&v["end"])),
        url: opt_s(opt_field(v, "url")),
        sequence: opt_field(v, "sequence").filter(Value::is_number).map(|x| num(&x)),
    }
}

fn calendar_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "fitEncoded" => json!(calendar::fit_encoded(&s_or_empty(&arg(a, 0)), num(&arg(a, 1)) as i64)),
        "googleCalendarUrl" => {
            let max = a.get(1).filter(|v| !is_undef(v)).map_or(calendar::LINK_URL_MAX, |m| num(m) as usize);
            json!(calendar::google_calendar_url(&entry_of(&arg(a, 0)), max))
        }
        "outlookCalendarUrl" => {
            let max = a.get(1).filter(|v| !is_undef(v)).map_or(calendar::LINK_URL_MAX, |m| num(m) as usize);
            json!(calendar::outlook_calendar_url(&entry_of(&arg(a, 0)), max))
        }
        "icsEscape" => json!(calendar::ics_escape(&s_or_empty(&arg(a, 0)))),
        "icsFold" => json!(calendar::ics_fold(&s(&arg(a, 0)))),
        "buildIcs" => {
            let o = opts_obj(a, 1);
            let prod = o.get("prodId").map(s).unwrap_or_else(|| calendar::DEFAULT_PROD_ID.into());
            json!(calendar::build_ics(&entry_of(&arg(a, 0)), date_of(now_of(&o, "now")), &prod))
        }
        _ => return None,
    })
}

/// A zone argument: a string, or something Intl would refuse (an empty id).
fn zone_arg(v: &Value) -> String {
    as_string(v).unwrap_or_default()
}

fn parts_json(p: &time::ZonedParts) -> Value {
    json!({"year": p.year, "month": p.month, "day": p.day, "hour": p.hour, "minute": p.minute, "second": p.second, "weekday": p.weekday})
}

fn weekdays_of(v: Option<&Value>) -> Vec<f64> {
    v.and_then(Value::as_array).map(|a| a.iter().map(|x| if x.is_number() { num(x) } else { f64::NAN }).collect()).unwrap_or_default()
}

fn when_json(p: Option<time::ParsedWhen>) -> Value {
    p.map_or(Value::Null, |p| json!({"date": enc_date(p.date_ms), "zone": p.zone, "zoneFromInput": p.zone_from_input}))
}

fn time_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "isValidTimeZone" => json!(as_string(&arg(a, 0)).is_some_and(|z| time::is_valid_time_zone(&z))),
        "zonedParts" => parts_json(&time::zoned_parts_ms(ms_of(&arg(a, 0)), &zone_arg(&arg(a, 1)))),
        "localDateKey" => json!(time::local_date_key_ms(ms_of(&arg(a, 0)), &zone_arg(&arg(a, 1)))),
        "zonedToUtc" => {
            let l = arg(a, 0);
            let field = |k: &str, d: f64| l.get(k).filter(|v| !is_undef(v)).map_or(d, num);
            enc_date(time::zoned_to_utc_ms(field("year", f64::NAN), field("month", f64::NAN), field("day", f64::NAN), field("hour", 0.0), field("minute", 0.0), &zone_arg(&arg(a, 1))))
        }
        "daysInMonth" => enc_num(time::days_in_month_f64(num(&arg(a, 0)), num(&arg(a, 1)))),
        "parseTimeOfDay" => time::parse_time_of_day(&s_or_empty(&arg(a, 0))).map_or(Value::Null, |t| json!({"hour": t.hour, "minute": t.minute})),
        "formatTimeOfDay" => {
            let o = arg(a, 0);
            json!(time::format_time_of_day(time::TimeOfDay { hour: num(&o["hour"]) as u32, minute: num(&o["minute"]) as u32 }))
        }
        "parseWeekdays" => time::parse_weekdays(&s_or_empty(&arg(a, 0))).map_or(Value::Null, |d| json!(d)),
        "nextTimeOfDay" => {
            let o = opts_obj(a, 0);
            let after = now_of(&o, "after");
            enc_date(time::next_time_of_day_ms(&zone_arg(&o["tz"]), num(&o["hour"]), num(&o["minute"]), &weekdays_of(o.get("weekdays")), after))
        }
        "nextIntervalRun" => {
            let o = opts_obj(a, 0);
            let tz = zone_arg(&o["tz"]);
            let ws = o.get("windowStart").and_then(as_string);
            let we = o.get("windowEnd").and_then(as_string);
            let im = o.get("intervalMinutes").filter(|v| v.is_number()).map_or(f64::NAN, num);
            let anchor = o.get("anchor").filter(|v| !v.is_null() && !is_undef(v)).map(ms_of);
            let opts = time::IntervalOptions {
                tz: &tz,
                interval_minutes: im,
                weekdays: weekdays_of(o.get("weekdays")),
                window_start: ws.as_deref(),
                window_end: we.as_deref(),
                after_ms: now_of(&o, "after"),
                anchor_ms: anchor,
            };
            time::next_interval_run_ms(&opts).map_or(Value::Null, enc_date)
        }
        "parseDateInput" => {
            let o = opts_obj(a, 2);
            time::parse_date_input_ms(&s_or_empty(&arg(a, 0)), &zone_arg(&arg(a, 1)), now_of(&o, "now")).map_or(Value::Null, enc_date)
        }
        "parseBirthday" => {
            let o = opts_obj(a, 1);
            let req = o.get("requireYear").is_some_and(truthy);
            time::parse_birthday_ms(&s_or_empty(&arg(a, 0)), req, now_of(&o, "now"))
                .map_or(Value::Null, |b| json!({"month": b.month, "day": b.day, "year": b.year}))
        }
        "discordTimestamp" => {
            let style = a.get(1).filter(|v| !is_undef(v)).map_or_else(|| "f".to_string(), |v| as_string(v).unwrap_or_default());
            json!(time::discord_timestamp_ms(ms_of(&arg(a, 0)), &style))
        }
        "zoneFromAbbreviation" => time::zone_from_abbreviation(&s_or_empty(&arg(a, 0))).map_or(Value::Null, |z| json!(z)),
        "zoneFromOffset" => time::zone_from_offset(&s_or_empty(&arg(a, 0))).map_or(Value::Null, |z| json!(z)),
        "parseWhen" => {
            let o = opts_obj(a, 2);
            when_json(time::parse_when_ms(&s_or_empty(&arg(a, 0)), &zone_arg(&arg(a, 1)), now_of(&o, "now")))
        }
        _ => return None,
    })
}

fn zone_err_json(e: &tzs::ZoneInputError) -> Value {
    let mut m = Map::new();
    m.insert("ok".into(), json!(false));
    m.insert("code".into(), json!(e.code.as_str()));
    if let Some(o) = &e.options {
        m.insert("options".into(), json!(o));
    }
    Value::Object(m)
}

fn timezones_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "listTimeZones" => json!(tzs::list_time_zones()),
        "normalizeTimeZone" => match tzs::normalize_time_zone(&s_or_empty(&arg(a, 0))) {
            Ok(z) => json!({"ok": true, "zone": z}),
            Err(c) => json!({"ok": false, "code": c.as_str()}),
        },
        "parseZoneInput" => match tzs::parse_zone_input(&s_or_empty(&arg(a, 0))) {
            Ok(p) => json!({"ok": true, "zone": p.zone, "abbreviation": p.abbreviation}),
            Err(e) => zone_err_json(&e),
        },
        "utcOffsetLabel" => json!(tzs::utc_offset_label_ms(&s(&arg(a, 0)), ms_of(&arg(a, 1)))),
        "zoneLabel" => json!(tzs::zone_label_ms(&s(&arg(a, 0)), ms_of(&arg(a, 1)))),
        "describeLocalTime" => {
            let d = tzs::describe_local_time_ms(&zone_arg(&arg(a, 0)), ms_of(&arg(a, 1)));
            json!({"valid": d.valid && as_string(&arg(a, 0)).is_some(), "zone": d.zone, "time": d.time, "date": d.date, "weekday": d.weekday, "offset": d.offset})
        }
        "formatInZone" => {
            json!(tzs::format_in_zone_ms(ms_of(&arg(a, 0)), &zone_arg(&arg(a, 1))))
        }
        "searchTimeZones" => {
            let o = opts_obj(a, 1);
            let q = s_or_empty(&arg(a, 0));
            let current = o.get("current").and_then(as_string);
            let opts = tzs::SearchOptions {
                current: current.as_deref(),
                now_ms: now_of(&o, "now"),
                limit: o.get("limit").map_or(25, |l| num(l) as i64),
                abbreviations: o.get("abbreviations").is_some_and(truthy),
            };
            let choices = tzs::search_time_zones(&q, &opts);
            json!(choices.iter().map(|c| json!({"name": c.name, "value": c.value})).collect::<Vec<_>>())
        }
        "zoneFromRoleName" => tzs::zone_from_role_name(&s_or_empty(&arg(a, 0))).map_or(Value::Null, |z| json!(z)),
        "normalizeTimezoneRoles" => {
            let roles = tzs::normalize_timezone_roles(&arg(a, 0));
            json!(roles.iter().map(|r| json!({"roleId": r.role_id, "zone": r.zone})).collect::<Vec<_>>())
        }
        "zoneSourceText" => {
            let r = arg(a, 0);
            let o = opts_obj(a, 1);
            let role = r.get("roleId").filter(|v| !v.is_null()).map(s);
            json!(tzs::zone_source_text(&s(&r["source"]), role.as_deref(), o.get("zoneFromInput").is_some_and(truthy)))
        }
        "zoneInputError" => {
            let r = arg(a, 1);
            let options: Vec<String> = r.get("options").and_then(Value::as_array).map(|x| x.iter().map(s).collect()).unwrap_or_default();
            let (t, b) = tzs::zone_input_error(&s_or_empty(&arg(a, 0)), &s(&r["code"]), &options);
            json!([t, b])
        }
        _ => return None,
    })
}

fn span_of(v: &Value) -> tm::TimeSpan {
    tm::TimeSpan {
        text: s(&v["text"]),
        day: v.get("day").and_then(as_string),
        time: s(&v["time"]),
        zone: v.get("zone").and_then(as_string),
    }
}

fn mentions_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "mayMentionTime" => json!(as_string(&arg(a, 0)).is_some_and(|c| tm::may_mention_time(&c))),
        "extractTimeSpans" => {
            let Some(c) = as_string(&arg(a, 0)) else { return Some(json!([])) };
            let max = opts_obj(a, 1).get("max").map_or(10, |m| num(m) as usize);
            json!(tm::extract_time_spans(&c, max).iter().map(|sp| json!({"text": sp.text, "day": sp.day, "time": sp.time, "zone": sp.zone})).collect::<Vec<_>>())
        }
        "readTimeSpans" => {
            let spans: Vec<tm::TimeSpan> = arg(a, 0).as_array().map(|x| x.iter().map(span_of).collect()).unwrap_or_default();
            let zone = as_string(&arg(a, 1));
            let o = opts_obj(a, 2);
            let grace = o.get("pastGraceMs").map_or(3_600_000.0, num);
            let max = o.get("max").map_or(3, |m| num(m) as usize);
            let out = tm::read_time_spans(&spans, zone.as_deref(), now_of(&o, "now"), grace, max);
            json!(out.iter().map(|r| json!({"text": r.text, "date": enc_date(r.date_ms), "zone": r.zone, "zoneFromInput": r.zone_from_input})).collect::<Vec<_>>())
        }
        _ => return None,
    })
}

fn regex_of(v: &Value) -> Option<Arc<SafeRegex>> {
    let src = v.get("$re")?.as_str()?;
    let flags = v.get("flags")?.as_str()?;
    Some(Arc::new(SafeRegex::new(src, flags).unwrap_or_else(|e| panic!("regress refused /{src}/{flags}: {e}"))))
}

fn flags_arg(a: &[Value], i: usize) -> String {
    match a.get(i) {
        None => "i".into(),
        Some(v) if is_undef(v) => "i".into(),
        Some(v) => as_string(v).unwrap_or_default(),
    }
}

fn match_opts(a: &[Value], i: usize) -> MatchOptions {
    let o = opts_obj(a, i);
    // The deadline itself is tested in safe_regex.rs; replaying values must not flake on a slow debug build.
    MatchOptions { timeout_ms: 5_000, input_cap: o.get("inputCap").map_or(safe_regex::DEFAULT_INPUT_CAP, |c| num(c) as usize) }
}

fn regex_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "validateRegex" => {
            let Some(p) = as_string(&arg(a, 0)) else { return Some(json!({"ok": false, "reason": safe_regex::NOT_TEXT_REASON})) };
            match safe_regex::validate_regex(&p, &flags_arg(a, 1)) {
                Ok(()) => json!({"ok": true}),
                Err(r) => json!({"ok": false, "reason": r}),
            }
        }
        "compileSafeRegex" => {
            let Some(p) = as_string(&arg(a, 0)) else { return Some(Value::Null) };
            safe_regex::compile_safe_regex(&p, &flags_arg(a, 1)).map_or(Value::Null, |r| json!({"$re": r.source(), "flags": r.flags()}))
        }
        "escapeRegex" => json!(safe_regex::escape_regex(&s_or_empty(&arg(a, 0)))),
        "containsWord" => {
            let cs = opts_obj(a, 2).get("caseSensitive").is_some_and(truthy);
            json!(safe_regex::contains_word(&s_or_empty(&arg(a, 0)), &s_or_empty(&arg(a, 1)), cs))
        }
        "safeRegexTest" => {
            let Some(re) = regex_of(&arg(a, 0)) else { return Some(json!(false)) };
            let q = Quarantine::new();
            json!(safe_regex::safe_regex_test_in(&q, &re, &s_or_empty(&arg(a, 1)), Some(match_opts(a, 2))))
        }
        "safeRegexFirstMatch" => {
            let list: Vec<Option<Arc<SafeRegex>>> = arg(a, 0).as_array().map(|x| x.iter().map(regex_of).collect()).unwrap_or_default();
            let q = Quarantine::new();
            json!(safe_regex::safe_regex_first_match_in(&q, &list, &s_or_empty(&arg(a, 1)), Some(match_opts(a, 2))))
        }
        _ => return None,
    })
}

fn user_of(v: &Value) -> Option<PlaceholderUser> {
    if !v.is_object() {
        return None;
    }
    Some(PlaceholderUser {
        id: s(&v["id"]),
        username: opt_s(opt_field(v, "username")),
        tag: opt_field(v, "tag").and_then(|t| as_string(&t)),
        discriminator: opt_field(v, "discriminator").filter(truthy).map(|d| s(&d)),
        global_name: opt_s(opt_field(v, "globalName")),
        bot: v.get("bot").is_some_and(truthy),
        created_timestamp: opt_num_typed(opt_field(v, "createdTimestamp")),
        avatar_url: opt_s(opt_field(v, "avatarURL")),
    })
}

fn guild_of(v: &Value) -> Option<PlaceholderGuild> {
    if !v.is_object() {
        return None;
    }
    Some(PlaceholderGuild {
        id: opt_s(opt_field(v, "id")).unwrap_or_default(),
        name: opt_s(opt_field(v, "name")),
        owner_id: opt_field(v, "ownerId").filter(|x| !x.is_null()).map(|x| if truthy(&x) { s(&x) } else { String::new() }),
        member_count: opt_field(v, "memberCount").filter(|x| !x.is_null()).map(|x| num(&x)),
        verification_level: opt_s(opt_field(v, "verificationLevel")),
        joined_timestamp: opt_num_typed(opt_field(v, "joinedTimestamp")),
        created_timestamp: opt_num_typed(opt_field(v, "createdTimestamp")),
        premium_subscription_count: opt_s(opt_field(v, "premiumSubscriptionCount")),
        premium_tier: opt_s(opt_field(v, "premiumTier")),
        icon_url: opt_s(opt_field(v, "iconURL")),
    })
}

fn ctx_of(v: &Value) -> RenderContext {
    if v.get("$ctx").is_none() {
        return RenderContext::default();
    }
    let member = v.get("member").filter(|m| m.is_object()).map(|m| PlaceholderMember {
        display_name: opt_s(opt_field(m, "displayName")),
        joined_timestamp: opt_num_typed(opt_field(m, "joinedTimestamp")),
        avatar_url: opt_s(opt_field(m, "avatarURL")),
        user: m.get("user").and_then(user_of),
        guild: m.get("guild").and_then(guild_of),
    });
    let vars = v.get("vars").and_then(Value::as_object).map(|o| {
        o.iter()
            .map(|(k, x)| {
                let val = if let Some(r) = x.get("$fnres") {
                    let r = r.as_str().map(str::to_string);
                    VarValue::Lazy(Box::new(move || Ok(r.clone())))
                } else if let Some(e) = x.get("$fnthrow") {
                    let e = s(e);
                    VarValue::Lazy(Box::new(move || Err(e.clone())))
                } else if x.is_null() || is_undef(x) {
                    VarValue::Null
                } else if let Some(b) = x.as_bool() {
                    VarValue::Bool(b)
                } else if is_number(x) {
                    VarValue::Number(num(x))
                } else {
                    VarValue::Text(s(x))
                };
                (k.clone(), val)
            })
            .collect()
    });
    RenderContext {
        member,
        user: v.get("user").and_then(user_of),
        guild: v.get("guild").and_then(guild_of),
        channel: v.get("channel").filter(|c| c.is_object()).map(|c| PlaceholderChannel {
            id: s(&c["id"]),
            name: opt_s(opt_field(c, "name")),
            kind: opt_field(c, "type").filter(Value::is_number).map(|t| num(&t) as i64),
        }),
        vars,
        args: v.get("args").and_then(Value::as_array).map(|x| x.iter().map(s).collect()),
    }
}

fn template_of(v: &Value) -> Option<String> {
    if v.is_null() || is_undef(v) {
        Some(String::new())
    } else {
        Some(s(v))
    }
}

fn is_random(t: &str) -> bool {
    let lower = t.to_lowercase();
    // {random} and a {random:x:y} with x != y are random; {random:5:5} is not.
    let re = bot_util::jsre::JsRegex::new(r"\{\s*random\s*\}|\{random:(-?\d{1,10}):(-?\d{1,10})\}", "i");
    let u = bot_util::jsre::u16s(&lower);
    re.find_all(&u).iter().any(|m| match (m.group(&u, 1), m.group(&u, 2)) {
        (Some(x), Some(y)) => x.parse::<f64>().ok() != y.parse::<f64>().ok(),
        _ => true,
    })
}

fn max_length_of(a: &[Value], i: usize) -> Option<usize> {
    let o = opts_obj(a, i);
    o.get("maxLength").filter(|m| m.as_f64().is_some_and(|x| x.fract() == 0.0 && x > 0.0)).map(|m| num(m) as usize)
}

fn placeholder_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "renderPlaceholdersDetailed" | "renderPlaceholders" | "parsePlaceholders" => {
            let t = template_of(&arg(a, 0))?;
            let ctx = ctx_of(&arg(a, 1));
            let r = ph::render_placeholders_detailed(&t, &ctx, max_length_of(a, 2));
            if f == "renderPlaceholdersDetailed" {
                json!({"text": r.text, "missingArgs": r.missing_args, "unknownTokens": r.unknown_tokens})
            } else {
                json!(r.text)
            }
        }
        "placeholderHelp" => {
            let groups: Vec<String> = match a.first().and_then(Value::as_array) {
                Some(g) => g.iter().filter_map(as_string).collect(),
                None => vec!["user".into(), "server".into(), "channel".into()],
            };
            let refs: Vec<&str> = groups.iter().map(String::as_str).collect();
            json!(ph::placeholder_help(&refs))
        }
        _ => return None,
    })
}

fn misc_call(module: &str, f: &str, a: &[Value]) -> Option<Value> {
    Some(match (module, f) {
        ("safeFetch", "isBlockedAddress") => json!(safe_fetch::is_blocked_address(&s_or_empty(&arg(a, 0)))),
        ("fatalGateway", "fatalCodeFromError") => {
            let e = arg(a, 0);
            if !truthy(&e) {
                return Some(Value::Null);
            }
            let code = e.get("code").and_then(as_string);
            let message = e.get("$err").or_else(|| e.get("message")).and_then(as_string);
            fatal_gateway::fatal_code_from_error(code.as_deref(), message.as_deref()).map_or(Value::Null, |c| json!(c))
        }
        ("restartBrake", "brakeDelayMs") => enc_num(restart_brake::brake_delay_ms_f64(num(&arg(a, 0)))),
        _ => return None,
    })
}

fn extra_call(f: &str, a: &[Value]) -> Option<Value> {
    Some(match f {
        "joinWithLimit" => {
            let list: Vec<String> = arg(a, 0).as_array().map(|x| x.iter().map(s).collect()).unwrap_or_default();
            let o = opts_obj(a, 1);
            let tpl = s(&arg(a, 2));
            let more = |n: usize| tpl.replacen("{n}", &n.to_string(), 1);
            json!(text::join_with_limit(&list, &s(&o["sep"]), num(&o["max"]) as usize, more))
        }
        "effectiveText" => {
            let content = s_or_empty(&arg(a, 0));
            let snaps: Vec<String> = arg(a, 1).as_array().map(|x| x.iter().filter(|v| !v.is_null() && !is_undef(v)).map(s).collect()).unwrap_or_default();
            json!(text::effective_text(Some(&content), snaps.iter().map(|x| Some(x.as_str()))))
        }
        "resolveTimezone" => {
            let o = opts_obj(a, 0);
            let personal = o.get("personal").and_then(as_string);
            let ids: Vec<String> = o.get("memberRoleIds").and_then(Value::as_array).map(|x| x.iter().map(s).collect()).unwrap_or_default();
            let positions = o.get("positions").and_then(Value::as_object).cloned().unwrap_or_default();
            let settings = o.get("settings").filter(|v| v.is_object()).cloned();
            let roles = settings.as_ref().and_then(|st| st.get("timezoneRoles").cloned()).unwrap_or(Value::Null);
            let tz = settings.as_ref().and_then(|st| st.get("timezone").and_then(as_string));
            let r = tzs::resolve_timezone(
                personal.as_deref(),
                &ids,
                |id| positions.get(id).map_or(0, |p| num(p) as i64),
                settings.as_ref().map(|_| (tz.as_deref(), &roles)),
            );
            json!({"zone": r.zone, "source": r.source.as_str(), "roleId": r.role_id, "roleCount": r.role_count})
        }
        "formatError" => {
            let code = a.get(1).filter(|v| !is_undef(v)).cloned();
            let ctx = opts_obj(a, 2);
            let mut m = bot_util::error_formatter::format_error_parts(&s(&arg(a, 0)), "Error", Some("Error: stack"), code, &ctx);
            let has = m.shift_remove("stack").is_some_and(|v| v.is_string());
            m.insert("hasStack".into(), json!(has));
            Value::Object(m)
        }
        "formatNonError" => Value::Object(bot_util::error_formatter::format_non_error(&s(&arg(a, 0)), &opts_obj(a, 1))),
        _ => return None,
    })
}

fn dispatch(key: &str, args: &[Value]) -> Option<Value> {
    let (module, f) = key.split_once('.')?;
    match module {
        "text" => text_call(f, args),
        "parseDuration" => duration_call(f, args),
        "imageSize" => image_call(f, args),
        "calendar" => calendar_call(f, args),
        "time" => time_call(f, args),
        "timezones" => timezones_call(f, args),
        "timeMentions" => mentions_call(f, args),
        "safeRegex" => regex_call(f, args),
        "placeholderParser" => placeholder_call(f, args),
        "extra" => extra_call(f, args),
        _ => misc_call(module, f, args),
    }
}

/// Cases a port cannot replay on purpose: random output, and formatInZone of an invalid date (the JS throws).
fn deliberately_skipped(key: &str, args: &[Value]) -> bool {
    match key {
        "placeholderParser.renderPlaceholdersDetailed" | "placeholderParser.renderPlaceholders" | "placeholderParser.parsePlaceholders" => {
            template_of(&arg(args, 0)).is_some_and(|t| is_random(&t))
        }
        "timezones.formatInZone" => !bot_util::js::time_clip(ms_of(&arg(args, 0))).is_finite(),
        _ => false,
    }
}

/// Lone surrogates survive only where the JS turns them into U+FFFD itself (calendar's toWellFormed).
fn representable(key: &str, args: &[Value], expected: &Value) -> bool {
    if has_u16(expected) {
        return false;
    }
    key.starts_with("calendar.") || !args.iter().any(has_u16)
}

fn replay(prefix: &str) {
    let keys: Vec<String> = all_keys().into_iter().filter(|k| k.starts_with(prefix)).collect();
    assert!(!keys.is_empty(), "no golden cases for {prefix}");
    let mut failures = Vec::new();
    let mut total = 0;
    let mut skipped = 0;
    for key in &keys {
        let mut key_skipped = 0;
        for (args, expected, src) in cases(key) {
            if !representable(key, &args, &expected) || deliberately_skipped(key, &args) {
                skipped += 1;
                key_skipped += 1;
                continue;
            }
            match dispatch(key, &args) {
                None => panic!("no Rust port is wired for {key} (args {})", Value::Array(args.clone())),
                Some(actual) => {
                    total += 1;
                    if !same(&actual, &expected) {
                        failures.push(format!("{key} [{src}] args={}\n   expected {}\n   actual   {}", Value::Array(args.clone()), expected, actual));
                    }
                }
            }
        }
        if key_skipped > 0 {
            eprintln!("  {key}: {key_skipped} skipped (lone surrogate, random output or an invalid date)");
        }
    }
    eprintln!("{prefix}: {total} cases replayed, {skipped} skipped, {} failed", failures.len());
    if !failures.is_empty() {
        let shown: Vec<&String> = failures.iter().take(40).collect();
        panic!("{} of {total} {prefix} cases differ from the JS:\n{}", failures.len(), shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"));
    }
}

#[test]
fn golden_text() {
    replay("text.");
}

#[test]
fn golden_parse_duration() {
    replay("parseDuration.");
}

#[test]
fn golden_image_size() {
    replay("imageSize.");
}

#[test]
fn golden_calendar() {
    replay("calendar.");
}

#[test]
fn golden_time() {
    replay("time.");
}

#[test]
fn golden_timezones() {
    replay("timezones.");
}

#[test]
fn golden_time_mentions() {
    replay("timeMentions.");
}

#[test]
fn golden_safe_regex() {
    replay("safeRegex.");
}

#[test]
fn golden_placeholders() {
    replay("placeholderParser.");
}

#[test]
fn golden_safe_fetch_addresses() {
    replay("safeFetch.");
}

#[test]
fn golden_fatal_gateway() {
    replay("fatalGateway.");
}

#[test]
fn golden_extra() {
    replay("extra.");
}

#[test]
fn golden_restart_brake() {
    replay("restartBrake.");
}
