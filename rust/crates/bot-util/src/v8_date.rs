//! `Date.parse(string)` as V8 implements it (`src/date/dateparser*.{h,cc}`): the ES5 ISO format first, then the
//! legacy parser that accepts RFC 822 dates, zone names (`EST`, `GMT+2`), `+0000` offsets, month names, AM/PM, two-digit
//! years and the like (ported for feed dates in feat-feeds, shared here). [`crate::js::date_parse`] uses it as its
//! fallback after its ISO and HTTP-date fast paths.
//!
//! A string without a zone is local time in V8. The bot runs in UTC (Docker); here local time is the `TZ` environment
//! variable when it names an IANA zone, else UTC.

use chrono::TimeZone as _;

const NONE: i64 = i64::MAX;
const MAX_SIGNIFICANT_DIGITS: usize = 9;
const SMI_MAX: i64 = i32::MAX as i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KwType {
    Invalid,
    MonthName,
    TimeZoneName,
    TimeSeparator,
    AmPm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Invalid,
    Unknown,
    Number { n: i64, len: usize },
    Symbol(u16),
    WhiteSpace,
    Keyword { kind: KwType, value: i64, len: usize },
    End,
}

impl Tok {
    fn is_number(self) -> bool {
        matches!(self, Tok::Number { .. })
    }
    fn number(self) -> i64 {
        if let Tok::Number { n, .. } = self { n } else { 0 }
    }
    fn is_fixed(self, l: usize) -> bool {
        matches!(self, Tok::Number { len, .. } if len == l)
    }
    fn is_symbol(self, c: u8) -> bool {
        self == Tok::Symbol(u16::from(c))
    }
    fn is_sign(self) -> bool {
        self.is_symbol(b'+') || self.is_symbol(b'-')
    }
    fn sign(self) -> i64 {
        if self.is_symbol(b'-') { -1 } else { 1 }
    }
    fn is_keyword_type(self, t: KwType) -> bool {
        matches!(self, Tok::Keyword { kind, .. } if kind == t)
    }
    fn is_keyword_z(self) -> bool {
        matches!(self, Tok::Keyword { kind: KwType::TimeZoneName, len: 1, value: 0 })
    }
}

const KEYWORDS: [([u16; 3], KwType, i64); 26] = {
    const fn k(a: u8, b: u8, c: u8) -> [u16; 3] {
        [a as u16, b as u16, c as u16]
    }
    [
        (k(b'j', b'a', b'n'), KwType::MonthName, 1),
        (k(b'f', b'e', b'b'), KwType::MonthName, 2),
        (k(b'm', b'a', b'r'), KwType::MonthName, 3),
        (k(b'a', b'p', b'r'), KwType::MonthName, 4),
        (k(b'm', b'a', b'y'), KwType::MonthName, 5),
        (k(b'j', b'u', b'n'), KwType::MonthName, 6),
        (k(b'j', b'u', b'l'), KwType::MonthName, 7),
        (k(b'a', b'u', b'g'), KwType::MonthName, 8),
        (k(b's', b'e', b'p'), KwType::MonthName, 9),
        (k(b'o', b'c', b't'), KwType::MonthName, 10),
        (k(b'n', b'o', b'v'), KwType::MonthName, 11),
        (k(b'd', b'e', b'c'), KwType::MonthName, 12),
        (k(b'a', b'm', 0), KwType::AmPm, 0),
        (k(b'p', b'm', 0), KwType::AmPm, 12),
        (k(b'u', b't', 0), KwType::TimeZoneName, 0),
        (k(b'u', b't', b'c'), KwType::TimeZoneName, 0),
        (k(b'z', 0, 0), KwType::TimeZoneName, 0),
        (k(b'g', b'm', b't'), KwType::TimeZoneName, 0),
        (k(b'c', b'd', b't'), KwType::TimeZoneName, -5),
        (k(b'c', b's', b't'), KwType::TimeZoneName, -6),
        (k(b'e', b'd', b't'), KwType::TimeZoneName, -4),
        (k(b'e', b's', b't'), KwType::TimeZoneName, -5),
        (k(b'm', b'd', b't'), KwType::TimeZoneName, -6),
        (k(b'm', b's', b't'), KwType::TimeZoneName, -7),
        (k(b'p', b'd', b't'), KwType::TimeZoneName, -7),
        (k(b'p', b's', b't'), KwType::TimeZoneName, -8),
    ]
};

fn lookup(prefix: [u16; 3], len: usize) -> (KwType, i64) {
    for (p, kind, value) in KEYWORDS {
        if p == prefix && (len <= 3 || kind == KwType::MonthName) {
            return (kind, value);
        }
    }
    if prefix == [u16::from(b't'), 0, 0] && len <= 3 {
        return (KwType::TimeSeparator, 0);
    }
    (KwType::Invalid, 0)
}

fn is_ws(c: u16) -> bool {
    char::from_u32(u32::from(c)).is_some_and(crate::js::is_js_space)
}

struct Scanner {
    s: Vec<u16>,
    pos: usize,
    next: Tok,
}

impl Scanner {
    fn new(s: &str) -> Self {
        let mut sc = Scanner { s: s.encode_utf16().collect(), pos: 0, next: Tok::End };
        sc.next = sc.scan();
        sc
    }

    fn ch(&self) -> u16 {
        self.s.get(self.pos).copied().unwrap_or(0)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.s.len()
    }

    fn scan(&mut self) -> Tok {
        if self.at_end() {
            return Tok::End;
        }
        let pre = self.pos;
        let c = self.ch();
        if (u16::from(b'0')..=u16::from(b'9')).contains(&c) {
            let mut n = 0i64;
            let mut i = 0;
            while !self.at_end() && (u16::from(b'0')..=u16::from(b'9')).contains(&self.ch()) {
                if i < MAX_SIGNIFICANT_DIGITS {
                    n = n * 10 + i64::from(self.ch() - u16::from(b'0'));
                }
                i += 1;
                self.pos += 1;
            }
            return Tok::Number { n, len: self.pos - pre };
        }
        for sym in *b":-+.)" {
            if c == u16::from(sym) {
                self.pos += 1;
                return Tok::Symbol(c);
            }
        }
        if c >= u16::from(b'A') && !is_ws(c) {
            let mut prefix = [0u16; 3];
            let mut i = 0;
            while !self.at_end() && self.ch() >= u16::from(b'A') && !is_ws(self.ch()) {
                if i < 3 {
                    prefix[i] = self.ch() | 0x20;
                }
                i += 1;
                self.pos += 1;
            }
            let (kind, value) = lookup(prefix, i);
            return Tok::Keyword { kind, value, len: i };
        }
        if is_ws(c) {
            self.pos += 1;
            return Tok::WhiteSpace;
        }
        if c == u16::from(b'(') {
            let mut balance = 0;
            loop {
                let ch = self.ch();
                if ch == u16::from(b')') {
                    balance -= 1;
                } else if ch == u16::from(b'(') {
                    balance += 1;
                }
                self.pos += 1;
                if !(balance > 0 && self.ch() != 0) {
                    break;
                }
            }
            return Tok::Unknown;
        }
        self.pos += 1;
        Tok::Unknown
    }

    fn next(&mut self) -> Tok {
        let r = self.next;
        self.next = self.scan();
        r
    }

    fn peek(&self) -> Tok {
        self.next
    }

    fn skip_symbol(&mut self, c: u8) -> bool {
        if self.next.is_symbol(c) {
            self.next();
            true
        } else {
            false
        }
    }
}

fn between(x: i64, lo: i64, hi: i64) -> bool {
    x >= lo && x <= hi
}

#[derive(Default)]
struct TzComp {
    sign: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
}

impl TzComp {
    fn set(&mut self, offset_hours: i64) {
        self.sign = Some(if offset_hours < 0 { -1 } else { 1 });
        self.hour = Some(offset_hours.abs());
        self.minute = Some(0);
    }
    fn is_expecting(&self, n: i64) -> bool {
        self.hour.is_some() && self.minute.is_none() && between(n, 0, 59)
    }
    fn is_utc(&self) -> bool {
        self.hour == Some(0) && self.minute == Some(0)
    }
    fn is_empty(&self) -> bool {
        self.hour.is_none()
    }
    /// The offset in seconds; `Some(None)` is local time; `None` is a failure.
    fn write(&self) -> Option<Option<i64>> {
        let Some(sign) = self.sign else { return Some(None) };
        let hour = self.hour.unwrap_or(0);
        let minute = match self.minute {
            Some(m) if m != NONE => m,
            _ => 0,
        };
        // Unsigned 32-bit arithmetic, as V8 does it.
        let total = (hour as u32).wrapping_mul(3600).wrapping_add((minute as u32).wrapping_mul(60));
        if i64::from(total) > SMI_MAX {
            return None;
        }
        Some(Some(i64::from(total) * sign))
    }
}

struct TimeComp {
    comp: [i64; 4],
    index: usize,
    hour_offset: Option<i64>,
}

impl TimeComp {
    fn new() -> Self {
        Self { comp: [0; 4], index: 0, hour_offset: None }
    }
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn is_expecting(&self, n: i64) -> bool {
        (self.index == 1 && between(n, 0, 59)) || (self.index == 2 && between(n, 0, 59)) || (self.index == 3 && between(n, 0, 999))
    }
    fn add(&mut self, n: i64) -> bool {
        if self.index < 4 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn add_final(&mut self, n: i64) -> bool {
        if !self.add(n) {
            return false;
        }
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        true
    }
    fn write(&mut self) -> Option<[i64; 4]> {
        while self.index < 4 {
            self.comp[self.index] = 0;
            self.index += 1;
        }
        let [mut hour, minute, second, ms] = self.comp;
        if let Some(off) = self.hour_offset {
            if !between(hour, 0, 12) {
                return None;
            }
            hour %= 12;
            hour += off;
        }
        // A 24th hour is allowed when minutes, seconds and milliseconds are 0.
        let valid = between(hour, 0, 23) && between(minute, 0, 59) && between(second, 0, 59) && between(ms, 0, 999);
        if !valid && (hour != 24 || minute != 0 || second != 0 || ms != 0) {
            return None;
        }
        Some([hour, minute, second, ms])
    }
}

struct DayComp {
    comp: [i64; 3],
    index: usize,
    named_month: Option<i64>,
    iso: bool,
}

impl DayComp {
    fn new() -> Self {
        Self { comp: [0; 3], index: 0, named_month: None, iso: false }
    }
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn add(&mut self, n: i64) -> bool {
        if self.index < 3 {
            self.comp[self.index] = n;
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn write(&mut self) -> Option<[i64; 3]> {
        if self.index < 1 {
            return None;
        }
        while self.index < 3 {
            self.comp[self.index] = 1;
            self.index += 1;
        }
        let mut year = 0;
        let month;
        let day;
        match self.named_month {
            None => {
                if self.iso || (self.index == 3 && !between(self.comp[0], 1, 31)) {
                    year = self.comp[0];
                    month = self.comp[1];
                    day = self.comp[2];
                } else {
                    month = self.comp[0];
                    day = self.comp[1];
                    if self.index == 3 {
                        year = self.comp[2];
                    }
                }
            }
            Some(m) => {
                month = m;
                if self.index == 1 {
                    day = self.comp[0];
                } else if !between(self.comp[0], 1, 31) {
                    year = self.comp[0];
                    day = self.comp[1];
                } else {
                    day = self.comp[0];
                    year = self.comp[1];
                }
            }
        }
        if !self.iso {
            if between(year, 0, 49) {
                year += 2000;
            } else if between(year, 50, 99) {
                year += 1900;
            }
        }
        if !(-SMI_MAX - 1..=SMI_MAX).contains(&year) || !between(month, 1, 12) || !between(day, 1, 31) {
            return None;
        }
        Some([year, month - 1, day])
    }
}

fn read_milliseconds(t: Tok) -> i64 {
    let Tok::Number { n, len } = t else { return 0 };
    let mut number = n;
    let mut length = len;
    if length < 3 {
        if length == 1 {
            number *= 100;
        } else if length == 2 {
            number *= 10;
        }
    } else if length > 3 {
        if length > MAX_SIGNIFICANT_DIGITS {
            length = MAX_SIGNIFICANT_DIGITS;
        }
        let mut factor = 1;
        while length > 3 {
            factor *= 10;
            length -= 1;
        }
        number /= factor;
    }
    number
}

fn parse_es5(sc: &mut Scanner, day: &mut DayComp, time: &mut TimeComp, tz: &mut TzComp) -> Tok {
    if sc.peek().is_sign() {
        let sign_token = sc.next();
        if !sc.peek().is_fixed(6) {
            return sign_token;
        }
        let sign = sign_token.sign();
        let year = sc.next().number();
        if sign < 0 && year == 0 {
            return sign_token;
        }
        day.add(sign * year);
    } else if sc.peek().is_fixed(4) {
        let y = sc.next().number();
        day.add(y);
    } else {
        return sc.next();
    }
    if sc.skip_symbol(b'-') {
        if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 1, 12) {
            return sc.next();
        }
        let m = sc.next().number();
        day.add(m);
        if sc.skip_symbol(b'-') {
            if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 1, 31) {
                return sc.next();
            }
            let d = sc.next().number();
            day.add(d);
        }
    }
    if !sc.peek().is_keyword_type(KwType::TimeSeparator) {
        if sc.peek() != Tok::End {
            return sc.next();
        }
    } else {
        sc.next();
        if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 0, 24) {
            return Tok::Invalid;
        }
        let hour_is_24 = sc.peek().number() == 24;
        let h = sc.next().number();
        time.add(h);
        if !sc.skip_symbol(b':') {
            return Tok::Invalid;
        }
        if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 0, 59) || (hour_is_24 && sc.peek().number() > 0) {
            return Tok::Invalid;
        }
        let m = sc.next().number();
        time.add(m);
        if sc.skip_symbol(b':') {
            if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 0, 59) || (hour_is_24 && sc.peek().number() > 0) {
                return Tok::Invalid;
            }
            let s = sc.next().number();
            time.add(s);
            if sc.skip_symbol(b'.') {
                if !sc.peek().is_number() || (hour_is_24 && sc.peek().number() > 0) {
                    return Tok::Invalid;
                }
                let ms = read_milliseconds(sc.next());
                time.add(ms);
            }
        }
        if sc.peek().is_keyword_z() {
            sc.next();
            tz.set(0);
        } else if sc.peek().is_sign() {
            tz.sign = Some(sc.next().sign());
            if sc.peek().is_fixed(4) {
                let hm = sc.next().number();
                let (h, m) = (hm / 100, hm % 100);
                if !between(h, 0, 23) || !between(m, 0, 59) {
                    return Tok::Invalid;
                }
                tz.hour = Some(h);
                tz.minute = Some(m);
            } else {
                if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 0, 23) {
                    return Tok::Invalid;
                }
                tz.hour = Some(sc.next().number());
                if !sc.skip_symbol(b':') {
                    return Tok::Invalid;
                }
                if !sc.peek().is_fixed(2) || !between(sc.peek().number(), 0, 59) {
                    return Tok::Invalid;
                }
                tz.minute = Some(sc.next().number());
            }
        }
        if sc.peek() != Tok::End {
            return Tok::Invalid;
        }
    }
    if tz.is_empty() && time.is_empty() {
        tz.set(0);
    }
    day.iso = true;
    Tok::End
}

/// The fields V8's parser produces: `[year, month0, day, hour, minute, second, ms]` and the UTC offset in seconds
/// (`None`: local time).
fn parse_fields(s: &str) -> Option<([i64; 7], Option<i64>)> {
    let mut sc = Scanner::new(s);
    let mut tz = TzComp::default();
    let mut time = TimeComp::new();
    let mut day = DayComp::new();
    let first = parse_es5(&mut sc, &mut day, &mut time, &mut tz);
    if first == Tok::Invalid {
        return None;
    }
    let mut has_read_number = !day.is_empty();
    let mut token = first;
    while token != Tok::End {
        match token {
            Tok::Number { n, .. } => {
                has_read_number = true;
                if sc.skip_symbol(b':') {
                    if sc.skip_symbol(b':') {
                        if !time.is_empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        if !time.add(n) {
                            return None;
                        }
                        if sc.peek().is_symbol(b'.') {
                            sc.next();
                        }
                    }
                } else if sc.skip_symbol(b'.') && time.is_expecting(n) {
                    time.add(n);
                    if !sc.peek().is_number() {
                        return None;
                    }
                    let ms = read_milliseconds(sc.next());
                    if ms < 0 {
                        return None;
                    }
                    time.add_final(ms);
                } else if tz.is_expecting(n) {
                    tz.minute = Some(n);
                } else if time.is_expecting(n) {
                    time.add_final(n);
                    let p = sc.peek();
                    if p != Tok::End && p != Tok::WhiteSpace && !p.is_keyword_z() && !p.is_sign() {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    sc.skip_symbol(b'-');
                }
            }
            Tok::Keyword { kind, value, .. } => {
                if kind == KwType::AmPm && !time.is_empty() {
                    time.hour_offset = Some(value);
                } else if kind == KwType::MonthName {
                    day.named_month = Some(value);
                    sc.skip_symbol(b'-');
                } else if kind == KwType::TimeZoneName && has_read_number {
                    tz.set(value);
                } else {
                    if has_read_number {
                        return None;
                    }
                    if sc.peek().is_number() {
                        return None;
                    }
                }
            }
            t if t.is_sign() && (tz.is_utc() || !time.is_empty()) => {
                tz.sign = Some(t.sign());
                let mut n = 0;
                let mut length = 0;
                if sc.peek().is_number() {
                    let nt = sc.next();
                    if let Tok::Number { n: v, len } = nt {
                        n = v;
                        length = len;
                    }
                }
                has_read_number = true;
                if sc.peek().is_symbol(b':') {
                    tz.hour = Some(n);
                    tz.minute = None;
                } else if length == 1 || length == 2 {
                    tz.hour = Some(n);
                    tz.minute = Some(0);
                } else if length == 3 || length == 4 {
                    tz.hour = Some(n / 100);
                    tz.minute = Some(n % 100);
                } else {
                    return None;
                }
            }
            t if (t.is_sign() || t.is_symbol(b')')) && has_read_number => return None,
            _ => {}
        }
        token = sc.next();
    }
    let d = day.write()?;
    let t = time.write()?;
    let off = tz.write()?;
    Some(([d[0], d[1], d[2], t[0], t[1], t[2], t[3]], off))
}

/// The local zone used for strings without a zone (`TZ`, else UTC).
fn local_to_utc_ms(local_ms: f64) -> f64 {
    let Some(tz) = std::env::var("TZ").ok().and_then(|z| z.parse::<chrono_tz::Tz>().ok()) else { return local_ms };
    let secs = (local_ms / 1000.0).floor();
    let Some(naive) = chrono::DateTime::from_timestamp(secs as i64, 0).map(|d| d.naive_utc()) else { return local_ms };
    match tz.from_local_datetime(&naive).earliest() {
        Some(dt) => local_ms - (naive.and_utc().timestamp() - dt.timestamp()) as f64 * 1000.0,
        None => {
            // A gap: V8 uses the offset before the transition.
            let before = naive - chrono::Duration::hours(3);
            match tz.from_local_datetime(&before).earliest() {
                Some(dt) => local_ms - (before.and_utc().timestamp() - dt.timestamp()) as f64 * 1000.0,
                None => local_ms,
            }
        }
    }
}

/// `Date.parse(s)` in milliseconds, NaN when V8 returns NaN.
pub fn date_parse(s: &str) -> f64 {
    let Some((f, off)) = parse_fields(s) else { return f64::NAN };
    let day = crate::js::make_day(f[0] as f64, f[1] as f64, f[2] as f64);
    let time = crate::js::make_time(f[3] as f64, f[4] as f64, f[5] as f64, f[6] as f64);
    let mut date = day * crate::js::MS_PER_DAY + time;
    const LIMIT: f64 = 8.64e15 + 864e5 * 10.0;
    match off {
        None => {
            if !(-LIMIT..=LIMIT).contains(&date) {
                return f64::NAN;
            }
            date = local_to_utc_ms(date);
        }
        Some(sec) => {
            date -= sec as f64 * 1000.0;
            if !(-LIMIT..=LIMIT).contains(&date) {
                return f64::NAN;
            }
        }
    }
    crate::js::time_clip(date)
}

/// A date as a UTC `DateTime`, `None` for NaN.
pub fn parse_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let t = date_parse(s);
    if !t.is_finite() {
        return None;
    }
    chrono::DateTime::from_timestamp_millis(t as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_shapes() {
        let iso = |s: &str| parse_date(s).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        assert_eq!(iso("Fri, 09 Oct 2026 10:00:00 GMT").as_deref(), Some("2026-10-09T10:00:00.000Z"));
        assert_eq!(iso("2026-10-09T12:00:00+00:00").as_deref(), Some("2026-10-09T12:00:00.000Z"));
        assert_eq!(iso("Fri, 09 Oct 2026 10:00:00 EST").as_deref(), Some("2026-10-09T15:00:00.000Z"));
        assert_eq!(iso("Fri, 09 Oct 2026 10:00:00 +0200").as_deref(), Some("2026-10-09T08:00:00.000Z"));
        assert_eq!(iso("2026-10-09").as_deref(), Some("2026-10-09T00:00:00.000Z"));
        assert!(iso("soon").is_none());
    }
}
