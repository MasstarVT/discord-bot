//! Port of `src/dashboard/collections/reminders.js`: the `reminders` collection (Reminder rows behind /reminder) and its
//! schedule helpers.

use bot_util::js::number_to_string;
use bot_util::parse_duration::format_duration_long;
use bot_util::time::WEEKDAY_NAMES;
use serde_json::{Map, Value, json};

use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, date_ms, int_of, is_integer, iso, iso_string, merged, or_null, strict_eq, truthy, upper_first};

/// The default message (reminderService.DEFAULT_TEMPLATE).
pub fn default_reminder_template() -> Value {
    json!({ "content": "Hey, I'm a reminder!", "embed": null, "buttons": [] })
}

/// `MODE_OPTIONS` (value, label).
pub const MODE_OPTIONS: [(&str, &str); 3] = [("TIME_OF_DAY", "At a time of day"), ("INTERVAL", "Every interval"), ("ONE_SHOT", "Once")];

fn day_label(name: &str) -> Option<&'static str> {
    Some(match name {
        "sun" => "Sunday",
        "mon" => "Monday",
        "tue" => "Tuesday",
        "wed" => "Wednesday",
        "thu" => "Thursday",
        "fri" => "Friday",
        "sat" => "Saturday",
        _ => return None,
    })
}

/// Monday first; the values are the stored day numbers' names (WEEKDAY_NAMES: 0 = sun).
const WEEKDAY_OPTIONS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn mode_of(r: &Map<String, Value>) -> Option<&str> {
    r.get("mode").and_then(Value::as_str)
}

/// `checkReminder(data, { mode, current })`: what each mode needs.
pub fn check_reminder(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let r = if ctx.mode == Mode::Update && ctx.current.is_some() { merged(ctx.current, data) } else { data.clone() };
    let mode = mode_of(&r);
    if mode == Some("TIME_OF_DAY") && !truthy(r.get("timeOfDay")) {
        return Some(Problem::new("timeOfDay", "Pick the time of day."));
    }
    if mode == Some("INTERVAL") && !is_integer(r.get("intervalMinutes")) {
        return Some(Problem::new("intervalMinutes", "Set how often it repeats."));
    }
    if mode == Some("ONE_SHOT") && !truthy(r.get("runAt")) {
        return Some(Problem::new("runAt", "Pick the date and time."));
    }
    if mode == Some("INTERVAL")
        && truthy(r.get("windowStart"))
        && strict_eq(r.get("windowStart").unwrap_or(&Value::Null), r.get("windowEnd").unwrap_or(&Value::Null))
    {
        return Some(Problem::new("windowEnd", "The daily start and stop can't be the same time."));
    }
    None
}

fn choice(mode: &str) -> Option<&'static str> {
    Some(match mode {
        "TIME_OF_DAY" => "time",
        "INTERVAL" => "interval",
        "ONE_SHOT" => "once",
        _ => return None,
    })
}

/// `scheduleInput(s)`: the form's schedule as the raw /reminder options validateReminderInput reads (only the options
/// of the chosen mode; `at` is a Discord timestamp, an exact instant).
pub fn schedule_input(s: &Map<String, Value>) -> Map<String, Value> {
    let mode = mode_of(s).unwrap_or("");
    let mut input = Map::new();
    if let Some(c) = choice(mode) {
        input.insert("mode".into(), json!(c));
    }
    if mode != "ONE_SHOT" {
        let days = weekday_numbers(s.get("weekdays"));
        let text = if days.is_empty() { "all".to_string() } else { days.iter().map(i64::to_string).collect::<Vec<_>>().join(",") };
        input.insert("weekdays".into(), json!(text));
        input.insert("requireActivity".into(), Value::Bool(s.get("requireActivity") == Some(&Value::Bool(true))));
    }
    if mode == "TIME_OF_DAY"
        && let Some(t) = s.get("timeOfDay").filter(|v| !v.is_null())
    {
        input.insert("time".into(), t.clone());
    }
    if mode == "INTERVAL" {
        if let Some(n) = s.get("intervalMinutes").filter(|v| is_integer(Some(v))) {
            input.insert("interval".into(), json!(dash_schema::js::to_js_string(Some(n))));
        }
        input.insert("windowStart".into(), s.get("windowStart").filter(|v| !v.is_null()).cloned().unwrap_or(json!("none")));
        input.insert("windowEnd".into(), s.get("windowEnd").filter(|v| !v.is_null()).cloned().unwrap_or(json!("none")));
    }
    if mode == "ONE_SHOT"
        && truthy(s.get("runAt"))
    {
        let t = match s.get("runAt") {
            Some(Value::String(x)) => bot_util::js::date_parse(x),
            _ => f64::NAN,
        };
        input.insert("at".into(), json!(format!("<t:{}>", number_to_string((t / 1000.0).floor()))));
    }
    input
}

/// `weekdayNames(days)`: stored day numbers (0 = Sunday) -> option values, in the options' order.
pub fn weekday_names(days: Option<&Value>) -> Vec<&'static str> {
    let set: Vec<i64> = as_list(days).iter().filter_map(|d| int_of(Some(d))).filter(|d| (0..=6).contains(d)).collect();
    WEEKDAY_OPTIONS
        .iter()
        .copied()
        .filter(|v| WEEKDAY_NAMES.iter().position(|n| n == v).is_some_and(|i| set.contains(&(i as i64))))
        .collect()
}

/// `weekdayNumbers(names)`: option values -> stored day numbers, sorted.
pub fn weekday_numbers(names: Option<&Value>) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for n in as_list(names) {
        if let Some(i) = n.as_str().and_then(|s| WEEKDAY_NAMES.iter().position(|w| *w == s)) {
            let i = i as i64;
            if !out.contains(&i) {
                out.push(i);
            }
        }
    }
    out.sort_unstable();
    out
}

fn days_text(days: Option<&Value>) -> String {
    let mut wd: Vec<f64> = Vec::new();
    for d in as_list(days) {
        let n = d.as_f64().unwrap_or(f64::NAN);
        if !wd.iter().any(|x| x == &n || (x.is_nan() && n.is_nan())) {
            wd.push(n);
        }
    }
    wd.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if wd.is_empty() || wd.len() == 7 {
        return "every day".into();
    }
    let joined = wd.iter().map(|d| number_to_string(*d)).collect::<Vec<_>>().join(",");
    if joined == "1,2,3,4,5" {
        return "on weekdays".into();
    }
    if joined == "0,6" {
        return "on weekends".into();
    }
    let names: Vec<&str> = wd
        .iter()
        .map(|d| {
            if d.fract() == 0.0 && (0.0..7.0).contains(d) { day_label(WEEKDAY_NAMES[*d as usize]).unwrap_or("") } else { "" }
        })
        .collect();
    format!("on {}", names.join(", "))
}

/// `scheduleText(r, tz)`: a one-line schedule for the list (describeSchedule without Discord timestamps).
pub fn schedule_text(r: &Value, tz: &str) -> String {
    match r.get("mode").and_then(Value::as_str) {
        Some("TIME_OF_DAY") => {
            let at = r.get("timeOfDay").filter(|v| !v.is_null()).map_or_else(|| "??:??".to_string(), |v| dash_schema::js::to_js_string(Some(v)));
            format!("{} at {at} ({tz})", upper_first(&days_text(r.get("weekdays"))))
        }
        Some("INTERVAL") => {
            let minutes = match r.get("intervalMinutes") {
                None | Some(Value::Null) => 0.0,
                Some(v) => dash_schema::js::to_number(Some(v)),
            };
            let mut s = format!("Every {}", format_duration_long(minutes * 60_000.0));
            if truthy(r.get("windowStart")) || truthy(r.get("windowEnd")) {
                let txt = |k: &str, d: &str| match r.get(k) {
                    Some(x) if !x.is_null() => dash_schema::js::to_js_string(Some(x)),
                    _ => d.to_string(),
                };
                s.push_str(&format!(" between {} and {} ({tz})", txt("windowStart", "00:00"), txt("windowEnd", "24:00")));
            }
            let n = as_list(r.get("weekdays")).len();
            if n > 0 && n < 7 {
                s.push(' ');
                s.push_str(&days_text(r.get("weekdays")));
            }
            s
        }
        Some("ONE_SHOT") => match iso_string(r.get("runAt")) {
            Some(i) => format!("Once at {} UTC", i[..16].replace('T', " ")),
            None => "Once".into(),
        },
        _ => "Unknown schedule".into(),
    }
}

/// `isDone(r)`: a one-off that was sent and switched itself off (/reminder list's "done").
pub fn is_done(r: &Value) -> bool {
    !truthy(r.get("enabled"))
        && r.get("mode").and_then(Value::as_str) == Some("ONE_SHOT")
        && match (date_ms(r.get("lastSentAt")), date_ms(r.get("runAt"))) {
            (Some(sent), Some(run)) => sent >= run,
            _ => false,
        }
}

/// `reminderItem(row, { tz, nextRunAt })`: a Reminder row as a collection item.
pub fn reminder_item(row: &Value, tz: &str, next_run_at: Option<&Value>) -> Value {
    let enabled = truthy(row.get("enabled"));
    let status = if is_done(row) {
        "Sent"
    } else if enabled {
        "Active"
    } else {
        "Paused"
    };
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "name": raw(row.get("name")),
        "channelId": raw(row.get("channelId")),
        "template": bot_core::custom_embed::normalize_template(row.get("template").unwrap_or(&Value::Null)),
        "mode": raw(row.get("mode")),
        "timeOfDay": or_null(row.get("timeOfDay")),
        "weekdays": weekday_names(row.get("weekdays")),
        "intervalMinutes": int_of(row.get("intervalMinutes")).map_or(Value::Null, |n| json!(n)),
        "windowStart": or_null(row.get("windowStart")),
        "windowEnd": or_null(row.get("windowEnd")),
        "runAt": iso(row.get("runAt")),
        "requireActivity": row.get("requireActivity") == Some(&Value::Bool(true)),
        "pingRoleId": or_null(row.get("pingRoleId")),
        "enabled": row.get("enabled") != Some(&Value::Bool(false)),
        "status": status,
        "schedule": schedule_text(row, tz),
        "nextRunAt": if enabled { iso(next_run_at) } else { Value::Null },
        "lastSentAt": iso(row.get("lastSentAt")),
        "timezone": tz,
    }))
}
