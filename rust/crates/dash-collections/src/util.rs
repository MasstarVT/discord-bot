//! Small JS-semantics helpers shared by the collection files. Rows are Prisma-shaped JSON (`serde_json::to_value` of a
//! `bot_db::models` struct): dates are ISO strings, absent optionals `null` or missing.

use bot_util::js::{date_parse, to_iso_string, to_locale_en_us, utf16_len, utf16_prefix};
use dash_schema::js::to_js_string;
use serde_json::{Map, Value};

pub use dash_schema::js::{strict_eq, truthy};

/// `/^\d{17,20}$/.test(String(v))` for a string.
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `/^\d{17,20}$/.test(String(v))` for any value (numbers are written as JS would).
pub fn is_snowflake_value(v: Option<&Value>) -> bool {
    match v {
        Some(Value::String(s)) => is_snowflake(s),
        Some(v @ Value::Number(_)) => is_snowflake(&to_js_string(Some(v))),
        _ => false,
    }
}

/// The epoch ms of a stored date (an ISO string, or a number of ms); None when it is not a valid date.
pub fn date_ms(v: Option<&Value>) -> Option<f64> {
    let t = match v? {
        Value::String(s) => date_parse(s),
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    t.is_finite().then_some(t)
}

/// `toIso(d)` / `iso(d)`: a valid date as `2026-01-02T03:04:05.678Z`, else null.
pub fn iso(v: Option<&Value>) -> Value {
    date_ms(v).and_then(to_iso_string).map_or(Value::Null, Value::String)
}

/// `iso(d)` as an Option.
pub fn iso_string(v: Option<&Value>) -> Option<String> {
    date_ms(v).and_then(to_iso_string)
}

/// `utc(d)`: "2026-01-02 03:04 UTC", or '?' for an invalid date.
pub fn utc_text(v: Option<&Value>) -> String {
    match iso_string(v) {
        Some(s) => format!("{} UTC", s[..16].replace('T', " ")),
        None => "?".into(),
    }
}

/// `Number(n ?? 0).toLocaleString('en-US')`.
pub fn fmt_num(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "∞".into() } else { "-∞".into() };
    }
    to_locale_en_us(n, 3)
}

/// `Number(v ?? 0)` of a JSON value.
pub fn number_of(v: Option<&Value>) -> f64 {
    match v {
        None | Some(Value::Null) => 0.0,
        Some(v) => dash_schema::js::to_number(Some(v)),
    }
}

/// `Array.isArray(v) ? v : []`.
pub fn as_list(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

/// `{ ...current, ...data }` (current may be absent).
pub fn merged(current: Option<&Map<String, Value>>, data: &Map<String, Value>) -> Map<String, Value> {
    let mut out = current.cloned().unwrap_or_default();
    for (k, v) in data {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// `Number.isInteger(v)`.
pub fn is_integer(v: Option<&Value>) -> bool {
    v.is_some_and(dash_schema::js::is_integer)
}

/// The value of an integer JSON number.
pub fn int_of(v: Option<&Value>) -> Option<i64> {
    v.filter(|v| dash_schema::js::is_integer(v)).and_then(Value::as_f64).map(|f| f as i64)
}

/// `v ?? null` for a map entry.
pub fn or_null(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

/// `(v ?? null)` as an owned value where undefined and null both read as null.
pub fn nullish(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// `String(v)` of a JSON value (undefined: 'undefined').
pub fn js_string(v: Option<&Value>) -> String {
    to_js_string(v)
}

/// `s.slice(0, n)` in UTF-16 units.
pub fn slice16(s: &str, n: usize) -> &str {
    utf16_prefix(s, n)
}

/// `s.length` in UTF-16 units.
pub fn len16(s: &str) -> usize {
    utf16_len(s)
}

/// `typeof v === 'string' && v.trim() !== ''`.
pub fn non_empty(v: Option<&Value>) -> bool {
    dash_schema::js::non_blank(v)
}

/// A string field, when it is a string.
pub fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

/// `(a ?? null) === (b ?? null)`.
pub fn same_nullish(a: Option<&Value>, b: Option<&Value>) -> bool {
    strict_eq(a.unwrap_or(&Value::Null), b.unwrap_or(&Value::Null))
}

/// `new Set(list)` membership with `===` semantics.
pub fn list_has(list: &[Value], v: &Value) -> bool {
    list.iter().any(|x| strict_eq(x, v))
}

/// `s.replace(/^./, (c) => c.toUpperCase())` for the first UTF-16 unit (BMP letters as JS uppercases them).
pub fn upper_first(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c != '\n' && c != '\r' && c != '\u{2028}' && c != '\u{2029}' => {
            let up: String = c.to_uppercase().collect();
            format!("{up}{}", it.as_str())
        }
        _ => s.to_string(),
    }
}

/// A `Map<String, Value>` from a JSON object value (empty for anything else).
pub fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Marks a JS `undefined` inside a value built with `json!` (see [`strip_undefined`]).
const UNDEFINED_MARK: &str = "\u{0}\u{1}undefined";

/// `row.x` as the JS copies it: a missing key stays undefined, which [`strip_undefined`] then drops like
/// `JSON.stringify` does.
pub fn raw(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or_else(|| Value::String(UNDEFINED_MARK.into()))
}

/// What `JSON.stringify` makes of a value holding undefined: object keys whose value is undefined are dropped, array
/// entries that are undefined become null.
pub fn strip_undefined(v: Value) -> Value {
    let is_undef = |v: &Value| matches!(v, Value::String(s) if s == UNDEFINED_MARK);
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !is_undef(v)).map(|(k, v)| (k, strip_undefined(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| if is_undef(&v) { Value::Null } else { strip_undefined(v) }).collect()),
        other => other,
    }
}
