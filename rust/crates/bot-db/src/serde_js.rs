//! serde helpers that read and write values exactly like the JS bot's `JSON.stringify` / `JSON.parse`.
//!
//! * Dates: written as `Date.prototype.toISOString()` (`2026-01-02T03:04:05.678Z`, always 3 fraction digits, `Z`);
//!   read leniently (any ISO 8601 / RFC 3339 form, with or without zone, `T` or space, date only, or epoch ms).
//! * Int: read from an integer or an integral float (`5` or `5.0`), clamped to `i32`.
//! * Float: written like JS (`1` for 1.0, `1.5`), read from any number.
//! * BigInt: written as a number (a string beyond 2^53, which JS cannot hold exactly), read from a number or a
//!   numeric string.
//!
//! Use with `#[serde(with = "bot_db::serde_js::date")]` and friends (`date_opt`, `date_vec`, `int`, ...).

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use serde::de::{self, Deserializer, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use std::fmt;

// ── Dates ────────────────────────────────────────────────────────────────────

/// `date.toISOString()` for a UTC instant (millisecond precision; extended years as `±YYYYYY` like JS).
pub fn format_js_date(dt: &DateTime<Utc>) -> String {
    use chrono::Datelike;
    let year = dt.year();
    let year_str = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -(year as i64))
    } else {
        format!("+{year:06}")
    };
    format!(
        "{year_str}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.timestamp_subsec_millis().min(999)
    )
}

/// Truncates to whole milliseconds (the precision of a JS Date and of `timestamp(3)`).
pub fn truncate_ms(dt: DateTime<Utc>) -> DateTime<Utc> {
    let ms = dt.timestamp_millis();
    Utc.timestamp_millis_opt(ms).single().unwrap_or(dt)
}

/// A UTC instant from epoch milliseconds (`new Date(ms)`).
pub fn from_epoch_ms(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

/// Parses a date string leniently. Accepted: `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, then optionally `T` or a space,
/// `HH:MM`, `:SS`, `.fraction` (any length, truncated to ms), then optionally `Z`, `±HH`, `±HHMM` or `±HH:MM`.
/// A missing zone means UTC (the JS bot only ever writes UTC). Extended years `±YYYYYY` are accepted.
pub fn parse_js_date(input: &str) -> Option<DateTime<Utc>> {
    let s = input.trim();
    let b = s.as_bytes();
    let mut i = 0usize;

    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        if *i + n > b.len() || !b[*i..*i + n].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = s[*i..*i + n].parse::<i64>().ok()?;
        *i += n;
        Some(v)
    };

    // Year.
    let year: i64 = if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        let neg = b[i] == b'-';
        i += 1;
        let y = digits(&mut i, 6)?;
        if neg { -y } else { y }
    } else {
        digits(&mut i, 4)?
    };
    let mut month = 1i64;
    let mut day = 1i64;
    if i < b.len() && b[i] == b'-' {
        i += 1;
        month = digits(&mut i, 2)?;
        if i < b.len() && b[i] == b'-' {
            i += 1;
            day = digits(&mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Like V8, a day past the end of the month (up to 31) rolls over into the next month.
    let first = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month as u32, 1)?;
    let date = first.checked_add_days(chrono::Days::new((day - 1) as u64))?;

    let (mut h, mut m, mut sec, mut nanos) = (0i64, 0i64, 0i64, 0i64);
    if i < b.len() && (b[i] == b'T' || b[i] == b't' || b[i] == b' ') {
        i += 1;
        h = digits(&mut i, 2)?;
        if i >= b.len() || b[i] != b':' {
            return None;
        }
        i += 1;
        m = digits(&mut i, 2)?;
        if i < b.len() && b[i] == b':' {
            i += 1;
            sec = digits(&mut i, 2)?;
            if i < b.len() && (b[i] == b'.' || b[i] == b',') {
                i += 1;
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let frac = &s[start..i];
                let ms_digits: String = frac.chars().chain("000".chars()).take(3).collect();
                nanos = ms_digits.parse::<i64>().ok()? * 1_000_000;
            }
        }
    }
    // JS accepts 24:00:00 as the end of the day.
    let mut extra_day = false;
    if h == 24 && m == 0 && sec == 0 && nanos == 0 {
        h = 0;
        extra_day = true;
    }
    let time = NaiveTime::from_hms_nano_opt(h as u32, m as u32, sec as u32, nanos as u32)?;

    // Zone.
    let mut offset_secs: i64 = 0;
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => i += 1,
            b'+' | b'-' => {
                let neg = b[i] == b'-';
                i += 1;
                let oh = digits(&mut i, 2)?;
                let mut om = 0;
                if i < b.len() && b[i] == b':' {
                    i += 1;
                    om = digits(&mut i, 2)?;
                } else if i + 2 <= b.len() && b[i].is_ascii_digit() {
                    om = digits(&mut i, 2)?;
                }
                if oh > 23 || om > 59 {
                    return None;
                }
                offset_secs = (oh * 3600 + om * 60) * if neg { -1 } else { 1 };
            }
            _ => return None,
        }
    }
    if i != b.len() {
        return None;
    }
    let mut naive = NaiveDateTime::new(date, time);
    if extra_day {
        naive = naive.checked_add_signed(chrono::Duration::days(1))?;
    }
    let utc = naive.checked_sub_signed(chrono::Duration::seconds(offset_secs))?;
    Some(truncate_ms(utc.and_utc()))
}

struct DateVisitor;

impl Visitor<'_> for DateVisitor {
    type Value = DateTime<Utc>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an ISO 8601 date string or epoch milliseconds")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        parse_js_date(v).ok_or_else(|| E::custom(format!("invalid date {v:?}")))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        from_epoch_ms(v).ok_or_else(|| E::custom("date out of range"))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        self.visit_i64(i64::try_from(v).map_err(|_| E::custom("date out of range"))?)
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        if !v.is_finite() {
            return Err(E::custom("invalid date"));
        }
        self.visit_i64(v.trunc() as i64)
    }
}

/// A `DateTime<Utc>` wrapper that (de)serialises the JS way; handy inside collections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JsDate(pub DateTime<Utc>);

impl Serialize for JsDate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_js_date(&self.0))
    }
}

impl<'de> Deserialize<'de> for JsDate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(DateVisitor).map(JsDate)
    }
}

/// `DateTime<Utc>` as a JS ISO string.
pub mod date {
    use super::*;
    pub fn serialize<S: Serializer>(v: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_js_date(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        d.deserialize_any(DateVisitor)
    }
}

/// `Option<DateTime<Utc>>`: `null` (or absent, with `#[serde(default)]`) is `None`.
pub mod date_opt {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(dt) => s.serialize_str(&format_js_date(dt)),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        Ok(Option::<JsDate>::deserialize(d)?.map(|v| v.0))
    }
}

/// `Vec<DateTime<Utc>>`.
pub mod date_vec {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[DateTime<Utc>], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for dt in v {
            seq.serialize_element(&JsDate(*dt))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<DateTime<Utc>>, D::Error> {
        Ok(Option::<Vec<JsDate>>::deserialize(d)?.unwrap_or_default().into_iter().map(|v| v.0).collect())
    }
}

// ── Numbers ──────────────────────────────────────────────────────────────────

/// Clamps an integer into `i32` (Prisma `Int`).
pub fn clamp_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Clamps a float into `i32` if it is integral; `None` otherwise.
pub fn integral_f64_to_i32(v: f64) -> Option<i32> {
    if !v.is_finite() || v.fract() != 0.0 {
        return None;
    }
    Some(v.clamp(i32::MIN as f64, i32::MAX as f64) as i32)
}

struct IntVisitor;

impl Visitor<'_> for IntVisitor {
    type Value = i32;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an integer")
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<i32, E> {
        Ok(clamp_i32(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<i32, E> {
        Ok(clamp_i32(i64::try_from(v).unwrap_or(i64::MAX)))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<i32, E> {
        integral_f64_to_i32(v).ok_or_else(|| E::custom(format!("{v} is not an integer")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsInt(i32);
impl<'de> Deserialize<'de> for JsInt {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(IntVisitor).map(JsInt)
    }
}

/// `i32` (Prisma `Int`).
pub mod int {
    use super::*;
    pub fn serialize<S: Serializer>(v: &i32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i32(*v)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
        d.deserialize_any(IntVisitor)
    }
}

/// `Option<i32>`.
pub mod int_opt {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<i32>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => s.serialize_i32(*n),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
        Ok(Option::<JsInt>::deserialize(d)?.map(|v| v.0))
    }
}

/// `Vec<i32>` (Prisma `Int[]`).
pub mod int_vec {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[i32], s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i32>, D::Error> {
        Ok(Option::<Vec<JsInt>>::deserialize(d)?.unwrap_or_default().into_iter().map(|v| v.0).collect())
    }
}

/// Serialises a float like `JSON.stringify` (integral values without a fraction, non-finite as `null`).
pub fn serialize_js_float<S: Serializer>(v: f64, s: S) -> Result<S::Ok, S::Error> {
    if !v.is_finite() {
        return s.serialize_none();
    }
    if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        if v == 0.0 {
            return s.serialize_i64(0); // JS writes -0 as 0
        }
        return s.serialize_i64(v as i64);
    }
    s.serialize_f64(v)
}

struct FloatVisitor;
impl Visitor<'_> for FloatVisitor {
    type Value = f64;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number")
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
        Ok(v as f64)
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
        Ok(v as f64)
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
        Ok(v)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct JsFloat(f64);
impl<'de> Deserialize<'de> for JsFloat {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(FloatVisitor).map(JsFloat)
    }
}
impl Serialize for JsFloat {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_js_float(self.0, s)
    }
}

/// `f64` (Prisma `Float`).
pub mod float {
    use super::*;
    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        serialize_js_float(*v, s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        d.deserialize_any(FloatVisitor)
    }
}

/// `Option<f64>`.
pub mod float_opt {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => serialize_js_float(*n, s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(Option::<JsFloat>::deserialize(d)?.map(|v| v.0))
    }
}

/// `Vec<f64>`.
pub mod float_vec {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[f64], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for n in v {
            seq.serialize_element(&JsFloat(*n))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
        Ok(Option::<Vec<JsFloat>>::deserialize(d)?.unwrap_or_default().into_iter().map(|v| v.0).collect())
    }
}

const MAX_SAFE: i64 = 9_007_199_254_740_991;

struct BigIntVisitor;
impl Visitor<'_> for BigIntVisitor {
    type Value = i64;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an integer or a numeric string")
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
        Ok(v)
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
        i64::try_from(v).map_err(|_| E::custom("BigInt out of range"))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
        if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.3e18 {
            Ok(v as i64)
        } else {
            Err(E::custom(format!("{v} is not an integer")))
        }
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
        v.trim().trim_end_matches('n').parse::<i64>().map_err(|_| E::custom(format!("invalid BigInt {v:?}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsBigInt(i64);
impl<'de> Deserialize<'de> for JsBigInt {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(BigIntVisitor).map(JsBigInt)
    }
}
impl Serialize for JsBigInt {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.abs() <= MAX_SAFE {
            s.serialize_i64(self.0)
        } else {
            s.serialize_str(&self.0.to_string())
        }
    }
}

/// `i64` (Prisma `BigInt`; the schema has none today).
pub mod bigint {
    use super::*;
    pub fn serialize<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
        JsBigInt(*v).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        d.deserialize_any(BigIntVisitor)
    }
}

/// `Option<i64>`.
pub mod bigint_opt {
    use super::*;
    pub fn serialize<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => JsBigInt(*n).serialize(s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        Ok(Option::<JsBigInt>::deserialize(d)?.map(|v| v.0))
    }
}

/// `Vec<i64>`.
pub mod bigint_vec {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[i64], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for n in v {
            seq.serialize_element(&JsBigInt(*n))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i64>, D::Error> {
        Ok(Option::<Vec<JsBigInt>>::deserialize(d)?.unwrap_or_default().into_iter().map(|v| v.0).collect())
    }
}
