//! JavaScript semantics the JS utilities rely on implicitly: UTF-16 string lengths, `String.prototype.trim`,
//! `Number.prototype.toString`, `toLocaleString('en-US')`, `Date.UTC` and the `Date` field maths, and
//! `encodeURIComponent`. Every other module builds on these so a port reads like the JS it mirrors.

/// Length of `s` in UTF-16 code units (JS `string.length`).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, n)` in UTF-16 code units. A cut through a surrogate pair drops that whole character (JS would keep a
/// lone surrogate, which a Rust `String` cannot hold); callers that care check [`cuts_surrogate`] first.
pub fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let l = c.len_utf16();
        if used + l > n {
            return &s[..i];
        }
        used += l;
    }
    s
}

/// True when `s.slice(0, n)` would end in the first half of a surrogate pair.
pub fn cuts_surrogate(s: &str, n: usize) -> bool {
    let mut used = 0;
    for c in s.chars() {
        let l = c.len_utf16();
        if used + l > n {
            return used < n;
        }
        used += l;
    }
    false
}

/// `s.slice(start, end)` in UTF-16 code units, both clamped to the length (whole characters only).
pub fn utf16_slice(s: &str, start: usize, end: usize) -> &str {
    let tail = utf16_suffix_from(s, start);
    utf16_prefix(tail, end.saturating_sub(start))
}

/// `s.slice(start)` in UTF-16 code units (whole characters only).
pub fn utf16_suffix_from(s: &str, start: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        if used >= start {
            return &s[i..];
        }
        used += c.len_utf16();
    }
    ""
}

/// Byte offset of UTF-16 offset `n` in `s` (clamped to the end).
pub fn utf16_to_byte(s: &str, n: usize) -> usize {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        if used >= n {
            return i;
        }
        used += c.len_utf16();
    }
    s.len()
}

/// UTF-16 offset of byte offset `b` in `s`.
pub fn byte_to_utf16(s: &str, b: usize) -> usize {
    utf16_len(&s[..b.min(s.len())])
}

/// The characters JS `\s` and `String.prototype.trim` treat as white space (WhiteSpace and LineTerminator).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `String.prototype.trimEnd`.
pub fn trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_space)
}

/// `String.prototype.trimStart`.
pub fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_space)
}

/// `String.prototype.toLowerCase` (full Unicode lower-casing, as Rust's).
pub fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// Shortest round-trip decimal digits of a finite, non-zero `|v|` and the exponent `n` such that
/// `v = 0.d1d2d3… × 10^n` (the `k` digits and `n` of ECMAScript Number::toString).
fn shortest_digits(v: f64) -> (Vec<u8>, i32) {
    let s = format!("{:e}", v.abs());
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
    let mut digits = digits;
    while digits.len() > 1 && *digits.last().unwrap_or(&1) == 0 {
        digits.pop();
    }
    (digits, exp + 1)
}

/// ECMAScript `Number.prototype.toString()` (radix 10): `String(-0)` is "0", `1e21` is "1e+21".
pub fn number_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sign = if v < 0.0 { "-" } else { "" };
    let (d, n) = shortest_digits(v);
    let k = d.len() as i32;
    let ds: String = d.iter().map(|x| (b'0' + x) as char).collect();
    let body = if k <= n && n <= 21 {
        format!("{ds}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &ds[..n as usize], &ds[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{ds}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let es = if e >= 0 { format!("+{e}") } else { format!("-{}", -e) };
        if k == 1 { format!("{ds}e{es}") } else { format!("{}.{}e{es}", &ds[..1], &ds[1..]) }
    };
    format!("{sign}{body}")
}

/// Groups an ASCII digit string with commas (en-US).
fn group3(int: &str) -> String {
    let b = int.as_bytes();
    let mut out = String::with_capacity(b.len() + b.len() / 3);
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

/// `v.toLocaleString('en-US', { maximumFractionDigits })` for a finite `v`: ICU rounds the shortest decimal form of
/// the double half away from zero, groups by three, and keeps the sign of -0 ("-0").
pub fn to_locale_en_us(v: f64, max_fraction_digits: u32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let neg = v.is_sign_negative();
    if v == 0.0 {
        return if neg { "-0".into() } else { "0".into() };
    }
    let (d, n) = shortest_digits(v);
    // Decimal digits as integer part / fraction part.
    let mut int_digits: Vec<u8> = Vec::new();
    let mut frac_digits: Vec<u8> = Vec::new();
    if n > 0 {
        for i in 0..n as usize {
            int_digits.push(*d.get(i).unwrap_or(&0));
        }
        if (n as usize) < d.len() {
            frac_digits.extend_from_slice(&d[n as usize..]);
        }
    } else {
        int_digits.push(0);
        frac_digits.extend(std::iter::repeat_n(0, (-n) as usize));
        frac_digits.extend_from_slice(&d);
    }
    let m = max_fraction_digits as usize;
    if frac_digits.len() > m {
        let round_up = frac_digits[m] >= 5;
        frac_digits.truncate(m);
        if round_up {
            // Propagate the carry through fraction then integer digits.
            let mut carry = true;
            for x in frac_digits.iter_mut().rev() {
                if *x == 9 {
                    *x = 0;
                } else {
                    *x += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                for x in int_digits.iter_mut().rev() {
                    if *x == 9 {
                        *x = 0;
                    } else {
                        *x += 1;
                        carry = false;
                        break;
                    }
                }
                if carry {
                    int_digits.insert(0, 1);
                }
            }
        }
    }
    while frac_digits.last() == Some(&0) {
        frac_digits.pop();
    }
    while int_digits.len() > 1 && int_digits[0] == 0 {
        int_digits.remove(0);
    }
    let int: String = int_digits.iter().map(|x| (b'0' + x) as char).collect();
    let mut out = String::new();
    // A value that rounds to zero keeps its sign in ICU ("-0").
    if neg {
        out.push('-');
    }
    out.push_str(&group3(&int));
    if !frac_digits.is_empty() {
        out.push('.');
        out.extend(frac_digits.iter().map(|x| (b'0' + x) as char));
    }
    out
}

/// en-US grouping of an integer given as decimal text (BigInt `toLocaleString('en-US')`).
pub fn group_integer_text(s: &str) -> String {
    let (sign, digits) = s.strip_prefix('-').map_or(("", s), |d| ("-", d));
    format!("{sign}{}", group3(digits))
}

/// `encodeURIComponent` of a well-formed string.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Length of `encodeURIComponent(s)`.
pub fn encoded_len(s: &str) -> usize {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
                1
            } else {
                3
            }
        })
        .sum()
}

// ── Date maths (ECMAScript §21.4.1) ────────────────────────────────────────────

/// Milliseconds per day.
pub const MS_PER_DAY: f64 = 86_400_000.0;
/// The largest |time value| a JS Date can hold.
pub const MAX_TIME: f64 = 8.64e15;

fn day_from_year(y: f64) -> f64 {
    365.0 * (y - 1970.0) + ((y - 1969.0) / 4.0).floor() - ((y - 1901.0) / 100.0).floor() + ((y - 1601.0) / 400.0).floor()
}

fn days_in_year(y: f64) -> f64 {
    if y % 4.0 != 0.0 {
        365.0
    } else if y % 100.0 != 0.0 {
        366.0
    } else if y % 400.0 != 0.0 {
        365.0
    } else {
        366.0
    }
}

fn to_integer(v: f64) -> f64 {
    if v.is_nan() { 0.0 } else { v.trunc() }
}

/// MakeDay(year, month, date): month may be out of range; NaN for non-finite input.
pub fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let y = to_integer(year);
    let m = to_integer(month);
    let dt = to_integer(date);
    let ym = y + (m / 12.0).floor();
    if ym.abs() > 400_000.0 {
        return f64::NAN;
    }
    let mn = m.rem_euclid(12.0);
    let leap = days_in_year(ym) == 366.0;
    const CUM: [f64; 12] = [0.0, 31.0, 59.0, 90.0, 120.0, 151.0, 181.0, 212.0, 243.0, 273.0, 304.0, 334.0];
    let mut day = day_from_year(ym) + CUM[mn as usize];
    if leap && mn >= 2.0 {
        day += 1.0;
    }
    day + dt - 1.0
}

/// MakeTime(hour, min, sec, ms).
pub fn make_time(h: f64, m: f64, s: f64, ms: f64) -> f64 {
    if !h.is_finite() || !m.is_finite() || !s.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    to_integer(h) * 3_600_000.0 + to_integer(m) * 60_000.0 + to_integer(s) * 1000.0 + to_integer(ms)
}

/// TimeClip.
pub fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > MAX_TIME { f64::NAN } else { to_integer(t) + 0.0 }
}

/// `Date.UTC(year, month0, day, hour, minute, second, ms)`, including the 0..99 → 1900s year rule.
pub fn date_utc(year: f64, month0: f64, day: f64, hour: f64, minute: f64, second: f64, ms: f64) -> f64 {
    let mut y = year;
    if y.is_finite() {
        let yi = to_integer(y);
        if (0.0..=99.0).contains(&yi) {
            y = 1900.0 + yi;
        }
    }
    time_clip(make_day(y, month0, day) * MS_PER_DAY + make_time(hour, minute, second, ms))
}

/// UTC fields of a time value (as `getUTC*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcFields {
    pub year: i64,
    /// 0-11
    pub month0: u32,
    pub day: u32,
    /// 0 = Sunday
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub ms: u32,
}

/// The UTC fields of a finite time value (any finite f64; not clipped).
pub fn utc_fields(t: f64) -> UtcFields {
    let day = (t / MS_PER_DAY).floor();
    let in_day = t - day * MS_PER_DAY;
    let mut y = (day / 365.2425).floor() + 1970.0;
    while day_from_year(y) > day {
        y -= 1.0;
    }
    while day_from_year(y + 1.0) <= day {
        y += 1.0;
    }
    let doy = day - day_from_year(y);
    let leap = days_in_year(y) == 366.0;
    let lens = [31.0, if leap { 29.0 } else { 28.0 }, 31.0, 30.0, 31.0, 30.0, 31.0, 31.0, 30.0, 31.0, 30.0, 31.0];
    let mut m = 0usize;
    let mut rest = doy;
    while m < 11 && rest >= lens[m] {
        rest -= lens[m];
        m += 1;
    }
    let weekday = ((day + 4.0).rem_euclid(7.0)) as u32;
    let in_day = in_day as i64;
    UtcFields {
        year: y as i64,
        month0: m as u32,
        day: rest as u32 + 1,
        weekday,
        hour: (in_day / 3_600_000) as u32,
        minute: ((in_day / 60_000) % 60) as u32,
        second: ((in_day / 1000) % 60) as u32,
        ms: (in_day % 1000) as u32,
    }
}

/// `Date.prototype.toISOString`; None for an invalid date (JS throws a RangeError).
pub fn to_iso_string(t: f64) -> Option<String> {
    let t = time_clip(t);
    if t.is_nan() {
        return None;
    }
    let f = utc_fields(t);
    let year = if (0..=9999).contains(&f.year) {
        format!("{:04}", f.year)
    } else if f.year < 0 {
        format!("-{:06}", -f.year)
    } else {
        format!("+{:06}", f.year)
    };
    Some(format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        f.month0 + 1,
        f.day,
        f.hour,
        f.minute,
        f.second,
        f.ms
    ))
}

/// Milliseconds since the epoch, now (`Date.now()`).
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// `Date.parse(s)` as V8 implements it ([`crate::v8_date::date_parse`]): the ES5 ISO format (`2026-01-02T03:04:05.678Z`;
/// date-only is UTC, a zone-less date-time is local time, i.e. `TZ` or UTC), then the legacy forms (RFC 1123/850,
/// asctime, zone names, offsets, month names, ...). NaN when V8 returns NaN.
pub fn date_parse(s: &str) -> f64 {
    crate::v8_date::date_parse(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string_matches_js() {
        for (v, s) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (1.5, "1.5"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (123.456, "123.456"),
            (-12.5, "-12.5"),
            (f64::NAN, "NaN"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.2345e25, "1.2345e+25"),
        ] {
            assert_eq!(number_to_string(v), s, "{v}");
        }
    }

    #[test]
    fn date_utc_and_iso() {
        assert_eq!(date_utc(2026.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0), 1_767_225_600_000.0);
        assert_eq!(to_iso_string(0.0).as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert_eq!(date_utc(5.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0), date_utc(1905.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0));
        assert_eq!(utc_fields(-1.0).year, 1969);
        assert_eq!(date_parse("Wed, 21 Oct 2015 07:28:00 GMT"), 1_445_412_480_000.0);
        assert_eq!(date_parse("2015-10-21T07:28:00.000Z"), 1_445_412_480_000.0);
    }
}
