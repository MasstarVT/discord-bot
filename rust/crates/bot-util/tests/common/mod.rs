//! Shared by the bot-util tests: loads the JS-produced golden files and decodes the JS values they encode
//! (see rust/fixtures/gen/bot-util-encode.mjs).
#![allow(dead_code)]

use bot_util::js;
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/bot-util")
}

/// Every `[args, result]` pair recorded for `key` ("module.fn"), from tables.json and recorded.json.
pub fn cases(key: &str) -> Vec<(Vec<Value>, Value, &'static str)> {
    let mut out = Vec::new();
    for (file, src) in [("tables.json", "tables"), ("recorded.json", "recorded"), ("extra.json", "extra"), ("fuzz.json", "fuzz")] {
        let path = golden_dir().join(file);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
        if let Some(list) = v.get(key).and_then(Value::as_array) {
            for pair in list {
                let args = pair[0].as_array().cloned().unwrap_or_default();
                out.push((args, pair[1].clone(), src));
            }
        }
    }
    out
}

/// All keys present in the golden files.
pub fn all_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for file in ["tables.json", "recorded.json", "extra.json", "fuzz.json"] {
        if let Ok(text) = std::fs::read_to_string(golden_dir().join(file)) {
            let v: Value = serde_json::from_str(&text).expect("golden json");
            if let Some(o) = v.as_object() {
                for k in o.keys() {
                    if !keys.contains(k) {
                        keys.push(k.clone());
                    }
                }
            }
        }
    }
    keys
}

// ── Decoding ──────────────────────────────────────────────────────────────────

pub fn is_undef(v: &Value) -> bool {
    v.get("$u").is_some()
}

pub fn is_nullish(v: Option<&Value>) -> bool {
    match v {
        None => true,
        Some(v) => v.is_null() || is_undef(v),
    }
}

pub fn has_u16(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.contains_key("$u16") || o.values().any(has_u16),
        Value::Array(a) => a.iter().any(has_u16),
        _ => false,
    }
}

/// A string value (lone surrogates replaced, as `toWellFormed` does).
pub fn as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) if o.contains_key("$u16") => {
            let units: Vec<u16> = o["$u16"].as_array()?.iter().filter_map(|x| x.as_u64().map(|n| n as u16)).collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// JS `String(v)`.
pub fn string_of(v: &Value) -> String {
    if let Some(s) = as_string(v) {
        return s;
    }
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(_) => js::number_to_string(num(v)),
        Value::Array(a) => a.iter().map(|x| if x.is_null() || is_undef(x) { String::new() } else { string_of(x) }).collect::<Vec<_>>().join(","),
        Value::Object(o) => {
            if o.contains_key("$u") {
                "undefined".into()
            } else if let Some(n) = o.get("$n") {
                n.as_str().unwrap_or("NaN").replace("-0", "0")
            } else if let Some(b) = o.get("$big") {
                b.as_str().unwrap_or("0").to_string()
            } else {
                "[object Object]".into()
            }
        }
        Value::String(_) => unreachable!(),
    }
}

/// JS `ToNumber(v)`.
pub fn num(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => str_to_number(s),
        Value::Array(a) => match a.len() {
            0 => 0.0,
            1 => num(&Value::String(string_of(&a[0]))),
            _ => f64::NAN,
        },
        Value::Object(o) => {
            if let Some(n) = o.get("$n").and_then(Value::as_str) {
                match n {
                    "NaN" => f64::NAN,
                    "Infinity" => f64::INFINITY,
                    "-Infinity" => f64::NEG_INFINITY,
                    "-0" => -0.0,
                    _ => f64::NAN,
                }
            } else if let Some(d) = o.get("$d") {
                d.as_f64().unwrap_or(f64::NAN)
            } else if let Some(b) = o.get("$big") {
                b.as_str().and_then(|s| s.parse().ok()).unwrap_or(f64::NAN)
            } else {
                f64::NAN
            }
        }
    }
}

pub fn str_to_number(s: &str) -> f64 {
    let t = js::trim(s);
    if t.is_empty() {
        return 0.0;
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        t.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// JS truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Object(o) => {
            if o.contains_key("$u") {
                false
            } else if o.contains_key("$n") {
                !matches!(o["$n"].as_str(), Some("NaN" | "-0"))
            } else {
                true
            }
        }
        Value::Array(_) => true,
    }
}

pub fn is_number(v: &Value) -> bool {
    v.is_number() || v.get("$n").is_some()
}

/// A Date or a number as epoch ms (`date instanceof Date ? getTime() : Number(date)`).
pub fn ms_of(v: &Value) -> f64 {
    num(v)
}

pub fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(json!({"$u": 1}))
}

pub fn opt_field(v: &Value, k: &str) -> Option<Value> {
    v.get(k).filter(|x| !is_undef(x)).cloned()
}

// ── Encoding ──────────────────────────────────────────────────────────────────

pub fn enc_num(n: f64) -> Value {
    if n.is_nan() {
        json!({"$n": "NaN"})
    } else if n.is_infinite() {
        json!({"$n": if n > 0.0 { "Infinity" } else { "-Infinity" }})
    } else if n == 0.0 && n.is_sign_negative() {
        json!({"$n": "-0"})
    } else {
        json!(n)
    }
}

pub fn enc_date(ms: f64) -> Value {
    if ms.is_finite() { json!({"$d": ms}) } else { json!({"$d": null}) }
}

pub fn undef() -> Value {
    json!({"$u": 1})
}

/// Deep equality with numbers compared as f64 and object keys in any order.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
        _ => a == b,
    }
}

/// Collects mismatches for one function and fails with the first few.
pub struct Report {
    pub key: String,
    pub total: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

impl Report {
    pub fn new(key: &str) -> Self {
        Self { key: key.into(), total: 0, skipped: 0, failures: Vec::new() }
    }
}
