//! Port of `src/utils/parseDuration.js`: duration parsing and formatting. Pure; nothing panics.

use crate::js;
use crate::jsre::{u16s, LazyRe};
use crate::lazy_re;

/// Longest accepted duration: 100 years. Larger inputs parse to None.
pub const MAX_DURATION_MS: i64 = 100 * 365 * 86_400_000;

// Sticky tokenizer, copied from the JS: the whole input must be consumed. `months?|mos?` is tried before `m`, and a
// unit may not be followed by another letter, so '5ms' fails and '1 month' is 30 days.
static TOKEN_RE: LazyRe = lazy_re!(
    r"(\d+(?:\.\d+)?|\.\d+)\s*(years?|yrs?|y|months?|mos?|weeks?|wks?|w|days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)(?![a-z])",
    "i"
);
static SEP_RE: LazyRe = lazy_re!(r"(?:\s+|,|\band\b)*", "i");

fn unit_ms(unit: &str) -> f64 {
    let u = unit.to_lowercase();
    let key = if u.starts_with("mo") {
        "mo"
    } else if u == "m" || u.starts_with("mi") {
        "m"
    } else {
        &u[..1]
    };
    match key {
        "y" => 31_536_000_000.0,
        "mo" => 2_592_000_000.0,
        "w" => 604_800_000.0,
        "d" => 86_400_000.0,
        "h" => 3_600_000.0,
        "m" => 60_000.0,
        _ => 1_000.0,
    }
}

/// Parses a human-readable duration ("1h", "1.5h", "2h30m", "1 month", "5 minutes and 3s") into milliseconds,
/// 1 ..= [`MAX_DURATION_MS`]. Only whitespace, ',' and 'and' may separate tokens; anything else gives None.
pub fn parse_duration(input: &str) -> Option<i64> {
    let s = js::trim(input);
    let t = u16s(s);
    if t.is_empty() || t.len() > 100 {
        return None;
    }
    let mut pos = 0;
    let mut sum = 0.0f64;
    while pos < t.len() {
        if pos > 0 {
            if let Some(m) = SEP_RE.sticky_at(&t, pos) {
                pos = m.end;
            }
            if pos >= t.len() {
                break;
            }
        }
        let m = TOKEN_RE.sticky_at(&t, pos)?;
        let num: f64 = m.group(&t, 1)?.parse().ok()?;
        sum += num * unit_ms(&m.group(&t, 2)?);
        pos = m.end;
    }
    let total = crate::text::js_round(sum);
    if total.is_finite() && total > 0.0 && total <= MAX_DURATION_MS as f64 { Some(total as i64) } else { None }
}

/// Formats milliseconds like "2h 30m"; 'Permanent' for 0 or less.
pub fn format_duration(ms: f64) -> String {
    if ms.is_nan() || ms <= 0.0 {
        return "Permanent".into();
    }
    let mut ms = ms;
    let mut parts = Vec::new();
    for (unit, label) in [(604_800_000.0, "w"), (86_400_000.0, "d"), (3_600_000.0, "h"), (60_000.0, "m"), (1_000.0, "s")] {
        let count = (ms / unit).floor();
        if count > 0.0 {
            parts.push(format!("{}{label}", js::number_to_string(count)));
            ms %= unit;
        }
    }
    if parts.is_empty() { "0s".into() } else { parts.join(" ") }
}

/// Formats milliseconds in words, e.g. 9_000_000 → "2 hours 30 minutes"; 'Permanent' for 0, negative or non-finite.
pub fn format_duration_long(ms: f64) -> String {
    if ms.is_nan() || !ms.is_finite() || ms <= 0.0 {
        return "Permanent".into();
    }
    let mut secs = (ms / 1000.0).floor();
    let mut parts = Vec::new();
    for (size, label) in [
        (31_536_000.0, "year"),
        (2_592_000.0, "month"),
        (604_800.0, "week"),
        (86_400.0, "day"),
        (3_600.0, "hour"),
        (60.0, "minute"),
        (1.0, "second"),
    ] {
        let n = (secs / size).floor();
        if n > 0.0 {
            parts.push(format!("{} {label}{}", js::number_to_string(n), if n == 1.0 { "" } else { "s" }));
            secs -= n * size;
        }
    }
    if parts.is_empty() { "0 seconds".into() } else { parts.join(" ") }
}
