//! Port of `src/dashboard/collections/translateStats.js`: translation activity per channel (TranslateStat counters),
//! busiest channel first. Read-only.

use std::cmp::Ordering;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::collections::small::{describe_languages, language_name};
use crate::util::{date_ms, fmt_num, iso, js_string};

/// Languages listed per line before "and N more".
const SHOWN_LANGUAGES: usize = 8;
const INT_MAX: f64 = 2_147_483_647.0;

fn language_label(code: &str) -> String {
    if code == "und" {
        return "Unknown".into();
    }
    language_name(code).map_or_else(|| code.to_string(), str::to_string)
}

/// `a.localeCompare(b)` for language codes and ids (ASCII): case-insensitive first, then lower case before upper.
pub(crate) fn locale_compare(a: &str, b: &str) -> Ordering {
    a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then_with(|| {
        for (x, y) in a.chars().zip(b.chars()) {
            if x != y {
                return if x.is_lowercase() { Ordering::Less } else { Ordering::Greater };
            }
        }
        a.len().cmp(&b.len())
    })
}

/// "Spanish 62% (80) · English 30% (38)" for code -> count, busiest first.
fn language_line(counts: &IndexMap<String, f64>) -> String {
    let total: f64 = counts.values().sum();
    if total == 0.0 {
        return "None yet".into();
    }
    let mut sorted: Vec<(&String, f64)> = counts.iter().map(|(k, v)| (k, *v)).collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal).then_with(|| locale_compare(a.0, b.0)));
    let mut parts: Vec<String> = sorted
        .iter()
        .take(SHOWN_LANGUAGES)
        .map(|(code, n)| format!("{} {}% ({})", language_label(code), dash_schema::js::js_round((n / total) * 100.0), fmt_num(*n)))
        .collect();
    if sorted.len() > SHOWN_LANGUAGES {
        parts.push(format!("and {} more", sorted.len() - SHOWN_LANGUAGES));
    }
    parts.join(" · ")
}

struct Entry {
    channel_id: String,
    from: IndexMap<String, f64>,
    to: IndexMap<String, f64>,
    auto: f64,
    command: f64,
    first_at: Option<Value>,
    last_at: Option<Value>,
    targets: Option<Vec<Value>>,
}

/// `earlier(a, b)` / `later(a, b)` of two dates (a missing one loses).
fn pick(a: Option<Value>, b: Option<&Value>, later: bool) -> Option<Value> {
    let b = b.filter(|v| crate::util::truthy(Some(v))).cloned();
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(a), Some(b)) => {
            let (x, y) = (date_ms(Some(&a)).unwrap_or(f64::NAN), date_ms(Some(&b)).unwrap_or(f64::NAN));
            let take_b = if later { y > x } else { y < x };
            Some(if take_b { b } else { a })
        }
    }
}

/// `buildStatItems(rows, configured)`: TranslateStat rows (`{ channelId, via, direction, lang, count, firstAt, lastAt }`)
/// and the server's auto-translate channels (`{ channelId, targetLangs }`) as list items, busiest first.
pub fn build_stat_items(rows: &[Value], configured: &[Value]) -> Vec<Value> {
    let mut by_channel: IndexMap<String, Entry> = IndexMap::new();
    fn entry<'a>(map: &'a mut IndexMap<String, Entry>, id: &str) -> &'a mut Entry {
        map.entry(id.to_string()).or_insert_with(|| Entry {
            channel_id: id.to_string(),
            from: IndexMap::new(),
            to: IndexMap::new(),
            auto: 0.0,
            command: 0.0,
            first_at: None,
            last_at: None,
            targets: None,
        })
    }
    for c in configured {
        let id = js_string(c.get("channelId"));
        entry(&mut by_channel, &id).targets = Some(c.get("targetLangs").and_then(Value::as_array).cloned().unwrap_or_default());
    }
    for r in rows {
        let e = entry(&mut by_channel, &js_string(r.get("channelId")));
        let n = {
            let x = crate::util::number_of(r.get("count"));
            if x.is_nan() { 0.0 } else { x }
        };
        let lang = js_string(r.get("lang"));
        match r.get("direction").and_then(Value::as_str) {
            Some("from") => {
                *e.from.entry(lang).or_insert(0.0) += n;
                match r.get("via").and_then(Value::as_str) {
                    Some("auto") => e.auto += n,
                    Some("command") => e.command += n,
                    _ => {}
                }
            }
            Some("to") => *e.to.entry(lang).or_insert(0.0) += n,
            _ => {}
        }
        e.first_at = pick(e.first_at.take(), r.get("firstAt"), false);
        e.last_at = pick(e.last_at.take(), r.get("lastAt"), true);
    }
    let mut items: Vec<(f64, Value)> = by_channel
        .into_values()
        .map(|e| {
            let total = e.auto + e.command;
            let mut top: Vec<(&String, f64)> = e.from.iter().filter(|(c, _)| c.as_str() != "und").map(|(c, n)| (c, *n)).collect();
            top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
            let top: Vec<String> = top.iter().take(3).map(|(c, _)| language_label(c)).collect();
            let summary = if total == 0.0 {
                "No translations yet".to_string()
            } else {
                let mut parts = vec![format!("{} message{}", fmt_num(total), if total == 1.0 { "" } else { "s" })];
                if !top.is_empty() {
                    parts.push(format!("mostly {}", top.join(", ")));
                }
                parts.join(" · ")
            };
            let auto_translate = match &e.targets {
                Some(t) if !t.is_empty() => describe_languages(t),
                _ => "Off".to_string(),
            };
            let item = json!({
                "id": e.channel_id,
                "channelId": e.channel_id,
                "summary": summary,
                "wroteIn": language_line(&e.from),
                "translatedInto": language_line(&e.to),
                "autoMessages": dash_schema::js::js_num(e.auto.min(INT_MAX)),
                "commandUses": dash_schema::js::js_num(e.command.min(INT_MAX)),
                "autoTranslate": auto_translate,
                "firstAt": iso(e.first_at.as_ref()),
                "lastAt": iso(e.last_at.as_ref()),
            });
            (total, item)
        })
        .collect();
    items.sort_by(|(ta, a), (tb, b)| {
        let la = a["lastAt"].as_str().unwrap_or("");
        let lb = b["lastAt"].as_str().unwrap_or("");
        tb.partial_cmp(ta)
            .unwrap_or(Ordering::Equal)
            .then_with(|| locale_compare(lb, la))
            .then_with(|| locale_compare(a["channelId"].as_str().unwrap_or(""), b["channelId"].as_str().unwrap_or("")))
    });
    items.into_iter().map(|(_, v)| v).collect()
}
