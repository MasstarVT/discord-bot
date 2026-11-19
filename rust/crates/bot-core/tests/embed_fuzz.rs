//! Differential fuzz of the pure template functions of customEmbed.js: rust/fixtures/golden/bot-core/embed-fuzz.json
//! (from rust/fixtures/gen/bot-core-embed-fuzz.mjs, seeded random templates, URLs and colours run through the JS)
//! replayed against `bot_core::custom_embed`.

use bot_core::custom_embed as ce;
use bot_core::testing::load_golden;
use serde_json::{Value, json};

fn golden() -> Value {
    load_golden("bot-core/embed-fuzz.json")
}

#[test]
fn random_templates_normalise_validate_and_import_like_the_js() {
    let g = golden();
    let mut failures = Vec::new();
    for (i, c) in g["templates"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        let input = &c["input"];
        let normalized = ce::normalize_template(input);
        if serde_json::to_string(&normalized).unwrap_or_default() != serde_json::to_string(&c["normalized"]).unwrap_or_default() {
            failures.push(format!("#{i} normalizeTemplate({input})\n  ours   {normalized}\n  theirs {}", c["normalized"]));
            continue;
        }
        if json!(ce::template_to_json(input)) != c["json"] {
            failures.push(format!("#{i} templateToJson({input})"));
        }
        for (loose, key) in [(false, "validate"), (true, "validateLoose")] {
            let v = ce::validate_template(input, !loose);
            let ours = json!({ "ok": v.ok, "errors": v.errors });
            if ours != c[key] {
                failures.push(format!("#{i} {key}({input})\n  ours   {ours}\n  theirs {}", c[key]));
            }
        }
        let (tpl, errors) = ce::template_from_json(&serde_json::to_string(input).unwrap_or_default());
        let ours = json!({ "template": tpl, "errors": errors });
        if ours != c["fromJson"] {
            failures.push(format!("#{i} templateFromJson({input})\n  ours   {ours}\n  theirs {}", c["fromJson"]));
        }
    }
    for c in g["urls"].as_array().cloned().unwrap_or_default() {
        let u = c["u"].as_str().unwrap_or_default();
        if json!(ce::is_http_url(u)) != c["ok"] {
            failures.push(format!("isHttpUrl({u:?}) ours {} theirs {}", ce::is_http_url(u), c["ok"]));
        }
    }
    for c in g["colors"].as_array().cloned().unwrap_or_default() {
        let ours = json!(ce::parse_color(&c["c"]));
        if ours != c["n"] {
            failures.push(format!("parseColor({}) ours {ours} theirs {}", c["c"], c["n"]));
        }
    }
    let shown: Vec<String> = failures.iter().take(25).map(|s| s.chars().take(1500).collect()).collect();
    assert!(failures.is_empty(), "{} cases differ from the JS:\n{}", failures.len(), shown.join("\n"));
}
