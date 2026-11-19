//! Port of `src/utils/time.js`: time zone maths and date/time parsing. The JS uses `Intl.DateTimeFormat`; this uses
//! the same ICU zone ids (see `tzdata`) with chrono-tz offsets, and reproduces every rule: hour 24 never appears,
//! a DST gap gives the first valid instant after it, an overlap the earlier instant, and an invalid zone is UTC.
//! Instants are epoch milliseconds as `f64` (`*_ms` functions, NaN = Invalid Date, exactly as the JS computes them),
//! with `chrono` wrappers for callers. Nothing panics.

use crate::js::{self, date_utc, utc_fields};
use crate::jsre::LazyRe;
use crate::lazy_re;
use crate::tzdata::{self, Zone};
use chrono::{DateTime, Utc};

/// `WEEKDAY_NAMES`.
pub const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

const DAY_MS: f64 = 86_400_000.0;

/// An instant as epoch milliseconds (`Date.getTime()`).
pub fn to_ms(d: &DateTime<Utc>) -> f64 {
    d.timestamp_millis() as f64
}

/// Epoch milliseconds back to a `DateTime` (None for NaN or out of range).
pub fn from_ms(ms: f64) -> Option<DateTime<Utc>> {
    if !ms.is_finite() {
        return None;
    }
    DateTime::from_timestamp_millis(ms as i64)
}

// ── Time zones ─────────────────────────────────────────────────────────────────

/// True when `tz` is a time zone id ICU knows ('Europe/London', 'utc', '+05:30'), 1 to 64 characters.
pub fn is_valid_time_zone(tz: &str) -> bool {
    let n = js::utf16_len(tz);
    (1..=64).contains(&n) && tzdata::resolve(tz).is_some()
}

fn zone_of(tz: &str) -> Zone {
    if is_valid_time_zone(tz) { tzdata::resolve(tz).map(|(_, z)| z).unwrap_or(Zone::Fixed(0)) } else { Zone::Fixed(0) }
}

/// Wall-clock fields of an instant in a zone. `month` is 1-12, `weekday` 0 (Sunday) to 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZonedParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub weekday: u32,
}

/// Fields of the instant `ms` in `zone` as ICU prints them (`formatToParts`): years before 1 are era years (1 BC is
/// year 1), an invalid date gives the epoch defaults.
fn icu_parts(ms: f64, zone: Zone) -> Option<(f64, f64, f64, f64, f64, f64)> {
    let t = js::time_clip(ms);
    if t.is_nan() {
        return None;
    }
    let local = t + tzdata::offset_seconds(zone, t) as f64 * 1000.0;
    let f = utc_fields(local);
    let year = if f.year <= 0 { 1 - f.year } else { f.year };
    Some((year as f64, (f.month0 + 1) as f64, f.day as f64, f.hour as f64, f.minute as f64, f.second as f64))
}

/// `zonedParts(ms, tz)`: a non-finite `ms` is the epoch; an invalid zone is UTC.
pub fn zoned_parts_ms(ms: f64, tz: &str) -> ZonedParts {
    let ms = if ms.is_finite() { ms } else { 0.0 };
    let (year, month, day, hour, minute, second) = icu_parts(ms, zone_of(tz)).unwrap_or((1970.0, 1.0, 1.0, 0.0, 0.0, 0.0));
    let weekday = {
        let t = date_utc(year, month - 1.0, day, 0.0, 0.0, 0.0, 0.0);
        if t.is_nan() { 0 } else { utc_fields(t).weekday }
    };
    ZonedParts { year: year as i64, month: month as u32, day: day as u32, hour: hour as u32, minute: minute as u32, second: second as u32, weekday }
}

/// Wall-clock fields of an instant in a zone.
pub fn zoned_parts(date: &DateTime<Utc>, tz: &str) -> ZonedParts {
    zoned_parts_ms(to_ms(date), tz)
}

/// Offset of the zone at an instant, in ms (positive east of UTC), computed exactly as the JS does.
fn offset_at(ms: f64, tz: &str) -> f64 {
    let p = zoned_parts_ms(ms, tz);
    date_utc(p.year as f64, p.month as f64 - 1.0, p.day as f64, p.hour as f64, p.minute as f64, p.second as f64, 0.0)
        - (ms / 1000.0).floor() * 1000.0
}

/// Converts a local wall-clock time in `tz` to the instant (epoch ms; NaN when a field is not a number). A time in a
/// DST gap gives the first valid instant after the gap; a time in an overlap gives the earlier instant.
pub fn zoned_to_utc_ms(year: f64, month: f64, day: f64, hour: f64, minute: f64, tz: &str) -> f64 {
    let zone = if is_valid_time_zone(tz) { tz } else { "UTC" };
    let wall = date_utc(year, month - 1.0, day, hour, minute, 0.0, 0.0);
    if !wall.is_finite() {
        return f64::NAN;
    }
    let off_before = offset_at(wall - DAY_MS, zone);
    let off_after = offset_at(wall + DAY_MS, zone);
    let mut offs = vec![off_before];
    if off_after != off_before && !(off_after.is_nan() && off_before.is_nan()) {
        offs.push(off_after);
    }
    let mut cands: Vec<f64> = offs.iter().map(|o| wall - o).filter(|&t| t + offset_at(t, zone) == wall).collect();
    cands.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(&c) = cands.first() {
        return js::time_clip(c);
    }
    let mut lo = wall - off_after;
    let mut hi = wall - off_before;
    if lo > hi {
        std::mem::swap(&mut lo, &mut hi);
    }
    let mut a = (lo / 60_000.0).floor();
    let mut b = (hi / 60_000.0).ceil();
    while a < b {
        let mid = ((a + b) / 2.0).floor();
        if offset_at(mid * 60_000.0, zone) == off_after {
            b = mid;
        } else {
            a = mid + 1.0;
        }
    }
    js::time_clip(a * 60_000.0)
}

/// Converts a local wall-clock time in `tz` to the instant (see [`zoned_to_utc_ms`]).
pub fn zoned_to_utc(year: i64, month: i64, day: i64, hour: i64, minute: i64, tz: &str) -> Option<DateTime<Utc>> {
    from_ms(zoned_to_utc_ms(year as f64, month as f64, day as f64, hour as f64, minute as f64, tz))
}

/// Local calendar date key 'YYYY-MM-DD' in a zone.
pub fn local_date_key_ms(ms: f64, tz: &str) -> String {
    let p = zoned_parts_ms(ms, tz);
    format!("{:04}-{:02}-{:02}", p.year, p.month, p.day)
}

/// Local calendar date key 'YYYY-MM-DD' in a zone.
pub fn local_date_key(date: &DateTime<Utc>, tz: &str) -> String {
    local_date_key_ms(to_ms(date), tz)
}

/// Days in a month (month 1-12; out-of-range months roll as in `Date.UTC`). NaN for a non-finite input.
pub fn days_in_month_f64(year: f64, month: f64) -> f64 {
    let t = date_utc(year, month, 0.0, 0.0, 0.0, 0.0, 0.0);
    if t.is_nan() { f64::NAN } else { utc_fields(t).day as f64 }
}

/// Days in a month (month 1-12).
pub fn days_in_month(year: i64, month: i64) -> u32 {
    days_in_month_f64(year as f64, month as f64) as u32
}

// ── Time of day and weekdays ───────────────────────────────────────────────────

/// A local time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOfDay {
    pub hour: u32,
    pub minute: u32,
}

static TIME_RE: LazyRe = lazy_re!(r"^(\d{1,2})(?::(\d{2}))?\s*(am|pm|a\.m\.|p\.m\.)?$", "");

/// Parses '9', '09:30', '9:30pm', '21:30', '12am', '12pm', 'noon' or 'midnight'.
pub fn parse_time_of_day(input: &str) -> Option<TimeOfDay> {
    let s = js::trim(input).to_lowercase();
    if s == "noon" {
        return Some(TimeOfDay { hour: 12, minute: 0 });
    }
    if s == "midnight" {
        return Some(TimeOfDay { hour: 0, minute: 0 });
    }
    let (t, m) = TIME_RE.exec(&s)?;
    let mut hour: u32 = m.group(&t, 1)?.parse().ok()?;
    let minute: u32 = match m.group(&t, 2) {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if minute > 59 {
        return None;
    }
    if let Some(ap) = m.group(&t, 3) {
        if !(1..=12).contains(&hour) {
            return None;
        }
        let pm = ap.starts_with('p');
        if hour == 12 {
            hour = if pm { 12 } else { 0 };
        } else if pm {
            hour += 12;
        }
    } else if hour > 23 {
        return None;
    }
    Some(TimeOfDay { hour, minute })
}

/// 'HH:MM'.
pub fn format_time_of_day(t: TimeOfDay) -> String {
    format!("{:02}:{:02}", t.hour, t.minute)
}

fn day_alias(tok: &str) -> Option<u32> {
    Some(match tok {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tues" | "tuesday" => 2,
        "wed" | "weds" | "wednesday" => 3,
        "thu" | "thur" | "thurs" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

fn day_token(tok: &str) -> Option<u32> {
    if tok.len() == 1 && matches!(tok.as_bytes()[0], b'0'..=b'6') {
        return Some((tok.as_bytes()[0] - b'0') as u32);
    }
    day_alias(tok)
}

static RANGE_RE: LazyRe = lazy_re!(r"^([a-z0-9]+)-([a-z0-9]+)$", "");

/// Parses a weekday list: 'mon,wed,fri', 'fri-mon' (wraps), 'weekdays', 'weekends', 'all', 'daily'. Sorted unique
/// days (0 = Sunday), or None when unreadable.
pub fn parse_weekdays(input: &str) -> Option<Vec<u32>> {
    let s = js::trim(input).to_lowercase();
    if s.is_empty() {
        return None;
    }
    match s.as_str() {
        "all" | "daily" | "everyday" | "every day" => return Some(vec![0, 1, 2, 3, 4, 5, 6]),
        "weekdays" => return Some(vec![1, 2, 3, 4, 5]),
        "weekends" => return Some(vec![0, 6]),
        _ => {}
    }
    let mut days = std::collections::BTreeSet::new();
    for tok in s.split(|c: char| js::is_js_space(c) || c == ',') {
        if tok.is_empty() {
            continue;
        }
        if let Some(d) = day_token(tok) {
            days.insert(d);
            continue;
        }
        let (t, m) = RANGE_RE.exec(tok)?;
        let from = day_token(&m.group(&t, 1)?)?;
        let to = day_token(&m.group(&t, 2)?)?;
        let mut d = from;
        loop {
            days.insert(d);
            if d == to {
                break;
            }
            d = (d + 1) % 7;
        }
    }
    if days.is_empty() { None } else { Some(days.into_iter().collect()) }
}

// ── Scheduling ─────────────────────────────────────────────────────────────────

fn allowed_days(weekdays: &[f64]) -> Option<Vec<u32>> {
    let wd: Vec<u32> = weekdays.iter().filter(|d| d.fract() == 0.0 && (0.0..=6.0).contains(*d)).map(|d| *d as u32).collect();
    if wd.is_empty() { None } else { Some(wd) }
}

#[derive(Debug, Clone, Copy)]
struct CalDay {
    year: f64,
    month: f64,
    day: f64,
    weekday: u32,
}

fn add_days(year: i64, month: u32, day: u32, offset: i64) -> CalDay {
    let t = date_utc(year as f64, month as f64 - 1.0, day as f64 + offset as f64, 0.0, 0.0, 0.0, 0.0);
    if t.is_nan() {
        return CalDay { year: f64::NAN, month: f64::NAN, day: f64::NAN, weekday: 0 };
    }
    let f = utc_fields(t);
    CalDay { year: f.year as f64, month: (f.month0 + 1) as f64, day: f.day as f64, weekday: f.weekday }
}

/// Next occurrence of a local time of day, strictly after `after_ms`. One instant per allowed local date: a DST
/// gap gives the first valid instant after the missing time, an overlap the earlier instant only.
pub fn next_time_of_day_ms(tz: &str, hour: f64, minute: f64, weekdays: &[f64], after_ms: f64) -> f64 {
    let wd = allowed_days(weekdays);
    if !after_ms.is_finite() {
        return f64::NAN;
    }
    let p = zoned_parts_ms(after_ms, tz);
    for k in 0..=8 {
        let d = add_days(p.year, p.month, p.day, k);
        if let Some(w) = &wd
            && !w.contains(&d.weekday) {
                continue;
            }
        let t = zoned_to_utc_ms(d.year, d.month, d.day, hour, minute, tz);
        if t > after_ms {
            return t;
        }
    }
    after_ms + DAY_MS
}

/// Next occurrence of a local time of day strictly after `after` (weekdays 0 = Sunday; empty means every day).
pub fn next_time_of_day(tz: &str, hour: u32, minute: u32, weekdays: &[u32], after: &DateTime<Utc>) -> Option<DateTime<Utc>> {
    let wd: Vec<f64> = weekdays.iter().map(|d| *d as f64).collect();
    from_ms(next_time_of_day_ms(tz, hour as f64, minute as f64, &wd, to_ms(after)))
}

/// Options of [`next_interval_run_ms`].
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalOptions<'a> {
    pub tz: &'a str,
    pub interval_minutes: f64,
    pub weekdays: Vec<f64>,
    pub window_start: Option<&'a str>,
    pub window_end: Option<&'a str>,
    pub after_ms: f64,
    pub anchor_ms: Option<f64>,
}

/// Next run of a repeating interval, strictly after `after`; None when nothing exists within 8 days or the input is
/// invalid. Without a window the candidates are `anchor + k·interval`; with one (a null start is 00:00, a null end
/// 24:00, an end at or before the start crosses midnight) they are `day at start + j·interval` before the end.
pub fn next_interval_run_ms(o: &IntervalOptions<'_>) -> Option<f64> {
    let im = o.interval_minutes;
    if im.fract() != 0.0 || !(1.0..=10_080.0).contains(&im) {
        return None;
    }
    let i = im * 60_000.0;
    let after_ms = o.after_ms;
    if !after_ms.is_finite() {
        return None;
    }
    let wd = allowed_days(&o.weekdays);
    let allowed = |w: u32| wd.as_ref().is_none_or(|l| l.contains(&w));
    let horizon = after_ms + 8.0 * DAY_MS;

    if o.window_start.is_none() && o.window_end.is_none() {
        let origin = match o.anchor_ms {
            Some(a) if a.is_finite() => a,
            _ => after_ms,
        };
        let mut t = origin + (((after_ms - origin) / i).floor() + 1.0) * i;
        while t <= horizon {
            let p = zoned_parts_ms(t, o.tz);
            if allowed(p.weekday) {
                return Some(t);
            }
            let next = add_days(p.year, p.month, p.day, 1);
            let midnight = zoned_to_utc_ms(next.year, next.month, next.day, 0.0, 0.0, o.tz);
            let a = ((midnight - origin) / i).ceil();
            let b = ((t - origin) / i).floor() + 1.0;
            t = origin + js_max(a, b) * i;
        }
        return None;
    }

    let start = match o.window_start {
        None => TimeOfDay { hour: 0, minute: 0 },
        Some(s) => parse_time_of_day(s)?,
    };
    let end = match o.window_end {
        None => None,
        Some(s) => Some(parse_time_of_day(s)?),
    };
    let start_min = start.hour * 60 + start.minute;
    let end_next_day = end.is_none_or(|e| e.hour * 60 + e.minute <= start_min);
    let p = zoned_parts_ms(after_ms, o.tz);
    for k in -1..=8 {
        let d = add_days(p.year, p.month, p.day, k);
        if !allowed(d.weekday) {
            continue;
        }
        let seg_start = zoned_to_utc_ms(d.year, d.month, d.day, start.hour as f64, start.minute as f64, o.tz);
        let e = add_days(p.year, p.month, p.day, k + if end_next_day { 1 } else { 0 });
        let (eh, em) = end.map_or((0.0, 0.0), |e| (e.hour as f64, e.minute as f64));
        let seg_end = zoned_to_utc_ms(e.year, e.month, e.day, eh, em, o.tz);
        let lo = js_max(seg_start, after_ms + 1.0);
        let t = seg_start + ((lo - seg_start) / i).ceil() * i;
        if t < seg_end {
            return Some(t);
        }
    }
    None
}

/// `Math.max` (NaN wins).
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }
}

// ── Date input ─────────────────────────────────────────────────────────────────

static STAMP_RE: LazyRe = lazy_re!(r"^<t:(\d{1,13})(?::[tTdDfFR])?>$", "");
static IN_RE: LazyRe = lazy_re!(r"^in\s+", "i");
static FULL_RE: LazyRe = lazy_re!(r"^(\d{4})-(\d{1,2})-(\d{1,2})(?:[ T](.+))?$", "");
static MD_RE: LazyRe = lazy_re!(r"^(\d{1,2})-(\d{1,2})(?:\s+(.+))?$", "");

fn num(s: &str) -> f64 {
    s.parse().unwrap_or(f64::NAN)
}

/// Parses a date a user typed: a Discord timestamp, a relative duration ('2h30m', 'in 2 days'),
/// 'YYYY-MM-DD [HH:MM]', 'MM-DD [HH:MM]' (this year or the next ones) or 'HH:MM' (today or tomorrow). Returns epoch
/// ms (NaN for an Invalid Date the JS would return too), or None.
pub fn parse_date_input_ms(input: &str, tz: &str, now_ms: f64) -> Option<f64> {
    let s = js::trim(input);
    if s.is_empty() || !now_ms.is_finite() {
        return None;
    }
    if let Some((t, m)) = STAMP_RE.exec(s) {
        return Some(js::time_clip(num(&m.group(&t, 1)?) * 1000.0));
    }
    if let Some(rel) = crate::parse_duration::parse_duration(&IN_RE.replace_first_with(s, |_, _| String::new())) {
        return Some(js::time_clip(now_ms + rel as f64));
    }
    if let Some((t, m)) = FULL_RE.exec(s) {
        let year = num(&m.group(&t, 1)?);
        let month = num(&m.group(&t, 2)?);
        let day = num(&m.group(&t, 3)?);
        if !(1.0..=12.0).contains(&month) || day < 1.0 || day > days_in_month_f64(year, month) {
            return None;
        }
        let time = match m.group(&t, 4) {
            None => TimeOfDay { hour: 0, minute: 0 },
            Some(x) => parse_time_of_day(&x)?,
        };
        return Some(zoned_to_utc_ms(year, month, day, time.hour as f64, time.minute as f64, tz));
    }
    if let Some((t, m)) = MD_RE.exec(s) {
        let month = num(&m.group(&t, 1)?);
        let day = num(&m.group(&t, 2)?);
        if !(1.0..=12.0).contains(&month) || day < 1.0 {
            return None;
        }
        let time = match m.group(&t, 3) {
            None => TimeOfDay { hour: 0, minute: 0 },
            Some(x) => parse_time_of_day(&x)?,
        };
        let this_year = zoned_parts_ms(now_ms, tz).year as f64;
        for k in 0..=8 {
            let year = this_year + k as f64;
            if day > days_in_month_f64(year, month) {
                continue;
            }
            let t = zoned_to_utc_ms(year, month, day, time.hour as f64, time.minute as f64, tz);
            if t > now_ms {
                return Some(t);
            }
        }
        return None;
    }
    let tod = parse_time_of_day(s)?;
    Some(next_time_of_day_ms(tz, tod.hour as f64, tod.minute as f64, &[], now_ms))
}

/// [`parse_date_input_ms`] as a `DateTime` (None also for an invalid date).
pub fn parse_date_input(input: &str, tz: &str, now: &DateTime<Utc>) -> Option<DateTime<Utc>> {
    parse_date_input_ms(input, tz, to_ms(now)).and_then(from_ms)
}

fn month_number(name: &str) -> Option<u32> {
    Some(match name {
        "january" | "jan" => 1,
        "february" | "feb" => 2,
        "march" | "mar" => 3,
        "april" | "apr" => 4,
        "may" => 5,
        "june" | "jun" => 6,
        "july" | "jul" => 7,
        "august" | "aug" => 8,
        "september" | "sep" | "sept" => 9,
        "october" | "oct" => 10,
        "november" | "nov" => 11,
        "december" | "dec" => 12,
        _ => return None,
    })
}

/// A birthday: month 1-12, day, optional year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Birthday {
    pub month: u32,
    pub day: u32,
    pub year: Option<i64>,
}

static BD_MD: LazyRe = lazy_re!(r"^(\d{1,2})-(\d{1,2})$", "");
static BD_YMD: LazyRe = lazy_re!(r"^(\d{4})-(\d{1,2})-(\d{1,2})$", "");
static BD_NAME_FIRST: LazyRe = lazy_re!(r"^([a-z]+)\.?\s+(\d{1,2})(?:st|nd|rd|th)?(?:,?\s+(\d{4}))?$", "");
static BD_DAY_FIRST: LazyRe = lazy_re!(r"^(\d{1,2})(?:st|nd|rd|th)?\s+([a-z]+)\.?(?:,?\s+(\d{4}))?$", "");

/// Parses a birthday: 'MM-DD', 'YYYY-MM-DD', 'march 14', '14 march', 'mar 14 1990', '14 mar 1990'. Anything with '/'
/// is rejected. Feb 29 is allowed without a year; a year must be 1900 ..= the current UTC year of `now_ms`.
pub fn parse_birthday_ms(input: &str, require_year: bool, now_ms: f64) -> Option<Birthday> {
    let s = js::trim(input).to_lowercase();
    if s.is_empty() || s.contains('/') {
        return None;
    }
    let (month, day, year): (f64, f64, Option<f64>);
    if let Some((t, m)) = BD_MD.exec(&s) {
        month = num(&m.group(&t, 1)?);
        day = num(&m.group(&t, 2)?);
        year = None;
    } else if let Some((t, m)) = BD_YMD.exec(&s) {
        year = Some(num(&m.group(&t, 1)?));
        month = num(&m.group(&t, 2)?);
        day = num(&m.group(&t, 3)?);
    } else if let Some((t, m)) = BD_NAME_FIRST.exec(&s) {
        month = month_number(&m.group(&t, 1)?).map_or(0.0, f64::from);
        day = num(&m.group(&t, 2)?);
        year = m.group(&t, 3).map(|y| num(&y));
    } else if let Some((t, m)) = BD_DAY_FIRST.exec(&s) {
        day = num(&m.group(&t, 1)?);
        month = month_number(&m.group(&t, 2)?).map_or(0.0, f64::from);
        year = m.group(&t, 3).map(|y| num(&y));
    } else {
        return None;
    }
    if month == 0.0 || !(1.0..=12.0).contains(&month) || day == 0.0 || day < 1.0 {
        return None;
    }
    let now_year = if now_ms.is_finite() { utc_fields(now_ms).year as f64 } else { f64::NAN };
    if let Some(y) = year
        && (y < 1900.0 || y > now_year) {
            return None;
        }
    if year.is_none() && require_year {
        return None;
    }
    if day > days_in_month_f64(year.unwrap_or(2000.0), month) {
        return None;
    }
    Some(Birthday { month: month as u32, day: day as u32, year: year.map(|y| y as i64) })
}

/// [`parse_birthday_ms`] with a `DateTime`.
pub fn parse_birthday(input: &str, require_year: bool, now: &DateTime<Utc>) -> Option<Birthday> {
    parse_birthday_ms(input, require_year, to_ms(now))
}

/// '<t:1700000000:f>'; an unknown style is 'f'.
pub fn discord_timestamp_ms(ms: f64, style: &str) -> String {
    let st = if style.len() == 1 && "tTdDfFR".contains(style) { style } else { "f" };
    format!("<t:{}:{st}>", js::number_to_string((ms / 1000.0).floor()))
}

/// '<t:1700000000:f>'; an unknown style is 'f'.
pub fn discord_timestamp(date: &DateTime<Utc>, style: &str) -> String {
    discord_timestamp_ms(to_ms(date), style)
}

// ── Free-form times (/timestamp) ───────────────────────────────────────────────

/// Abbreviations people type, mapped to the region they name (that region's local time, daylight saving included),
/// in the JS object's order.
pub const ZONE_ABBREVIATIONS: &[(&str, &str)] = &[
    ("et", "America/New_York"),
    ("est", "America/New_York"),
    ("edt", "America/New_York"),
    ("ct", "America/Chicago"),
    ("cst", "America/Chicago"),
    ("cdt", "America/Chicago"),
    ("mt", "America/Denver"),
    ("mst", "America/Denver"),
    ("mdt", "America/Denver"),
    ("pt", "America/Los_Angeles"),
    ("pst", "America/Los_Angeles"),
    ("pdt", "America/Los_Angeles"),
    ("akst", "America/Anchorage"),
    ("akdt", "America/Anchorage"),
    ("hst", "Pacific/Honolulu"),
    ("adt", "America/Halifax"),
    ("nst", "America/St_Johns"),
    ("ndt", "America/St_Johns"),
    ("brt", "America/Sao_Paulo"),
    ("art", "America/Argentina/Buenos_Aires"),
    ("wet", "Europe/Lisbon"),
    ("west", "Europe/Lisbon"),
    ("bst", "Europe/London"),
    ("cet", "Europe/Berlin"),
    ("cest", "Europe/Berlin"),
    ("eet", "Europe/Athens"),
    ("eest", "Europe/Athens"),
    ("msk", "Europe/Moscow"),
    ("wat", "Africa/Lagos"),
    ("sast", "Africa/Johannesburg"),
    ("eat", "Africa/Nairobi"),
    ("gst", "Asia/Dubai"),
    ("pkt", "Asia/Karachi"),
    ("npt", "Asia/Kathmandu"),
    ("ict", "Asia/Bangkok"),
    ("wib", "Asia/Jakarta"),
    ("sgt", "Asia/Singapore"),
    ("hkt", "Asia/Hong_Kong"),
    ("pht", "Asia/Manila"),
    ("jst", "Asia/Tokyo"),
    ("kst", "Asia/Seoul"),
    ("awst", "Australia/Perth"),
    ("acst", "Australia/Adelaide"),
    ("acdt", "Australia/Adelaide"),
    ("aest", "Australia/Sydney"),
    ("aedt", "Australia/Sydney"),
    ("chst", "Pacific/Guam"),
    ("nzst", "Pacific/Auckland"),
    ("nzdt", "Pacific/Auckland"),
    ("chamorro", "Pacific/Guam"),
];

/// The zone an abbreviation names ('CST' → 'America/Chicago'); 'UTC', 'GMT', 'Z' and 'Zulu' give 'UTC'.
pub fn zone_from_abbreviation(input: &str) -> Option<&'static str> {
    let s = js::trim(input).to_lowercase();
    if matches!(s.as_str(), "utc" | "gmt" | "z" | "zulu") {
        return Some("UTC");
    }
    ZONE_ABBREVIATIONS.iter().find(|(k, _)| *k == s).map(|(_, v)| *v)
}

static OFFSET_RE: LazyRe = lazy_re!(r"^(?:utc|gmt)\s*([+\-−])\s*(\d{1,2})(?::?00)?$", "i");

/// A whole-hour offset ('utc+2', 'GMT-5', 'utc+05:00') as its Etc/GMT zone (signs inverted: UTC+2 is Etc/GMT-2).
/// None for anything else, including half-hour offsets.
pub fn zone_from_offset(input: &str) -> Option<String> {
    let (t, m) = OFFSET_RE.exec(js::trim(input))?;
    let hours: u32 = m.group(&t, 2)?.parse().ok()?;
    if hours == 0 {
        return Some("UTC".into());
    }
    let east = m.group(&t, 1)? == "+";
    if hours > if east { 14 } else { 12 } {
        return None;
    }
    Some(format!("Etc/GMT{}{hours}", if east { '-' } else { '+' }))
}

/// The canonical spelling of a zone id (`resolvedOptions().timeZone`), when it is valid.
pub fn canonical_zone(tz: &str) -> Option<String> {
    if !is_valid_time_zone(tz) {
        return None;
    }
    tzdata::resolve(tz).map(|(c, _)| c)
}

fn zone_token(tok: &str) -> Option<String> {
    if let Some(z) = zone_from_abbreviation(tok) {
        return Some(z.to_string());
    }
    if let Some(z) = zone_from_offset(tok) {
        return Some(z);
    }
    if !tok.contains('/') {
        return None;
    }
    canonical_zone(tok)
}

const LATEST_MS: f64 = 253_402_300_799_000.0; // Date.UTC(9999, 11, 31, 23, 59, 59)

fn day_word_offset(w: &str) -> Option<i64> {
    Some(match w {
        "yesterday" => -1,
        "today" => 0,
        "tomorrow" | "tmrw" | "tmr" => 1,
        _ => return None,
    })
}

static WEEKDAY_RE: LazyRe = lazy_re!(r"^(?:(next|this) )?([a-z]+)$", "");

fn time_or_midnight(part: Option<&str>) -> Option<TimeOfDay> {
    match part {
        None => Some(TimeOfDay { hour: 0, minute: 0 }),
        Some(p) => parse_time_of_day(p),
    }
}

/// Splits '<day> [time]' or '<time> <day>' where `is_day` recognises the day part; two-word day parts go first.
fn split_day(s: &str, is_day: impl Fn(&str) -> bool) -> Option<(String, Option<String>)> {
    let words: Vec<&str> = s.split(' ').collect();
    for n in [2usize, 1] {
        if words.len() < n {
            continue;
        }
        let head = words[..n].join(" ");
        if is_day(&head) {
            let rest = words[n..].join(" ");
            return Some((head, if rest.is_empty() { None } else { Some(rest) }));
        }
        if words.len() > n {
            let tail = words[words.len() - n..].join(" ");
            if is_day(&tail) {
                return Some((tail, Some(words[..words.len() - n].join(" "))));
            }
        }
    }
    None
}

fn is_weekday_part(part: &str) -> bool {
    WEEKDAY_RE.exec(part).is_some_and(|(t, m)| m.group(&t, 2).is_some_and(|d| day_alias(&d).is_some()))
}

static LOCAL_MD1: LazyRe = lazy_re!(r"^([a-z]+)\.? (\d{1,2})(?:st|nd|rd|th)?(?:,? (\d{4}))?(?:,? (.+))?$", "");
static LOCAL_MD2: LazyRe = lazy_re!(r"^(\d{1,2})(?:st|nd|rd|th)? ([a-z]+)\.?(?:,? (\d{4}))?(?:,? (.+))?$", "");

enum Local {
    NotLocal,
    Invalid,
    At(f64),
}

fn with_time(d: &CalDay, t: TimeOfDay, tz: &str) -> f64 {
    zoned_to_utc_ms(d.year, d.month, d.day, t.hour as f64, t.minute as f64, tz)
}

/// The /timestamp-only forms, read in `tz`.
fn parse_local(s: &str, tz: &str, now_ms: f64) -> Local {
    if s == "now" {
        return Local::At(now_ms);
    }
    let today = zoned_parts_ms(now_ms, tz);

    if let Some((day, time)) = split_day(s, |p| day_word_offset(p).is_some()) {
        let Some(time) = time_or_midnight(time.as_deref()) else { return Local::Invalid };
        let d = add_days(today.year, today.month, today.day, day_word_offset(&day).unwrap_or(0));
        return Local::At(with_time(&d, time, tz));
    }

    if let Some((day, time)) = split_day(s, is_weekday_part) {
        let Some((t, m)) = WEEKDAY_RE.exec(&day) else { return Local::Invalid };
        let which = m.group(&t, 1);
        let name = m.group(&t, 2).unwrap_or_default();
        let Some(time) = time_or_midnight(time.as_deref()) else { return Local::Invalid };
        let target = day_alias(&name).unwrap_or(0);
        let first = if which.as_deref() == Some("next") { 1 } else { 0 };
        for k in first..=7 {
            let d = add_days(today.year, today.month, today.day, k);
            if d.weekday != target {
                continue;
            }
            let t = with_time(&d, time, tz);
            if k > 0 || t > now_ms {
                return Local::At(t);
            }
        }
        return Local::Invalid;
    }

    let md = LOCAL_MD1.exec(s).or_else(|| LOCAL_MD2.exec(s));
    if let Some((t, m)) = md {
        let g1 = m.group(&t, 1).unwrap_or_default();
        let g2 = m.group(&t, 2).unwrap_or_default();
        let day_first = g1.as_bytes().first().is_some_and(u8::is_ascii_digit);
        let month_word = if day_first { &g2 } else { &g1 };
        if let Some(month) = month_number(month_word) {
            let day = num(if day_first { &g1 } else { &g2 });
            let month = month as f64;
            let Some(time) = time_or_midnight(m.group(&t, 4).as_deref()) else { return Local::Invalid };
            if day < 1.0 {
                return Local::Invalid;
            }
            if let Some(y) = m.group(&t, 3) {
                let year = num(&y);
                if year < 1970.0 || day > days_in_month_f64(year, month) {
                    return Local::Invalid;
                }
                return Local::At(zoned_to_utc_ms(year, month, day, time.hour as f64, time.minute as f64, tz));
            }
            let past = month * 100.0 + day < today.month as f64 * 100.0 + today.day as f64;
            let mut year = today.year as f64 + if past { 1.0 } else { 0.0 };
            for _ in 0..=8 {
                if day <= days_in_month_f64(year, month) {
                    return Local::At(zoned_to_utc_ms(year, month, day, time.hour as f64, time.minute as f64, tz));
                }
                year += 1.0;
            }
            return Local::Invalid;
        }
    }
    Local::NotLocal
}

/// The result of [`parse_when_ms`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedWhen {
    /// Epoch milliseconds.
    pub date_ms: f64,
    /// The zone the text was read in.
    pub zone: String,
    /// True when the text itself named the zone.
    pub zone_from_input: bool,
}

impl ParsedWhen {
    /// The instant as a `DateTime`.
    pub fn date(&self) -> Option<DateTime<Utc>> {
        from_ms(self.date_ms)
    }
}

static WS_RUN: LazyRe = lazy_re!(r"\s+", "");
static UTC_SPACE_SIGN: LazyRe = lazy_re!(r"\b(utc|gmt) ([+\-−])", "i");
static AT_START: LazyRe = lazy_re!(r"^at ", "i");
static AT_MID: LazyRe = lazy_re!(r" at ", "i");

/// Parses a time typed for /timestamp: everything [`parse_date_input_ms`] reads, plus 'now',
/// 'today|tomorrow|yesterday [time]', '[next] fri [time]' (also '7pm fri'), 'oct 20 [2027] [time]', '20 oct [time]',
/// an optional 'at', and an optional last word naming the zone (an abbreviation, 'utc', 'utc+2' or Region/City).
/// A day with no time means midnight; past times are returned as they are.
pub fn parse_when_ms(input: &str, tz: &str, now_ms: f64) -> Option<ParsedWhen> {
    let mut s = WS_RUN.replace_all(js::trim(input), " ");
    if s.is_empty() || js::utf16_len(&s) > 100 || !now_ms.is_finite() {
        return None;
    }
    let mut zone = if is_valid_time_zone(tz) { tz.to_string() } else { "UTC".to_string() };
    let mut zone_from_input = false;
    s = UTC_SPACE_SIGN.replace_all_with(&s, |m, t| format!("{}{}", m.group(t, 1).unwrap_or_default(), m.group(t, 2).unwrap_or_default()));
    if let Some(last) = s.rfind(' ')
        && last > 0
            && let Some(named) = zone_token(&s[last + 1..]) {
                zone = named;
                zone_from_input = true;
                s.truncate(last);
            }
    s = AT_START.replace_first_with(&s, |_, _| String::new());
    s = AT_MID.replace_first_with(&s, |_, _| " ".to_string());

    let ms = match parse_local(&s.to_lowercase(), &zone, now_ms) {
        Local::At(t) => Some(t),
        Local::Invalid => None,
        Local::NotLocal => parse_date_input_ms(&s, &zone, now_ms),
    }?;
    if !ms.is_finite() || ms < 0.0 || ms > LATEST_MS {
        return None;
    }
    Some(ParsedWhen { date_ms: ms, zone, zone_from_input })
}

/// [`parse_when_ms`] with a `DateTime` clock.
pub fn parse_when(input: &str, tz: &str, now: &DateTime<Utc>) -> Option<ParsedWhen> {
    parse_when_ms(input, tz, to_ms(now))
}

/// Seconds east of UTC of a zone at an instant (None for an id ICU would refuse).
pub fn offset_seconds_at(ms: f64, tz: &str) -> Option<i32> {
    let (_, zone) = tzdata::resolve(tz)?;
    Some(tzdata::offset_seconds(zone, ms))
}
