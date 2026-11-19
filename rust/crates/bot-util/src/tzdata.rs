//! The ICU time zone data the JS bot sees through `Intl`, generated from Node by
//! `rust/fixtures/gen/bot-util-tzdata.mjs` into `data/tz.json`: the zone list, every accepted id with its canonical
//! spelling, and the `localeCompare` order of the strings the JS sorts. Offsets come from chrono-tz.

use chrono::{NaiveDateTime, Offset, TimeZone};
use chrono_tz::Tz;
use std::collections::HashMap;
use std::sync::LazyLock;

struct Data {
    list: Vec<String>,
    ids: HashMap<String, (String, String)>,
    rank: HashMap<String, usize>,
    tzdata: String,
    /// Zones whose ICU rules are newer than chrono-tz's: ICU's transitions (ms, seconds east) from 1900 on.
    overrides: HashMap<String, Vec<(f64, i32)>>,
}

static DATA: LazyLock<Data> = LazyLock::new(|| {
    let v: serde_json::Value = serde_json::from_str(include_str!("../data/tz.json")).expect("data/tz.json is valid JSON");
    let list = v["list"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let mut ids = HashMap::new();
    if let Some(o) = v["ids"].as_object() {
        for (k, pair) in o {
            if let (Some(c), Some(z)) = (pair[0].as_str(), pair[1].as_str()) {
                ids.insert(k.clone(), (c.to_string(), z.to_string()));
            }
        }
    }
    let mut rank = HashMap::new();
    if let Some(o) = v["rank"].as_object() {
        for (k, r) in o {
            rank.insert(k.clone(), r.as_u64().unwrap_or(0) as usize);
        }
    }
    let mut overrides = HashMap::new();
    if let Some(o) = v["overrides"].as_object() {
        for (zone, list) in o {
            let l: Vec<(f64, i32)> = list
                .as_array()
                .map(|a| a.iter().filter_map(|p| Some((p[0].as_f64()?, p[1].as_i64()? as i32))).collect())
                .unwrap_or_default();
            overrides.insert(zone.clone(), l);
        }
    }
    Data { list, ids, rank, tzdata: v["tzdata"].as_str().unwrap_or("").to_string(), overrides }
});

/// A resolved time zone: an IANA region (offsets from chrono-tz) or a fixed offset id such as '+05:30'.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zone {
    Tz(Tz),
    /// A zone whose ICU rules differ from chrono-tz's: ICU's transitions (ms, seconds east) from 1900, chrono-tz
    /// before that.
    Table(Tz, &'static [(f64, i32)]),
    /// Seconds east of UTC.
    Fixed(i32),
}

/// The tzdata version Node used when the data was generated.
pub fn icu_tzdata_version() -> &'static str {
    &DATA.tzdata
}

/// `Intl.supportedValuesOf('timeZone')` with the legacy names mapped, 'UTC' first (timezones.js `listTimeZones`).
pub fn zone_list() -> &'static [String] {
    &DATA.list
}

/// Position of `s` in ICU's en-US collation order, for the strings the JS sorts with `localeCompare`.
pub fn collation_rank(s: &str) -> Option<usize> {
    DATA.rank.get(s).copied()
}

/// `a.localeCompare(b)` for zone names and 'UTC±HH:MM' labels (falls back to code-unit order for anything else).
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    match (collation_rank(a), collation_rank(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.encode_utf16().cmp(b.encode_utf16()),
    }
}

/// An offset id ('+05:30', '-0800', '+05', '−05:00'): seconds east of UTC, as ICU accepts them.
fn parse_offset_id(s: &str) -> Option<i32> {
    let mut chars = s.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' | '−' => -1,
        _ => return None,
    };
    let rest: String = chars.collect();
    // Byte slicing below: anything but ASCII is no offset id (and '+1é1' must not split a character).
    if !rest.is_ascii() {
        return None;
    }
    let (hh, mm) = match rest.len() {
        2 => (&rest[..2], "00"),
        4 => (&rest[..2], &rest[2..]),
        5 if rest.as_bytes()[2] == b':' => (&rest[..2], &rest[3..]),
        _ => return None,
    };
    if !hh.bytes().chain(mm.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (i32, i32) = (hh.parse().ok()?, mm.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

fn offset_id_name(secs: i32) -> String {
    let a = secs.abs();
    format!("{}{:02}:{:02}", if secs < 0 { '-' } else { '+' }, a / 3600, (a % 3600) / 60)
}

/// Resolves a time zone id the way `new Intl.DateTimeFormat('en-US', { timeZone })` does: case-insensitive IANA
/// names and ICU aliases, and offset ids. Returns the canonical spelling (`resolvedOptions().timeZone`) and the zone.
pub fn resolve(id: &str) -> Option<(String, Zone)> {
    if let Some(secs) = parse_offset_id(id) {
        return Some((offset_id_name(secs), Zone::Fixed(secs)));
    }
    if !id.is_ascii() {
        return None;
    }
    let (canonical, chrono) = DATA.ids.get(&id.to_ascii_lowercase())?;
    let tz: Tz = chrono.parse().ok()?;
    let zone = match DATA.overrides.get(canonical) {
        Some(table) if !table.is_empty() => Zone::Table(tz, table.as_slice()),
        _ => Zone::Tz(tz),
    };
    Some((canonical.clone(), zone))
}

/// Seconds east of UTC of `zone` at the instant `ms` (epoch milliseconds, any finite value).
pub fn offset_seconds(zone: Zone, ms: f64) -> i32 {
    match zone {
        Zone::Fixed(s) => s,
        Zone::Tz(_) | Zone::Table(..) => {
            // chrono-tz lists transitions up to 2099; ICU applies a zone's last rule forever. Past 2099 the instant is
            // read in an earlier year with the same calendar (weekday of 1 January and leap year), so "last Sunday
            // of March" lands on the same date.
            let ms = shift_past_2099(ms);
            let tz = match zone {
                Zone::Table(tz, list) => {
                    if list.first().is_some_and(|first| ms >= first.0) {
                        let i = list.partition_point(|(t, _)| *t <= ms);
                        return list[i.saturating_sub(1)].1;
                    }
                    tz
                }
                Zone::Tz(tz) => tz,
                Zone::Fixed(s) => return s,
            };
            // chrono covers about ±262,000 years; clamp beyond that (the offset there is the zone's first or last).
            let secs = (ms / 1000.0).floor().clamp(-8_000_000_000_000.0, 8_000_000_000_000.0) as i64;
            let ndt = NaiveDateTime::from_timestamp_opt_compat(secs);
            tz.offset_from_utc_datetime(&ndt).fix().local_minus_utc()
        }
    }
}

const END_2099_MS: f64 = 4_102_444_800_000.0; // 2100-01-01T00:00:00Z

fn shift_past_2099(ms: f64) -> f64 {
    if !(ms >= END_2099_MS) {
        return ms;
    }
    let f = crate::js::utc_fields(ms);
    let year = f.year as f64;
    let jan1 = |y: f64| crate::js::make_day(y, 0.0, 1.0);
    let leap = |y: f64| crate::js::make_day(y, 2.0, 1.0) - crate::js::make_day(y, 1.0, 1.0) == 29.0;
    let weekday = |d: f64| (d + 4.0).rem_euclid(7.0);
    let target_wd = weekday(jan1(year));
    let target_leap = leap(year);
    for y in 2072..=2099 {
        let y = y as f64;
        if weekday(jan1(y)) == target_wd && leap(y) == target_leap {
            return ms - (jan1(year) - jan1(y)) * crate::js::MS_PER_DAY;
        }
    }
    ms
}

trait FromTimestampCompat {
    fn from_timestamp_opt_compat(secs: i64) -> NaiveDateTime;
}

impl FromTimestampCompat for NaiveDateTime {
    fn from_timestamp_opt_compat(secs: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|d| d.naive_utc())
            .unwrap_or(if secs < 0 { NaiveDateTime::MIN } else { NaiveDateTime::MAX })
    }
}
