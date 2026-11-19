//! Shared helpers of the feat-events tests.
#![allow(dead_code)]

use bot_core::Registry;
use bot_db::{GuildEvent, GuildSettings};
use serde_json::{Value, json};

pub fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_events::register(&mut reg);
    reg
}

/// The JS-produced reference data (`rust/fixtures/gen/feat-events-golden.mjs`).
pub fn golden() -> Value {
    bot_core::testing::load_golden("feat-events/golden.json")
}

/// A JS `GuildEvent` row (dates as ISO strings) as the Rust model.
pub fn event_of(v: &Value) -> GuildEvent {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("event row {v}: {e}"))
}

pub fn settings_of(v: &Value) -> Option<GuildSettings> {
    if v.is_null() {
        return None;
    }
    let mut s = GuildSettings { guild_id: "900000000000000777".into(), ..Default::default() };
    if let Some(c) = v.get("embedColor").and_then(Value::as_i64) {
        s.embed_color = Some(c as i32);
    }
    Some(s)
}

/// Drops what discord.js leaves out and serenity writes as defaults (`null`, `false` flags, `inline: false`,
/// `type: "rich"`, the embed timestamp), so the two JSON forms compare equal.
pub fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                let default_false = x == &json!(false) && matches!(k.as_str(), "inline" | "disabled" | "animated" | "required" | "tts");
                if x.is_null() || default_false || (k == "type" && x == "rich") || k == "timestamp" {
                    continue;
                }
                out.insert(k.clone(), norm(x));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        x => x.clone(),
    }
}

pub fn to_json<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).unwrap_or(Value::Null)
}

pub fn assert_json_eq(rust: &Value, js: &Value, what: &str) {
    let (a, b) = (norm(rust), norm(js));
    if a != b {
        panic!("{what} differs\n rust: {a}\n   js: {b}");
    }
}
