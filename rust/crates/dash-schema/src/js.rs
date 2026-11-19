//! JavaScript value semantics the descriptors rely on: numbers as JS numbers (an integral float is an integer and is
//! written without a fraction), `String(x)`, `Number(x)`, strict equality and own-key order.

use serde_json::{Map, Value};

/// A JSON number for the JS number `n`: integral values (and -0) become integers, so `1.0` is written `1` as
/// `JSON.stringify` does. NaN and the infinities become null (what `JSON.stringify` writes for them).
pub fn js_num(n: f64) -> Value {
    if !n.is_finite() {
        return Value::Null;
    }
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        return Value::from(n as i64);
    }
    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
}

/// The number of a JSON value, when it is one.
pub fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// `Number.isInteger(v)`.
pub fn is_integer(v: &Value) -> bool {
    matches!(v.as_f64(), Some(n) if n.is_finite() && n.fract() == 0.0)
}

/// `typeof v === 'number' && Number.isFinite(v)`.
pub fn is_finite_number(v: &Value) -> bool {
    matches!(v.as_f64(), Some(n) if n.is_finite())
}

/// Every number inside `v` in its canonical JS form (see [`js_num`]).
pub fn canon(v: Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if !(n.is_i64() || n.is_u64()) => js_num(f),
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(canon).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, canon(v))).collect()),
        other => other,
    }
}

/// `String(v)` (undefined is `None`).
pub fn to_js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { to_js_string(Some(x)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `String(v ?? '')`.
pub fn to_js_string_or_empty(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        other => to_js_string(other),
    }
}

/// `Number(v)` (ToNumber; undefined is `None`).
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => string_to_number(s),
        Some(Value::Array(a)) => match a.as_slice() {
            [] => 0.0,
            [one] => to_number(Some(&Value::String(to_js_string_or_empty(Some(one))))),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// StringToNumber: decimal literals (with exponent), `Infinity`, and 0x / 0o / 0b integers; blank is 0.
pub fn string_to_number(s: &str) -> f64 {
    let t = bot_util::js::trim(s);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || !rest.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return rest.chars().fold(0.0, |acc, c| acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)));
        }
    }
    let (sign, body) = match t.as_bytes()[0] {
        b'+' => (1.0, &t[1..]),
        b'-' => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    // StrUnsignedDecimalLiteral: digits [. digits] [e[+-]digits] or . digits [exponent].
    let b = body.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let int_digits = digits(&mut i);
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        frac_digits = digits(&mut i);
    }
    if int_digits == 0 && frac_digits == 0 {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return f64::NAN;
        }
    }
    if i != b.len() {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// `a === b` for JSON values (objects and arrays are never equal: they would be different references).
pub fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

/// JS truthiness of an optional value (undefined is falsy).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// True for a canonical array index ("0", "17", but not "01" or "-1"): such keys come first in `Object.keys`.
fn array_index(k: &str) -> Option<u32> {
    if k.is_empty() || (k.len() > 1 && k.starts_with('0')) || !k.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    k.parse::<u32>().ok().filter(|&n| n != u32::MAX)
}

/// `Object.keys(o)` of an object parsed by `JSON.parse`: array-index keys in ascending order, then the others in
/// insertion order.
pub fn js_keys(o: &Map<String, Value>) -> Vec<&String> {
    let mut idx: Vec<(u32, &String)> = o.keys().filter_map(|k| array_index(k).map(|n| (n, k))).collect();
    idx.sort_by_key(|(n, _)| *n);
    let mut out: Vec<&String> = idx.into_iter().map(|(_, k)| k).collect();
    out.extend(o.keys().filter(|k| array_index(k).is_none()));
    out
}

/// `Math.round(n * 100) / 100`.
pub fn round2(n: f64) -> f64 {
    js_round(n * 100.0) / 100.0
}

/// `Math.round`: the closest integer, halves rounded towards +Infinity.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let r = x.floor();
    if x - r >= 0.5 { r + 1.0 } else { r }
}

/// `typeof v === 'string' && v.trim() !== ''`.
pub fn non_blank(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if !bot_util::js::trim(s).is_empty())
}
