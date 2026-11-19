//! Small JS-semantics helpers shared by the server and the Discord client.

use bot_util::js::{trim, utf16_prefix};
use serde_json::Value;

/// `/^\d{17,20}$/`.
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `forLog`: control characters removed, at most 100 UTF-16 units.
pub fn for_log(s: &str) -> String {
    let cleaned: String = s.chars().filter(|&c| !matches!(c as u32, 0..=0x1f | 0x7f)).collect();
    utf16_prefix(&cleaned, 100).to_string()
}

/// A search text from a query string: control characters removed, trimmed, at most `max` UTF-16 units
/// (`.replace(/[\u0000-\u001f\u007f]/g, '').trim().slice(0, max)`).
pub fn clean_query(raw: &str, max: usize) -> String {
    let cleaned: String = raw.chars().filter(|&c| !matches!(c as u32, 0..=0x1f | 0x7f)).collect();
    utf16_prefix(trim(&cleaned), max).to_string()
}

/// `URLSearchParams(query)` as a list of pairs (`+` is a space, percent escapes decoded, bad UTF-8 replaced).
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// `params.get(name)`: the first value.
pub fn query_get<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// `URLSearchParams(pairs).toString()`.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// `Number.isInteger(v)` for a JSON value, as an i64 (None for anything else, or out of range).
pub fn js_int(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18).then_some(f as i64)
}

/// JS `BigInt(value)` of a JSON value; None where BigInt throws (the callers then use 0n).
pub fn js_bigint(v: &Value) -> Option<i128> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(i128::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(i128::from(i));
            }
            if let Some(u) = n.as_u64() {
                return Some(i128::from(u));
            }
            let f = n.as_f64()?;
            (f.is_finite() && f.fract() == 0.0 && f.abs() < 1.0e38).then_some(f as i128)
        }
        Value::String(s) => bigint_from_str(s),
        Value::Array(a) => {
            // BigInt([x]) is BigInt(String([x])).
            let joined: Vec<String> = a
                .iter()
                .map(|x| match x {
                    Value::Null => String::new(),
                    Value::String(s) => s.clone(),
                    other => dash_schema::js::to_js_string(Some(other)),
                })
                .collect();
            bigint_from_str(&joined.join(","))
        }
        Value::Object(_) => None,
    }
}

/// `BigInt(string)`: StringToBigInt (whitespace trimmed, empty is 0n, 0x/0o/0b prefixes, a sign only for decimals).
pub fn bigint_from_str(s: &str) -> Option<i128> {
    let t = trim(s);
    if t.is_empty() {
        return Some(0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            return i128::from_str_radix(digits, radix).ok();
        }
    }
    let (neg, digits) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i128 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Compares two snowflake strings as numbers (`compareIds` with BigInt).
pub fn compare_ids(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u128>(), b.parse::<u128>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

/// `typeof v === 'string' ? v : null`.
pub fn str_or_null(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}
