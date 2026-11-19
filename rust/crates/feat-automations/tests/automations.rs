//! Port of `test/automations.test.js` and the automation cases of `test/review-automations.test.js`, plus the golden,
//! database and cross-language checks every feature crate runs.
//!
//! Offline: sweeps of every pure helper against `rust/fixtures/golden/feat-automations/pure.json` (written by the JS
//! service), the trackers, the plugin and command payload, and the runtime (dispatch, loop protection, the action
//! runners) against a fake guild and a fake REST server. With `TEST_DATABASE_URL` (and `TEST_REDIS_URL`): the data
//! layer, `/automation` through the router, and both cross-language directions.

use std::collections::HashMap;
use std::sync::Arc;

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, load_golden, redis_url, run_node};
use feat_automations::automation_service::{self as svc, AutomationLite, Base, ByTrigger, ListKind};
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_automations::register(&mut reg);
    reg
}

fn golden() -> Value {
    load_golden("feat-automations/pure.json")
}

fn s(v: &Value) -> String {
    serde_json::to_string(v).expect("serializes")
}

fn norm_json(r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => json!({ "ok": true, "value": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn opt_str(v: &Value) -> Option<&str> {
    v.as_str()
}

const fn id(n: u64) -> u64 {
    100_000_000_000_000_000 + n
}

fn sid(n: u64) -> String {
    id(n).to_string()
}

// ── Golden sweeps of the pure helpers ────────────────────────────────────────

#[test]
fn names_types_ids_and_words_match_the_js() {
    let g = golden();
    for case in arr(&g["names"]) {
        assert_eq!(svc::normalize_automation_name(case[0].as_str().unwrap_or("")), case[1].as_str().map(str::to_string), "{case}");
    }
    for case in arr(&g["types"]) {
        assert_eq!(svc::normalize_type_input(case[0].as_str().unwrap_or("")), case[1].as_str().unwrap_or(""), "{case}");
    }
    for case in arr(&g["idLists"]) {
        let kind = svc::IdKind::parse(case[1].as_str().unwrap_or("")).expect("kind");
        assert_eq!(json!(svc::parse_id_list(opt_str(&case[0]), kind)), case[2], "{case}");
    }
    for case in arr(&g["words"]) {
        assert_eq!(json!(svc::parse_words(opt_str(&case[0]))), case[1], "{case}");
    }
    for case in arr(&g["snowflakes"]) {
        assert_eq!(json!(svc::is_snowflake(case[0].as_str().unwrap_or(""))), case[1], "{case}");
    }
    assert_eq!(svc::parse_id_list(None, svc::IdKind::User), Vec::<String>::new());
}

#[test]
fn emoji_parsing_and_keys_match_the_js() {
    let resolver = |x: &str| {
        (x == "pog" || x == "123456789012345678").then(|| svc::CustomEmoji { id: "123456789012345678".into(), name: "pog".into(), animated: false })
    };
    for case in arr(&golden()["emoji"]) {
        let input = case["in"].as_str().unwrap_or("");
        assert_eq!(json!(svc::is_stored_emoji(input)), case["stored"], "stored {input:?}");
        assert_eq!(json!(svc::emoji_key_of(input)), case["key"], "key {input:?}");
        assert_eq!(norm_json(svc::parse_emoji_input(input, resolver).map(Value::String)), case["parsed"], "parsed {input:?}");
    }
}

#[test]
fn attachment_kinds_match_the_js() {
    for case in arr(&golden()["attachments"]) {
        let a = &case[0];
        assert_eq!(json!(svc::attachment_kind(a["name"].as_str(), a["contentType"].as_str())), case[1], "{case}");
    }
}

#[test]
fn normalisers_match_the_js_key_order_included() {
    let g = golden();
    for case in arr(&g["conditions"]) {
        let raw: Value = serde_json::from_str(case["in"].as_str().unwrap_or("null")).expect("json");
        assert_eq!(s(&norm_json(svc::normalize_condition(&raw, false))), s(&case["plain"]), "condition {}", case["in"]);
        assert_eq!(s(&norm_json(svc::normalize_condition(&raw, true))), s(&case["hints"]), "condition (hints) {}", case["in"]);
    }
    for case in arr(&g["actions"]) {
        let raw: Value = serde_json::from_str(case["in"].as_str().unwrap_or("null")).expect("json");
        assert_eq!(s(&norm_json(svc::normalize_action(&raw))), s(&case["out"]), "action {}", case["in"]);
    }
    for case in arr(&g["buttons"]) {
        let raw: Value = serde_json::from_str(case["in"].as_str().unwrap_or("null")).expect("json");
        assert_eq!(s(&norm_json(svc::normalize_button(Some(&raw)))), s(&case["out"]), "button {}", case["in"]);
    }
    assert_eq!(svc::normalize_button(None), Ok(Value::Null));
    for case in arr(&g["lists"]) {
        let raw: Value = serde_json::from_str(case["in"].as_str().unwrap_or("null")).expect("json");
        let kind = if case["kind"] == "condition" { ListKind::Condition } else { ListKind::Action };
        let out = svc::normalize_list(&raw, kind, false);
        assert_eq!(json!({ "items": out.items, "invalid": out.invalid }), case["out"], "list {}", case["in"]);
    }
}

#[test]
fn shape_validation_matches_the_js() {
    for case in arr(&golden()["shapes"]) {
        let conds: Vec<Value> = serde_json::from_str(case["conditions"].as_str().unwrap_or("[]")).expect("json");
        let acts: Vec<Value> = serde_json::from_str(case["actions"].as_str().unwrap_or("[]")).expect("json");
        let cooldown: Value = serde_json::from_str(case["cooldownSeconds"].as_str().unwrap_or("0")).expect("json");
        let out = svc::validate_automation_shape(case["trigger"].as_str().unwrap_or(""), &conds, &acts, &cooldown);
        assert_eq!(json!(out), case["out"], "{case}");
    }
}

fn attempt_json<T>(r: Result<T, svc::AutomationError>, ok: impl Fn(T) -> Value) -> Value {
    match r {
        Ok(v) => json!({ "ok": ok(v) }),
        Err(e) => json!({ "err": e.message, "code": e.code.as_str() }),
    }
}

#[test]
fn button_numbers_and_list_edits_match_the_js() {
    let g = golden();
    for case in arr(&g["buttonSeqs"]) {
        let acts = arr(&case["actions"]);
        assert_eq!(json!(svc::next_button_index(&acts)), case["nextIndex"], "{case}");
        assert_eq!(json!(svc::next_button_seq(Some(&case["buttonSeq"]), &acts)), case["nextSeq"], "{case}");
    }
    for case in arr(&g["appends"]) {
        let row = &case["row"];
        let out = attempt_json(svc::append_action(&arr(&row["actions"]), row.get("buttonSeq"), &case["action"]), |r| {
            json!({ "actions": r.actions, "index": r.index, "buttonIndex": r.button_index, "buttonSeq": r.button_seq })
        });
        assert_eq!(s(&out), s(&case["out"]), "{case}");
    }
    for case in arr(&g["adds"]) {
        let out = attempt_json(svc::add_condition(&arr(&case["row"]["conditions"]), case["type"].as_str().unwrap_or(""), &case["params"]), |r| {
            json!({ "conditions": r.conditions, "merged": r.merged, "index": r.index, "addedNew": r.added_new })
        });
        assert_eq!(s(&out), s(&case["out"]), "{case}");
    }
    for case in arr(&g["removes"]) {
        let index = svc::js_int(&case["index"]).unwrap_or(0);
        let out = attempt_json(svc::remove_at(&arr(&case["list"]), index), |(list, removed)| json!({ "list": list, "removed": removed }));
        assert_eq!(out, case["out"], "{case}");
    }
}

#[test]
fn the_evaluator_matches_the_js() {
    let g = golden();
    for case in arr(&g["evaluate"]) {
        let facts: svc::Facts = serde_json::from_value(case["facts"].clone()).expect("facts");
        assert_eq!(json!(svc::evaluate_condition(&case["cond"], &facts)), case["out"], "{case}");
    }
    for case in arr(&g["evalAll"]) {
        let facts: svc::Facts = serde_json::from_value(case["facts"].clone()).expect("facts");
        assert_eq!(json!(svc::evaluate_conditions(&arr(&case["conds"]), &facts)), case["out"], "{case}");
    }
    let split = &g["split"];
    let all: Vec<Value> = arr(&split["needsMessage"]).into_iter().chain(arr(&split["cheap"])).collect();
    let (cheap, needs) = svc::split_conditions(&[all[0].clone(), all[1].clone(), all[2].clone(), all[3].clone(), all[4].clone(), all[5].clone()]);
    assert_eq!(json!(cheap), split["cheap"]);
    assert_eq!(json!(needs), split["needsMessage"]);
    for case in arr(&g["diffs"]) {
        let old: Vec<String> = serde_json::from_value(case["old"].clone()).expect("ids");
        let new: Vec<String> = serde_json::from_value(case["new"].clone()).expect("ids");
        let (gained, lost) = svc::role_diff(&old, &new, case["guild"].as_str().unwrap_or(""));
        assert_eq!(json!({ "gained": gained, "lost": lost }), case["out"], "{case}");
    }
    for case in arr(&g["voice"]) {
        assert_eq!(json!(svc::voice_triggers(case[0].as_str().unwrap_or(""))), case[1], "{case}");
    }
}

#[test]
fn presets_match_the_js() {
    let g = golden();
    for case in arr(&g["presets"]) {
        let opts: svc::PresetOptions = serde_json::from_value(case["opts"].clone()).expect("opts");
        let out = attempt_json(svc::build_preset_automation(case["preset"].as_str().unwrap_or(""), &opts), |b| {
            json!({ "trigger": b.trigger, "conditions": b.conditions, "actions": b.actions })
        });
        assert_eq!(s(&out), s(&case["out"]), "{case}");
    }
    for case in arr(&g["patterns"]) {
        assert_eq!(json!(svc::commands_only_pattern(case[0].as_str())), case[1], "{case}");
    }
}

#[test]
fn export_and_import_match_the_js_byte_for_byte() {
    let g = golden();
    for case in arr(&g["exports"]) {
        let names: HashMap<String, String> = serde_json::from_value(case["names"].clone()).expect("names");
        assert_eq!(svc::to_export_json(&case["row"], &names), case["json"].as_str().unwrap_or(""), "{}", case["row"]);
        assert_eq!(svc::to_export_json(&case["row"], &HashMap::new()), case["jsonNoNames"].as_str().unwrap_or(""));
    }
    for case in arr(&g["imports"]) {
        let text = case["in"].as_str().unwrap_or("");
        let out = svc::parse_automation_json(text);
        assert_eq!(s(&out.to_json()), s(&case["out"]), "import {}", bot_util::text::clamp_text(text, 200));
    }
}

fn settings_with_currency(emoji: &str, name: &str) -> bot_db::GuildSettings {
    bot_db::GuildSettings { guild_id: "1".into(), currency_emoji: emoji.into(), currency_name: name.into(), ..Default::default() }
}

#[test]
fn descriptions_and_error_texts_match_the_js() {
    let g = golden();
    for case in arr(&g["describeConditions"]) {
        assert_eq!(svc::describe_condition(&case[0], None), case[1].as_str().unwrap_or(""), "{case}");
    }
    let gems = settings_with_currency("💎", "gems");
    let plain = settings_with_currency("", "coins");
    for case in arr(&g["describeActions"]) {
        let want = arr(&case[1]);
        assert_eq!(svc::describe_action(&case[0], None, None), want[0].as_str().unwrap_or(""), "{case}");
        assert_eq!(svc::describe_action(&case[0], None, Some(&gems)), want[1].as_str().unwrap_or(""), "{case}");
        assert_eq!(svc::describe_action(&case[0], None, Some(&plain)), want[2].as_str().unwrap_or(""), "{case}");
    }
    for case in arr(&g["discordErrors"]) {
        let err = match &case[0] {
            Value::String(k) if k == "timeout" => svc::ActionError::Timeout,
            Value::String(k) if k == "long" => svc::ActionError::Api { code: Some(50035), message: Some("x".repeat(300)) },
            Value::String(_) => svc::ActionError::Api { code: None, message: None },
            Value::Null => svc::ActionError::Api { code: None, message: Some("boom null".into()) },
            other => svc::ActionError::Api { code: other.as_u64(), message: Some(format!("boom {other}")) },
        };
        assert_eq!(json!(svc::discord_error_text(&err)), case[1], "{case}");
    }
    let t = &g["errorTexts"];
    assert_eq!(t["name"], svc::error_texts::NAME);
    assert_eq!(t["notFound"], svc::error_texts::not_found("x"));
    assert_eq!(t["taken"], svc::error_texts::taken("x"));
    assert_eq!(t["limit"], svc::error_texts::limit());
    assert_eq!(t["maxConditions"], svc::error_texts::max_conditions());
    assert_eq!(t["maxActions"], svc::error_texts::max_actions());
    assert_eq!(t["buttonsUsed"], svc::error_texts::BUTTONS_USED);
    assert_eq!(t["needsActions"], svc::error_texts::NEEDS_ACTIONS);
}

#[test]
fn tables_constants_keys_and_raw_sql_match_the_js() {
    let g = golden();
    let tables = &g["tables"];
    let triggers: Vec<String> = tables["triggers"].as_object().expect("triggers").keys().cloned().collect();
    assert_eq!(triggers, svc::TRIGGERS.iter().map(|t| t.key.to_string()).collect::<Vec<_>>());
    for t in svc::TRIGGERS {
        assert_eq!(tables["triggers"][t.key], json!({ "label": t.label, "channel": t.channel }));
    }
    let ckeys: Vec<String> = tables["conditionTypes"].as_object().expect("conditions").keys().cloned().collect();
    assert_eq!(ckeys, svc::CONDITION_TYPES.iter().map(|d| d.key.to_string()).collect::<Vec<_>>());
    for d in svc::CONDITION_TYPES {
        let js = &tables["conditionTypes"][d.key];
        assert_eq!(js["label"], d.label);
        assert_eq!(js["triggers"], json!(d.triggers), "{}", d.key);
        assert_eq!(js["list"], json!(d.list));
        assert_eq!(js["max"], json!(d.max));
        assert_eq!(js["noParams"] == true, d.no_params, "{}", d.key);
    }
    let akeys: Vec<String> = tables["actionTypes"].as_object().expect("actions").keys().cloned().collect();
    assert_eq!(akeys, svc::ACTION_TYPES.iter().map(|d| d.key.to_string()).collect::<Vec<_>>());
    for d in svc::ACTION_TYPES {
        let js = &tables["actionTypes"][d.key];
        assert_eq!(js["label"], d.label);
        assert_eq!(js["triggers"], json!(d.triggers), "{}", d.key);
        assert_eq!(js["button"] == true, d.button, "{}", d.key);
        assert_eq!(js["thread"] == true, d.thread, "{}", d.key);
        assert_eq!(js["reward"].as_str(), d.reward, "{}", d.key);
    }
    for (name, list) in [("T_CHANNEL", svc::T_CHANNEL), ("T_CONTENT", svc::T_CONTENT), ("T_LIVE", svc::T_LIVE), ("T_COUNTS", svc::T_COUNTS), ("T_THREADY", svc::T_THREADY)] {
        assert_eq!(tables[name], json!(list), "{name}");
    }
    for p in svc::PRESETS {
        assert_eq!(tables["presetOptions"][p], json!(svc::preset_options(p)), "{p}");
    }
    for (t, needs) in tables["executorChannelNeeds"].as_object().expect("needs") {
        assert_eq!(&json!(svc::executor_channel_needs(t)), needs, "{t}");
    }
    assert_eq!(svc::executor_channel_needs("PIN"), None);

    let c = &g["constants"];
    assert_eq!(c["AUTOMATION_NAME_MAX"], json!(svc::AUTOMATION_NAME_MAX));
    assert_eq!(c["IMPORT_MAX_BYTES"], json!(svc::IMPORT_MAX_BYTES));
    assert_eq!(c["MAX_CHAIN_DEPTH"], json!(svc::MAX_CHAIN_DEPTH));
    assert_eq!(c["CAUSE_TTL_MS"], json!(svc::CAUSE_TTL_MS));
    assert_eq!(c["RUN_BUDGET_MAX"], json!(svc::RUN_BUDGET_MAX));
    assert_eq!(c["RUN_BUDGET_WINDOW_MS"], json!(svc::RUN_BUDGET_WINDOW_MS));
    assert_eq!(c["MAX_PENDING_RUNS"], json!(svc::MAX_PENDING_RUNS));
    assert_eq!(c["RUN_TIMEOUT_MS"], json!(svc::RUN_TIMEOUT_MS));
    assert_eq!(c["MEMO_TTL_MS"], json!(svc::MEMO_TTL_MS));
    assert_eq!(c["CACHE_TTL_SECONDS"], json!(svc::CACHE_TTL_SECONDS));
    assert_eq!(c["REPOST_DEDUPE_TTL_SECONDS"], json!(svc::REPOST_DEDUPE_TTL_SECONDS));
    assert_eq!(c["PROBLEM_TTL_SECONDS"], json!(svc::PROBLEM_TTL_SECONDS));
    assert_eq!(c["MAX_COOLDOWN_SECONDS"], json!(svc::MAX_COOLDOWN_SECONDS));
    assert_eq!(c["MAX_REWARD_AMOUNT"], json!(svc::MAX_REWARD_AMOUNT));
    assert_eq!(c["THREAD_ARCHIVE_MINUTES"], json!(svc::THREAD_ARCHIVE_MINUTES));
    assert_eq!(c["DEFAULT_THREAD_NAME"], json!(svc::DEFAULT_THREAD_NAME));
    assert_eq!(c["WELCOMER_DEFAULT_TEXT"], json!(svc::WELCOMER_DEFAULT_TEXT));
    assert_eq!(c["PRESETS"], json!(svc::PRESETS));
    assert_eq!(c["MAX_BUTTON_INDEX"], json!(svc::MAX_BUTTON_INDEX));
    assert_eq!(c["EXPORT_FORMAT"], json!(svc::EXPORT_FORMAT));
    assert_eq!(c["EXPORT_VERSION"], json!(svc::EXPORT_VERSION));

    assert_eq!(g["keys"]["automations"], json!(svc::automations_cache_key("955028000000000001")));
    let ids = arr(&g["customIds"]);
    assert_eq!(ids[0], json!(bot_config::build_custom_id("automation", &["clxabcdefgh", "4"]).expect("id")));
    assert_eq!(ids[1], json!(bot_config::build_custom_id("automation", &["autoone01", "99"]).expect("id")));

    let raw = arr(&g["rawSql"]);
    assert_eq!(raw.len(), svc::RAW_SQL.len());
    for (js, rs) in raw.iter().zip(svc::RAW_SQL.iter()) {
        assert_eq!(js["name"], rs.name);
        let mut n = 0;
        let expected: String = js["sql"].as_str().unwrap_or("").chars().fold(String::new(), |mut acc, ch| {
            if ch == '?' {
                n += 1;
                acc.push_str(&format!("${n}"));
            } else {
                acc.push(ch);
            }
            acc
        });
        assert_eq!(expected, rs.sql, "{}", rs.name);
        assert_eq!(js["values"], json!(rs.params));
        assert!(!rs.sql.to_lowercase().contains("now()"), "no bare now()");
    }
}

// ── Trackers (createRunBudget, createCauseTracker, createLocalExpiringSet) ──

#[test]
fn budgets_trackers_and_expiring_sets() {
    let mut b = svc::RunBudget::new(3, 1000);
    assert_eq!([b.take("g", 0), b.take("g", 0), b.take("g", 0), b.take("g", 0)], [true, true, true, false]);
    assert!(b.take("g", 1001));
    assert!(b.take("other", 0));

    let mut t = svc::CauseTracker::new(100, 10_000);
    t.mark("g", "u", 1, 0);
    t.mark("g", "u", 2, 10);
    assert_eq!(t.peek("g", "u", 50), 2);
    assert_eq!(t.peek("g", "u", 200), 0);
    t.mark("g", "u", 2, 300);
    t.mark("g", "u", 1, 360);
    assert_eq!(t.peek("g", "u", 400), 2);
    assert_eq!(t.peek("g", "nobody", 0), 0);

    let mut set = svc::LocalExpiringSet::default();
    assert!(set.add("k", 100, 0));
    assert!(!set.add("k", 100, 50));
    assert!(set.has("k", 50));
    assert!(set.add("k", 100, 150));
    let mut small = svc::LocalExpiringSet::new(2);
    small.add("a", 1000, 0);
    small.add("b", 1000, 1);
    small.add("c", 1000, 2);
    assert!(!small.has("a", 3), "the oldest was evicted");
    assert!(small.has("c", 3));
    assert!(small.delete("c"));
}

#[test]
fn appended_buttons_are_never_renumbered() {
    let new_btn = |label: &str| json!({ "type": "SEND_MESSAGE", "content": "x", "button": { "label": label, "url": null, "style": "PRIMARY", "index": null } });
    let a1 = svc::append_action(&[], Some(&json!(0)), &new_btn("B")).expect("a1");
    assert_eq!(a1.button_index, Some(1));
    let a2 = svc::append_action(&a1.actions, Some(&json!(a1.button_seq)), &new_btn("C")).expect("a2");
    assert_eq!(a2.button_index, Some(2));
    let (removed, _) = svc::remove_at(&a2.actions, 2).expect("remove");
    let a3 = svc::append_action(&removed, Some(&json!(a2.button_seq)), &new_btn("D")).expect("a3");
    assert_eq!(a3.button_index, Some(3), "a removed button's number is not reused");
    let full = svc::append_action(&[], Some(&json!(99)), &new_btn("B")).expect_err("full");
    assert_eq!(full.message, "This automation has used all 99 button numbers. Create a new automation for more buttons.");
    assert_eq!(full.code, svc::ErrorCode::Buttons);
}

// ── Plugin and command declarations ──────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["automation"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "automations");
    assert!(bot_core::testing::persistent_handler_problems_for(&reg, &["automations"]).is_empty());
}

#[test]
fn automation_payload_shape() {
    let payload = feat_automations::commands::automation::command().payload();
    let top = arr(&payload["options"]);
    assert_eq!(top.len(), 12);
    assert_eq!(top.iter().filter(|o| o["type"] == 2).count(), 2);
    let find = |path: &[&str]| -> Value {
        let mut node = json!({ "options": top.clone() });
        for p in path {
            node = arr(&node["options"]).into_iter().find(|o| o["name"] == *p).unwrap_or(Value::Null);
        }
        node
    };
    assert_eq!(arr(&find(&["create"])["options"]).len(), 10);
    assert_eq!(arr(&find(&["action", "add"])["options"]).len(), 13);
    assert_eq!(arr(&find(&["condition", "add"])["options"]).len(), 8);
    assert_eq!(arr(&find(&["create-blank", "trigger"])["choices"]).len(), 11);
    assert_eq!(find(&["condition", "remove", "index"])["max_value"], json!(10));
    assert_eq!(find(&["action", "remove", "index"])["max_value"], json!(10));
    let schema = std::fs::read_to_string(bot_core::testing::repo_root().join("prisma").join("schema.prisma")).expect("schema");
    let body = schema.split("enum AutomationTrigger {").nth(1).and_then(|b| b.split('}').next()).unwrap_or("");
    let mut values: Vec<String> = body.lines().map(str::trim).filter(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_uppercase() || c == '_')).map(str::to_string).collect();
    values.sort();
    let mut choices: Vec<String> = arr(&find(&["create-blank", "trigger"])["choices"]).iter().map(|c| c["value"].as_str().unwrap_or("").to_string()).collect();
    choices.sort();
    assert_eq!(choices, values);
}

#[test]
fn every_golden_section_is_populated() {
    let g = golden();
    for key in [
        "names", "types", "idLists", "words", "snowflakes", "emoji", "attachments", "conditions", "actions", "buttons", "lists", "shapes", "buttonSeqs",
        "appends", "adds", "removes", "evaluate", "evalAll", "diffs", "voice", "presets", "patterns", "exports", "imports", "describeConditions",
        "describeActions", "discordErrors",
    ] {
        assert!(!arr(&g[key]).is_empty(), "{key}");
    }
}

// ── Runtime: a fake guild and a fake Discord REST server ─────────────────────

const OWNER: &str = "955028000000000900";
const BOT_ROLE: &str = "955028000000000901";

fn bot_perms() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::EMBED_LINKS
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ADD_REACTIONS
        | Permissions::MANAGE_MESSAGES
        | Permissions::MANAGE_ROLES
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::from_bits_retain(1 << 51)
}

fn everyone() -> Permissions {
    Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY
}

struct World {
    t: TestApp,
    rest: FakeRest,
    g: String,
    gid: GuildId,
    fake: FakeGuild,
}

/// A guild with the bot (role at position 50), `#general` (40), `#other` (41) and a member (42).
fn world(g: &str, edit: impl FnOnce(FakeGuild) -> FakeGuild) -> World {
    let guild = FakeGuild::new(g, OWNER)
        .everyone(everyone())
        .role(BOT_ROLE, 50, bot_perms())
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .channel(&sid(40), 0, "general", &[])
        .channel(&sid(41), 0, "other", &[])
        .member(&sid(42), &[]);
    let guild = edit(guild);
    let rest = FakeRest::new();
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(std::slice::from_ref(&guild), &rest);
    World { t, rest, g: g.to_string(), gid: GuildId::new(g.parse().expect("guild id")), fake: guild }
}

impl World {
    fn guild_json(&self) -> Value {
        self.fake.json()
    }

    fn member(&self, user: &str) -> serenity::all::Member {
        let uid = serenity::all::UserId::new(user.parse().expect("id"));
        bot_core::interaction_helpers::with_cached_guild(&self.t, self.gid, |g| g.members.get(&uid).cloned()).flatten().expect("member cached")
    }

    fn channel(&self, id: &str) -> serenity::all::GuildChannel {
        bot_core::interaction_helpers::cached_channel(&self.t, self.gid, serenity::all::ChannelId::new(id.parse().expect("id"))).expect("channel cached")
    }

    fn guild(&self) -> serenity::all::Guild {
        bot_core::interaction_helpers::cached_guild(&self.t, self.gid).expect("guild cached")
    }

    fn base(&self, trigger: &'static str, user: &str, channel: Option<&str>, message: Option<serenity::all::Message>) -> Base {
        let mut b = Base::new(trigger, self.gid);
        let m = self.member(user);
        b.user = Some(m.user.clone());
        b.member = Some(m);
        b.channel = channel.map(|c| self.channel(c));
        b.message = message;
        b.get_settings = Some(Arc::new(|| Box::pin(async { None })));
        b
    }

    fn memo(&self, entries: Vec<AutomationLite>) {
        let mut by: ByTrigger = ByTrigger::new();
        for a in entries {
            by.entry(a.trigger.clone()).or_default().push(a);
        }
        svc::set_automation_memo(&self.t, &self.g, by);
    }

    fn posts(&self, channel: &str) -> Vec<Value> {
        self.rest.calls_to("POST", &format!("/channels/{channel}/messages")).into_iter().map(|c| c.body).collect()
    }

    async fn dispatch(&self, trigger: &'static str, base: Base) {
        svc::dispatch(&self.t, trigger, base, true).await;
    }
}

fn lite(id: &str, trigger: &str, conditions: Value, actions: Value) -> AutomationLite {
    AutomationLite {
        id: id.into(),
        name: id.into(),
        trigger: trigger.into(),
        cooldown_seconds: 0,
        conditions: svc::normalize_items(&conditions, ListKind::Condition),
        actions: svc::normalize_items(&actions, ListKind::Action),
    }
}

fn message_json(id: &str, g: &str, ch: &str, author: &str, content: &str) -> Value {
    json!({
        "id": id, "channel_id": ch, "guild_id": g,
        "author": { "id": author, "username": format!("user{author}"), "discriminator": "0", "avatar": null },
        "content": content, "timestamp": "2023-11-14T22:13:20.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
        "type": 0, "components": [], "reactions": []
    })
}

fn message(id: u64, g: &str, ch: &str, author: &str, content: &str, edit: impl FnOnce(&mut Value)) -> serenity::all::Message {
    let mut v = message_json(&sid(id), g, ch, author, content);
    edit(&mut v);
    serde_json::from_value(v).expect("message deserializes")
}

fn reaction_json(name: &str, count: u64, me: bool) -> Value {
    json!({ "count": count, "count_details": { "burst": 0, "normal": count }, "me": me, "me_burst": false, "burst_colors": [], "emoji": { "id": null, "name": name } })
}

#[tokio::test]
async fn dispatch_conditions_placeholders_mentions_and_buttons() {
    let w = world("955028100000000001", |g| g);
    let msg = message(701, &w.g, &sid(40), &sid(42), "Hello world", |_| {});
    w.memo(vec![
        lite(
            "autoone01",
            "MESSAGE_SENT",
            json!([{ "type": "CONTAINS", "words": ["hello"] }, { "type": "CHANNEL_IN", "channelIds": [sid(40)] }]),
            json!([{ "type": "SEND_MESSAGE", "content": format!("Hi {{user}}! @everyone <@&{}> {{message.content}}", sid(60)), "channelId": sid(41),
                     "button": { "label": "Click {user.name}", "url": null, "style": "SUCCESS", "index": 4 } }]),
        ),
        lite("autotwo02", "MESSAGE_SENT", json!([{ "type": "CONTAINS", "words": ["nope"] }]), json!([{ "type": "PIN" }])),
    ]);
    w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg))).await;
    let posts = w.posts(&sid(41));
    assert_eq!(posts.len(), 1);
    let content = posts[0]["content"].as_str().unwrap_or("");
    assert!(content.starts_with(&format!("Hi <@{}>!", sid(42))), "{content}");
    assert!(content.ends_with("Hello world"), "{content}");
    assert_eq!(posts[0]["allowed_mentions"]["parse"], json!(["users"]));
    assert_eq!(posts[0]["allowed_mentions"]["roles"], json!([]), "the role is not pingable");
    let button = &posts[0]["components"][0]["components"][0];
    assert_eq!(button["custom_id"], "automation:autoone01:4");
    assert_eq!(button["label"], format!("Click user{}", sid(42)));
    assert_eq!(button["style"], 3);
    assert!(w.rest.calls_to("PUT", "/channels/*/pins/*").is_empty(), "the second automation did not run");
}

#[tokio::test]
async fn dispatch_pings_only_pingable_roles_typed_by_the_admin() {
    let pingable = sid(61);
    let quiet = sid(62);
    let w = world("955028100000000002", |g| {
        g.role_with(&pingable, 1, Permissions::empty(), |r| r["mentionable"] = json!(true)).role(&quiet, 2, Permissions::empty()).member(&sid(63), &[&quiet])
    });
    w.memo(vec![lite(
        "autorole01",
        "ROLE_GAINED",
        json!([]),
        json!([{ "type": "SEND_MESSAGE", "content": format!("<@&{pingable}> <@&{quiet}> gained {{role}}"), "channelId": sid(40) }]),
    )]);
    let mut b = w.base("ROLE_GAINED", &sid(63), None, None);
    b.role_ids = Some(vec![quiet.clone()]);
    w.dispatch("ROLE_GAINED", b).await;
    let posts = w.posts(&sid(40));
    assert_eq!(posts[0]["allowed_mentions"]["roles"], json!([pingable]));
    assert!(posts[0]["content"].as_str().unwrap_or("").ends_with(&format!("gained <@&{quiet}>")));
}

#[tokio::test]
async fn the_per_user_cooldown_falls_back_to_the_process_when_redis_is_down() {
    let w = world("955028100000000003", |g| g.member(&sid(43), &[]));
    w.t.cache_backend.set_available(false);
    let mut a = lite("autocool01", "MESSAGE_SENT", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "ran", "channelId": sid(40) }]));
    a.cooldown_seconds = 30;
    w.memo(vec![a]);
    for (n, user) in [(1u64, 42u64), (2, 42), (3, 43)] {
        let msg = message(710 + n, &w.g, &sid(40), &sid(user), "hi", |_| {});
        w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(user), Some(&sid(40)), Some(msg))).await;
    }
    assert_eq!(w.posts(&sid(40)).len(), 2, "the same member is cooled down, another member is not");
}

#[tokio::test]
async fn at_most_thirty_runs_per_ten_seconds_and_a_refused_run_gives_its_cooldown_back() {
    let w = world("955028100000000004", |g| g);
    w.memo(vec![lite("autobud01", "MESSAGE_SENT", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(40) }]))]);
    for n in 0..35 {
        let msg = message(720 + n, &w.g, &sid(40), &sid(42), "hi", |_| {});
        w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg))).await;
    }
    assert_eq!(w.posts(&sid(40)).len(), svc::RUN_BUDGET_MAX);
    // The budget is spent: a run with an hour-long cooldown is refused and the cooldown is not kept.
    let mut cooled = lite("autocool02", "MESSAGE_SENT", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(40) }]));
    cooled.cooldown_seconds = 3600;
    w.memo(vec![cooled]);
    let msg = message(760, &w.g, &sid(40), &sid(42), "hi", |_| {});
    w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg))).await;
    assert_eq!(w.posts(&sid(40)).len(), svc::RUN_BUDGET_MAX);
    assert_eq!(w.t.cache_backend.raw(&format!("automation:cd:{}:autocool02:{}", w.g, sid(42))), None);
}

#[tokio::test]
async fn the_queue_holds_twenty_five_runs_and_dropped_runs_give_their_cooldown_back() {
    let members: Vec<u64> = (0..(svc::MAX_PENDING_RUNS as u64 + 3)).map(|i| 200 + i).collect();
    let ids: Vec<String> = members.iter().map(|m| sid(*m)).collect();
    let w = world("955028100000000005", |mut g| {
        for id in &ids {
            g = g.member(id, &[]);
        }
        g
    });
    let mut a = lite("autoqueue01", "MESSAGE_SENT", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(40) }]));
    a.cooldown_seconds = 600;
    w.memo(vec![a]);
    let gate = svc::hold_run_queue(&w.t, &w.g);
    for (n, id) in ids.iter().enumerate() {
        let msg = message(800 + n as u64, &w.g, &sid(40), id, "hi", |_| {});
        svc::dispatch(&w.t, "MESSAGE_SENT", w.base("MESSAGE_SENT", id, Some(&sid(40)), Some(msg)), false).await;
    }
    assert_eq!(svc::pending_runs(&w.t, &w.g), svc::MAX_PENDING_RUNS);
    for (k, id) in ids.iter().enumerate() {
        let key = format!("automation:cd:{}:autoqueue01:{id}", w.g);
        assert_eq!(w.t.cache_backend.raw(&key).is_some(), k < svc::MAX_PENDING_RUNS, "only the dropped members got their cooldown back ({k})");
    }
    let _ = gate.send(());
    svc::settle_runs(&w.t, &w.g).await;
    assert_eq!(w.posts(&sid(40)).len(), svc::MAX_PENDING_RUNS);
    assert_eq!(svc::pending_runs(&w.t, &w.g), 0);
}

fn member_with_roles(w: &World, user: &str, roles: &[String]) -> serenity::all::Member {
    let mut m = w.member(user);
    m.roles = roles.iter().map(|r| serenity::all::RoleId::new(r.parse().expect("role"))).collect();
    m
}

fn role_puts(w: &World) -> Vec<String> {
    w.rest.calls_to("PUT", "/guilds/*/members/*/roles/*").iter().map(|c| c.path.rsplit('/').next().unwrap_or("").to_string()).collect()
}

#[tokio::test]
async fn role_actions_one_at_a_time_checked_first_and_the_chain_stops_at_depth_three() {
    let w = world("955028100000000006", |mut g| {
        for (k, n) in [71u64, 72, 73, 74, 75].iter().enumerate() {
            g = g.role(&sid(*n), k as u16 + 1, Permissions::empty());
        }
        g.role(&sid(76), 6, Permissions::ADMINISTRATOR).role(&sid(77), 99, Permissions::empty())
    });
    w.memo(vec![
        lite("chain-a01", "ROLE_GAINED", json!([{ "type": "GAINED_ROLE", "roleIds": [sid(71)] }]), json!([{ "type": "GIVE_ROLES", "roleIds": [sid(72), sid(76), sid(77)] }])),
        lite("chain-b01", "ROLE_GAINED", json!([{ "type": "GAINED_ROLE", "roleIds": [sid(72)] }]), json!([{ "type": "GIVE_ROLES", "roleIds": [sid(73)] }])),
        lite("chain-c01", "ROLE_GAINED", json!([{ "type": "GAINED_ROLE", "roleIds": [sid(73)] }]), json!([{ "type": "GIVE_ROLES", "roleIds": [sid(74)] }])),
        lite("chain-d01", "ROLE_GAINED", json!([{ "type": "GAINED_ROLE", "roleIds": [sid(74)] }]), json!([{ "type": "GIVE_ROLES", "roleIds": [sid(75)] }])),
    ]);
    // Each step stands in for the guildMemberUpdate Discord sends after the previous role change.
    let mut held: Vec<String> = Vec::new();
    for gained in [71u64, 72, 73, 74] {
        let old = member_with_roles(&w, &sid(42), &held);
        held.push(sid(gained));
        let new = member_with_roles(&w, &sid(42), &held);
        let ctx = Arc::new(HookCtx::new(w.t.app.clone(), GuildMemberUpdate { guild_id: w.gid, old_member: Some(old), member: new, old_known: true }));
        feat_automations::plugin::on_member_update(ctx).await.expect("hook");
        svc::settle_runs(&w.t, &w.g).await;
    }
    assert_eq!(role_puts(&w), vec![sid(72), sid(73), sid(74)], "the dangerous and the too-high role were refused; D is past the depth");
    let calls = w.rest.calls_to("PUT", "/guilds/*/members/*/roles/*");
    assert_eq!(calls[0].reason.as_deref(), Some("Automation: chain-a01"));
    let problem = w.t.cache_backend.raw("automation:problem:chain-a01").unwrap_or_default();
    assert!(problem.contains("Action 1 (Give roles): "), "{problem}");
}

#[tokio::test]
async fn take_roles_and_a_pending_captcha() {
    let w = world("955028100000000007", |g| g.role(&sid(81), 1, Permissions::empty()).role(&sid(82), 2, Permissions::empty()).role(&sid(83), 3, Permissions::empty()));
    w.memo(vec![
        lite("join-a001", "MEMBER_JOINED", json!([]), json!([{ "type": "GIVE_ROLES", "roleIds": [sid(83)] }, { "type": "SEND_MESSAGE", "content": "welcome", "channelId": sid(40) }])),
        lite("lost-a001", "ROLE_LOST", json!([]), json!([{ "type": "TAKE_ROLES", "roleIds": [sid(81), sid(82), sid(83)] }])),
    ]);
    let joined = |pending: bool| {
        let mut b = w.base("MEMBER_JOINED", &sid(42), None, None);
        b.member = Some(member_with_roles(&w, &sid(42), &[sid(81), sid(82)]));
        b.captcha_pending = pending;
        b
    };
    w.dispatch("MEMBER_JOINED", joined(true)).await;
    assert!(role_puts(&w).is_empty(), "GIVE_ROLES is skipped while verification is pending");
    assert_eq!(w.posts(&sid(40)).len(), 1, "the rest of the automation still runs");
    w.dispatch("MEMBER_JOINED", joined(false)).await;
    assert_eq!(role_puts(&w), vec![sid(83)]);
    let mut lost = w.base("ROLE_LOST", &sid(42), None, None);
    lost.member = Some(member_with_roles(&w, &sid(42), &[sid(81), sid(82)]));
    lost.role_ids = Some(vec![sid(1)]);
    w.dispatch("ROLE_LOST", lost).await;
    let deletes: Vec<String> = w.rest.calls_to("DELETE", "/guilds/*/members/*/roles/*").iter().map(|c| c.path.rsplit('/').next().unwrap_or("").to_string()).collect();
    assert_eq!(deletes, vec![sid(81), sid(82)], "a role the member does not hold is not taken");
}

fn reaction_event(w: &World, channel: &str, message_id: &str, emoji: &str) -> bot_core::hooks::events::ReactionEvent {
    let reaction: serenity::all::Reaction = serde_json::from_value(json!({
        "channel_id": channel, "message_id": message_id, "guild_id": w.g, "user_id": sid(42),
        "emoji": { "id": null, "name": emoji }, "burst": false, "burst_colors": [], "type": 0
    }))
    .expect("reaction deserializes");
    bot_core::hooks::events::ReactionEvent::new(w.gid, reaction, w.member(&sid(42)).user)
}

#[tokio::test]
async fn reaction_added_fetches_the_message_lazily_and_skips_system_messages() {
    let w = world("955028100000000008", |g| g);
    w.memo(vec![lite(
        "react-a01",
        "REACTION_ADDED",
        json!([{ "type": "REACTED_WITH", "emojis": ["⭐"] }, { "type": "HAS_REACTIONS", "min": 3, "emoji": "⭐" }]),
        json!([{ "type": "REACT", "emoji": "👍" }]),
    )]);
    let starred = |id: u64, author: &str, kind: u64| {
        let mut m = message_json(&sid(id), &w.g, &sid(40), author, "star me");
        m["reactions"] = json!([reaction_json("⭐", 3, false)]);
        m["type"] = json!(kind);
        m
    };
    let fire = |id: u64, key: &str| {
        let ev = reaction_event(&w, &sid(40), &sid(id), key);
        let mut b = w.base("REACTION_ADDED", &sid(42), Some(&sid(40)), None);
        b.reaction = Some(svc::ReactionInfo { key: key.into(), display: key.into() });
        b.reaction_event = Some(Arc::new(ev));
        b
    };
    w.rest.on("GET", &format!("/channels/{}/messages/{}", sid(40), sid(901)), 200, starred(901, &sid(42), 0));
    w.rest.on("GET", &format!("/channels/{}/messages/{}", sid(40), sid(902)), 200, starred(902, TEST_BOT_ID, 0));
    w.rest.on("GET", &format!("/channels/{}/messages/{}", sid(40), sid(903)), 200, starred(903, &sid(42), 7));
    w.dispatch("REACTION_ADDED", fire(901, "🌟")).await;
    assert!(w.rest.calls_to("GET", "/channels/*/messages/*").is_empty(), "the cheap condition failed: no fetch");
    w.dispatch("REACTION_ADDED", fire(901, "⭐")).await;
    assert_eq!(w.rest.calls_to("GET", "/channels/*/messages/*").len(), 1);
    let reacts = |id: u64| w.rest.calls_to("PUT", &format!("/channels/{}/messages/{}/reactions/**", sid(40), sid(id))).len();
    assert_eq!(reacts(901), 1);
    w.dispatch("REACTION_ADDED", fire(902, "⭐")).await;
    assert_eq!(reacts(902), 1, "allowed on a bot-authored message");
    w.dispatch("REACTION_ADDED", fire(903, "⭐")).await;
    assert_eq!(reacts(903), 0, "a system message (a join) is skipped");
    w.dispatch("REACTION_ADDED", fire(904, "⭐")).await; // the message cannot be fetched (404): nothing runs
    assert_eq!(reacts(904), 0);
}

fn thread_json(id: &str, g: &str, parent: &str, name: &str, kind: u64) -> Value {
    json!({
        "id": id, "type": kind, "guild_id": g, "name": name, "parent_id": parent, "owner_id": TEST_BOT_ID,
        "message_count": 0, "member_count": 1, "rate_limit_per_user": 0, "nsfw": false, "permission_overwrites": [],
        "thread_metadata": { "archived": false, "auto_archive_duration": 60, "archive_timestamp": "2025-01-01T00:00:00.000000+00:00", "locked": false }
    })
}

fn script_threads(w: &World, pattern: &str, thread_id: u64, kind: u64) {
    let (g, tid) = (w.g.clone(), sid(thread_id));
    w.rest.on_with("POST", pattern, move |call| {
        let parent = call.path.split('/').nth(2).unwrap_or("").to_string();
        let name = call.body["name"].as_str().unwrap_or("").to_string();
        (201, thread_json(&tid, &g, &parent, &name, kind))
    });
}

#[tokio::test]
async fn message_actions_delete_pin_reply_repost_and_threads() {
    let w = world("955028100000000009", |g| g);
    w.t.cache_backend.set_available(false); // the repost dedupe falls back to the process
    script_threads(&w, "/channels/*/messages/*/threads", 555, 11);
    let msg = message(931, &w.g, &sid(40), &sid(42), "look at this", |m| {
        m["attachments"] = json!([
            { "id": "1", "filename": "pic.png", "size": 1, "url": "https://cdn.test/pic.png", "proxy_url": "https://cdn.test/pic.png", "content_type": "image/png" },
            { "id": "2", "filename": "a.zip", "size": 1, "url": "https://cdn.test/a.zip", "proxy_url": "https://cdn.test/a.zip", "content_type": "application/zip" }
        ]);
    });
    w.memo(vec![
        lite("msg-repost1", "MESSAGE_SENT", json!([{ "type": "ATTACHMENT_IMAGE" }]), json!([{ "type": "REPOST", "channelId": sid(41) }])),
        lite(
            "msg-thread1",
            "MESSAGE_SENT",
            json!([{ "type": "IS_NOT_REPLY" }]),
            json!([
                { "type": "CREATE_THREAD", "name": "{user.name} talk", "channelId": null, "autoArchiveMinutes": 60 },
                { "type": "ADD_TO_THREAD", "userIds": [] },
                { "type": "SEND_IN_THREAD", "content": "welcome to {thread.name}" },
                { "type": "PIN" },
                { "type": "REPLY", "content": "noted" },
                { "type": "DELETE" },
                { "type": "REACT", "emoji": "👍" }
            ]),
        ),
    ]);
    for _ in 0..2 {
        w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg.clone()))).await;
    }
    let reposts = w.posts(&sid(41));
    assert_eq!(reposts.len(), 1, "the repost is deduped");
    let embed = &reposts[0]["embeds"][0];
    assert_eq!(embed["description"], "look at this");
    assert_eq!(embed["image"]["url"], "https://cdn.test/pic.png");
    assert_eq!(embed["footer"]["text"], "#general");
    assert_eq!(embed["author"]["name"], format!("user{}", sid(42)));
    assert!(embed["fields"][0]["value"].as_str().unwrap_or("").contains("a.zip"));
    assert_eq!(reposts[0]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(reposts[0]["components"][0]["components"][0]["url"], svc::message_url(w.gid, &msg));

    let threads = w.rest.calls_to("POST", "/channels/*/messages/*/threads");
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].body["name"], format!("user{} talk", sid(42)));
    assert_eq!(threads[0].body["auto_archive_duration"], 60);
    assert_eq!(w.rest.calls_to("PUT", &format!("/channels/{}/thread-members/{}", sid(555), sid(42))).len(), 2);
    assert_eq!(w.posts(&sid(555))[0]["content"], format!("welcome to user{} talk", sid(42)));
    assert_eq!(w.rest.calls_to("PUT", "/channels/*/pins/*").len() + w.rest.calls_to("PUT", "/channels/*/messages/pins/*").len(), 2);
    assert_eq!(w.rest.calls_to("DELETE", &format!("/channels/{}/messages/{}", sid(40), sid(931))).len(), 2);
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&w.t, &sid(931)).as_deref(), Some("automations"));
    assert!(w.rest.calls_to("PUT", "/channels/*/messages/*/reactions/**").is_empty(), "after DELETE: skipped");
    let replies = w.posts(&sid(40));
    assert_eq!(replies[0]["message_reference"]["message_id"], sid(931), "REPLY ran before DELETE");
    assert_eq!(replies[0]["message_reference"]["fail_if_not_exists"], false);
}

fn message_create(w: &World, msg: serenity::all::Message) -> Arc<HookCtx<MessageCreate>> {
    let channel_id = msg.channel_id;
    Arc::new(HookCtx::new(
        w.t.app.clone(),
        MessageCreate { guild_id: w.gid, member: Some(w.member(&msg.author.id.to_string())), channel: Some(w.channel(&channel_id.to_string())), channel_id, message: msg },
    ))
}

fn message_delete(w: &World, msg: serenity::all::Message) -> Arc<HookCtx<MessageDelete>> {
    Arc::new(HookCtx::new(
        w.t.app.clone(),
        MessageDelete { guild_id: w.gid, message_id: msg.id, channel_id: msg.channel_id, partial: false, channel: Some(w.channel(&msg.channel_id.to_string())), message: Some(msg) },
    ))
}

#[tokio::test]
async fn reply_after_delete_has_no_reference_and_bot_deletions_never_fire_message_deleted() {
    let w = world("955028100000000010", |g| g);
    w.memo(vec![
        lite("del-then01", "MESSAGE_SENT", json!([]), json!([{ "type": "DELETE" }, { "type": "REPLY", "content": "gone" }])),
        lite(
            "deleted-a1",
            "MESSAGE_DELETED",
            json!([{ "type": "CONTAINS", "words": ["bye"] }]),
            json!([{ "type": "SEND_MESSAGE", "content": "saw {message.content} by {message.author.name}", "channelId": sid(40) }]),
        ),
    ]);
    let msg = message(1001, &w.g, &sid(40), &sid(42), "x", |_| {});
    assert!(feat_automations::plugin::filter_message_create(&message_create(&w, msg.clone())));
    feat_automations::plugin::on_message_create(message_create(&w, msg)).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    assert_eq!(w.rest.calls_to("DELETE", "/channels/*/messages/*").len(), 1);
    let posts = w.posts(&sid(40));
    assert_eq!(posts.len(), 1);
    assert!(posts[0].get("message_reference").is_none_or(Value::is_null), "the message is gone: no reference");

    let by_bot = message(1002, &w.g, &sid(40), &sid(42), "bye bye", |_| {});
    bot_core::deletion_tracker::mark_bot_deletion(&w.t, &[by_bot.id.to_string()], Some("automod"), None);
    feat_automations::plugin::on_message_delete(message_delete(&w, by_bot)).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    assert_eq!(w.posts(&sid(40)).len(), 1);
    let by_member = message(1003, &w.g, &sid(40), &sid(42), "bye bye", |_| {});
    assert!(feat_automations::plugin::filter_message_delete(&message_delete(&w, by_member.clone())));
    feat_automations::plugin::on_message_delete(message_delete(&w, by_member)).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    let posts = w.posts(&sid(40));
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1]["content"], format!("saw bye bye by user{}", sid(42)));

    // A message automod deleted never reaches MESSAGE_SENT automations.
    let automodded = message(1004, &w.g, &sid(40), &sid(42), "x", |_| {});
    let ctx = message_create(&w, automodded);
    ctx.state.set_automod_action(&bot_core::hooks::AutomodAction { rule: "spam".into(), deleted: true });
    feat_automations::plugin::on_message_create(ctx).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    assert_eq!(w.rest.calls_to("DELETE", "/channels/*/messages/*").len(), 1);

    // Bot, webhook and partial deletions never reach the hook.
    let bot_msg = message(1005, &w.g, &sid(40), TEST_BOT_ID, "x", |m| m["author"]["bot"] = json!(true));
    assert!(!feat_automations::plugin::filter_message_delete(&message_delete(&w, bot_msg)));
    let hook_msg = message(1006, &w.g, &sid(40), &sid(42), "x", |m| m["webhook_id"] = json!(sid(5)));
    assert!(!feat_automations::plugin::filter_message_delete(&message_delete(&w, hook_msg)));
}

fn enable_module(w: &World) {
    w.t.settings_store.put(bot_db::GuildSettings { guild_id: w.g.clone(), automations_enabled: true, ..Default::default() });
}

#[tokio::test]
async fn button_clicks_are_acknowledged_and_an_ephemeral_reply_follows() {
    let w = world("955028100000000011", |g| g.role(&sid(91), 1, Permissions::empty()));
    enable_module(&w);
    w.t.install(registry());
    w.memo(vec![lite(
        "accept-btn1",
        "BUTTON_CLICKED",
        json!([{ "type": "BUTTON_IS", "buttons": [{ "automationId": "panelrow01", "index": 1 }] }]),
        json!([{ "type": "GIVE_ROLES", "roleIds": [sid(91)] }, { "type": "REPLY", "content": "Done, {user}!" }]),
    )]);
    let click = |custom_id: &str| FakeInteraction::button(custom_id).guild(&w.g).user(&sid(42)).channel(&sid(40)).build();
    bot_core::router::route_interaction(&w.t, click("automation:panelrow01:2")).await;
    assert_eq!(w.t.responses.names(), vec!["deferUpdate"]);
    assert!(role_puts(&w).is_empty());
    bot_core::router::route_interaction(&w.t, click("automation:panelrow01:1")).await;
    assert_eq!(w.t.responses.names(), vec!["deferUpdate", "deferUpdate", "followUp"]);
    assert_eq!(role_puts(&w), vec![sid(91)]);
    let follow = &w.t.responses.calls()[2];
    assert!(follow.ephemeral());
    assert_eq!(follow.content(), Some(format!("Done, <@{}>!", sid(42)).as_str()));
    bot_core::router::route_interaction(&w.t, click("automation:bad:1")).await;
    bot_core::router::route_interaction(&w.t, click("automation:panelrow01:100")).await;
    assert_eq!(w.t.responses.names().iter().filter(|n| **n == "deferUpdate").count(), 4);
}

struct CoinsProvider(std::sync::Mutex<Vec<(i64, String, Option<String>)>>);

#[async_trait::async_trait]
impl RewardProvider for CoinsProvider {
    async fn grant(&self, _app: &App, args: RewardArgs) -> BotResult<RewardResult> {
        self.0.lock().expect("lock").push((args.amount, args.user_id.clone(), Some(args.reason.clone())));
        Ok(RewardResult::ok(Some(5)))
    }
    async fn take(&self, _app: &App, _args: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::fail("insufficient"))
    }
}

fn voice_state(w: &World, channel: Option<&str>) -> serenity::all::VoiceState {
    serde_json::from_value(json!({
        "channel_id": channel, "user_id": sid(42), "guild_id": w.g, "session_id": "s", "deaf": false, "mute": false, "self_deaf": false,
        "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null
    }))
    .expect("voice state deserializes")
}

#[tokio::test]
async fn reward_actions_go_through_the_rewards_bus() {
    let w = world("955028100000000012", |g| g.channel(&sid(95), 2, "Lounge", &[]));
    let provider = Arc::new(CoinsProvider(std::sync::Mutex::new(Vec::new())));
    w.t.rewards().register_provider(RewardKind::Coins, provider.clone(), Some("test-automations")).expect("provider");
    w.memo(vec![lite(
        "voice-a001",
        "VOICE_JOINED",
        json!([]),
        json!([{ "type": "GIVE_COINS", "amount": 5 }, { "type": "TAKE_COINS", "amount": 5 }, { "type": "GIVE_XP", "amount": 5 }]),
    )]);
    let ctx = Arc::new(HookCtx::new(
        w.t.app.clone(),
        VoiceStateUpdate { guild_id: w.gid, old_state: None, new_state: voice_state(&w, Some(&sid(95))), member: w.member(&sid(42)), change: VoiceChange::Join },
    ));
    feat_automations::plugin::on_voice_state(ctx).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    assert_eq!(provider.0.lock().expect("lock").clone(), vec![(5, sid(42), Some("automation:voice-a001".to_string()))], "XP has no provider: skipped, not failed");
    let problem = w.t.cache_backend.raw("automation:problem:voice-a001").unwrap_or_default();
    assert!(problem.contains("The Levels package is not installed"), "{problem}");
}

#[tokio::test]
async fn thread_created_ignores_bots_and_resolves_the_owner() {
    let w = world("955028100000000013", |g| {
        g.channel(&sid(96), 0, "forum", &[])
            .thread(&sid(97), 11, &sid(96))
            .thread_owner(&sid(97), &sid(42))
            .thread(&sid(99), 11, &sid(96))
            .thread_owner(&sid(99), &sid(120))
            .member(&sid(98), &[])
            .member_with(&sid(120), &[], |m| m["user"]["bot"] = json!(true))
    });
    w.memo(vec![lite(
        "thread-a01",
        "THREAD_CREATED",
        json!([{ "type": "CHANNEL_IN", "channelIds": [sid(96)] }]),
        json!([{ "type": "ADD_TO_THREAD", "userIds": [sid(98)] }, { "type": "SEND_IN_THREAD", "content": "hi {user}" }]),
    )]);
    let ctx = |id: u64| Arc::new(HookCtx::new(w.t.app.clone(), ThreadCreate { guild_id: w.gid, thread: w.channel(&sid(id)) }));
    assert!(feat_automations::plugin::filter_thread_create(&ctx(97)));
    feat_automations::plugin::on_thread_create(ctx(97)).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    assert_eq!(w.rest.calls_to("PUT", &format!("/channels/{}/thread-members/{}", sid(97), sid(98))).len(), 1);
    assert_eq!(w.posts(&sid(97))[0]["content"], format!("hi <@{}>", sid(42)));
    feat_automations::plugin::on_thread_create(ctx(99)).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    assert!(w.posts(&sid(99)).is_empty(), "a bot's thread is ignored");
}

#[tokio::test]
async fn a_failing_action_never_stops_the_next_one() {
    let w = world("955028100000000014", |g| g.channel(&sid(99), 0, "broken", &[]));
    w.rest.fail("POST", &format!("/channels/{}/messages", sid(99)), 403, 50013, "Missing Permissions");
    w.memo(vec![lite(
        "fail-a0001",
        "MESSAGE_SENT",
        json!([]),
        json!([
            { "type": "SEND_MESSAGE", "content": "first", "channelId": sid(99) },
            { "type": "SEND_MESSAGE", "content": "second", "channelId": sid(40) },
            { "type": "SEND_MESSAGE", "content": "to a missing channel", "channelId": sid(998) }
        ]),
    )]);
    let msg = message(1401, &w.g, &sid(40), &sid(42), "x", |_| {});
    w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg))).await;
    let sent: Vec<Value> = w.posts(&sid(40)).iter().map(|p| p["content"].clone()).collect();
    assert_eq!(sent, vec![json!("second")]);
    let problem: Value = serde_json::from_str(&w.t.cache_backend.raw("automation:problem:fail-a0001").unwrap_or_default()).expect("problem json");
    assert_eq!(problem["text"], "Action 3 (Send a message): The channel was deleted", "the last problem wins");
    assert!(problem["at"].as_str().is_some_and(|a| a.ends_with('Z')));
    assert_eq!(w.t.cache_backend.pttl("automation:problem:fail-a0001"), svc::PROBLEM_TTL_SECONDS as i64 * 1000);
}

#[tokio::test]
async fn create_thread_in_a_channel_remove_reaction_voice_moves_and_bot_joins() {
    let w = world("955028100000000017", |g| {
        g.channel(&sid(43), 5, "news", &[]).channel(&sid(601), 2, "Lounge", &[]).channel(&sid(602), 2, "Games", &[]).member_with(&sid(603), &[], |m| m["user"]["bot"] = json!(true))
    });
    script_threads(&w, &format!("/channels/{}/threads", sid(43)), 600, 10);
    w.memo(vec![
        lite(
            "join-thr01",
            "MEMBER_JOINED",
            json!([]),
            json!([
                { "type": "CREATE_THREAD", "name": "Welcome {user.name}", "channelId": sid(43), "autoArchiveMinutes": 4320 },
                { "type": "SEND_IN_THREAD", "content": "This is {thread.name} ({thread})" }
            ]),
        ),
        lite("voice-j001", "VOICE_JOINED", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "joined {voice.name}", "channelId": sid(41) }])),
        lite("voice-l001", "VOICE_LEFT", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "left {voice.name}", "channelId": sid(41) }])),
        lite("react-r001", "REACTION_ADDED", json!([{ "type": "REACTED_WITH", "emojis": ["🚫"] }]), json!([{ "type": "REMOVE_REACTION", "emoji": null }])),
    ]);
    let add = Arc::new(HookCtx::new(w.t.app.clone(), GuildMemberAdd { guild_id: w.gid, member: w.member(&sid(42)), user: w.member(&sid(42)).user }));
    assert!(feat_automations::plugin::filter_member_add(&add));
    feat_automations::plugin::on_member_add(add).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    let created = w.rest.calls_to("POST", &format!("/channels/{}/threads", sid(43)));
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].body["name"], format!("Welcome user{}", sid(42)));
    assert_eq!(created[0].body["auto_archive_duration"], 4320);
    assert_eq!(created[0].body["type"], 10, "an announcement thread");
    assert_eq!(w.posts(&sid(600))[0]["content"], format!("This is Welcome user{} (<#{}>)", sid(42), sid(600)), "{{thread}} is read at render time");

    let mv = Arc::new(HookCtx::new(
        w.t.app.clone(),
        VoiceStateUpdate {
            guild_id: w.gid,
            old_state: Some(voice_state(&w, Some(&sid(601)))),
            new_state: voice_state(&w, Some(&sid(602))),
            member: w.member(&sid(42)),
            change: VoiceChange::Move,
        },
    ));
    feat_automations::plugin::on_voice_state(mv).await.expect("hook");
    svc::settle_runs(&w.t, &w.g).await;
    let said: Vec<Value> = w.posts(&sid(41)).iter().map(|p| p["content"].clone()).collect();
    assert_eq!(said, vec![json!("left Lounge"), json!("joined Games")], "LEFT first, then JOINED");

    let mut msg = message_json(&sid(604), &w.g, &sid(40), &sid(42), "x");
    msg["reactions"] = json!([reaction_json("🚫", 1, false)]);
    let msg: serenity::all::Message = serde_json::from_value(msg).expect("message");
    let mut b = w.base("REACTION_ADDED", &sid(42), Some(&sid(40)), Some(msg));
    b.reaction = Some(svc::ReactionInfo { key: "🚫".into(), display: "🚫".into() });
    w.dispatch("REACTION_ADDED", b).await;
    let removed = w.rest.calls_to("DELETE", &format!("/channels/{}/messages/{}/reactions/**", sid(40), sid(604)));
    assert_eq!(removed.len(), 1);
    assert!(removed[0].path.ends_with(&sid(42)), "{}", removed[0].path);

    let bot_join = Arc::new(HookCtx::new(w.t.app.clone(), GuildMemberAdd { guild_id: w.gid, member: w.member(&sid(603)), user: w.member(&sid(603)).user }));
    assert!(!feat_automations::plugin::filter_member_add(&bot_join));
}

#[tokio::test]
async fn repost_and_message_content_never_copy_a_hidden_message_into_a_public_channel() {
    let g = "955028100000000021";
    let hide = [(g, false, Permissions::empty(), Permissions::VIEW_CHANNEL), (BOT_ROLE, false, Permissions::VIEW_CHANNEL, Permissions::empty())];
    let w = world(g, |f| f.channel(&sid(440), 0, "hidden", &hide).channel(&sid(441), 0, "open", &[]).channel(&sid(442), 0, "vault", &hide));
    let mut n = 0;
    let mut fire = async |source: u64, actions: Value| {
        n += 1;
        w.memo(vec![lite(&format!("autoleak{n:02}"), "MESSAGE_SENT", json!([]), actions)]);
        let msg = message(2100 + n, &w.g, &sid(source), &sid(42), "secret", |_| {});
        w.dispatch("MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(source)), Some(msg))).await;
    };
    fire(440, json!([{ "type": "REPOST", "channelId": sid(441) }])).await;
    assert_eq!(w.posts(&sid(441)).len(), 0, "hidden source, public target: skipped");
    fire(440, json!([{ "type": "REPOST", "channelId": sid(442) }])).await;
    assert_eq!(w.posts(&sid(442)).len(), 1, "hidden source, hidden target: reposted");
    fire(441, json!([{ "type": "REPOST", "channelId": sid(441) }])).await;
    assert_eq!(w.posts(&sid(441)).len(), 1, "public source: reposted");
    fire(440, json!([{ "type": "SEND_MESSAGE", "content": "said: {message.content}", "channelId": sid(441) }])).await;
    assert_eq!(w.posts(&sid(441)).len(), 1, "a text that copies the content is skipped too");
    fire(440, json!([{ "type": "SEND_MESSAGE", "content": "said: { Message.Old_Content }", "channelId": sid(441) }])).await;
    assert_eq!(w.posts(&sid(441)).len(), 1, "whatever the spelling of the placeholder");
    fire(440, json!([{ "type": "SEND_MESSAGE", "content": "someone posted in the vault", "channelId": sid(441) }])).await;
    assert_eq!(w.posts(&sid(441)).len(), 2, "a text that does not copy the message is fine");
    fire(440, json!([{ "type": "SEND_MESSAGE", "content": "said: {message.content}", "channelId": sid(442) }])).await;
    assert_eq!(w.posts(&sid(442)).len(), 2, "private to private is fine");
}

#[tokio::test]
async fn hooks_filter_on_the_memo() {
    let w = world("955028100000000018", |g| g);
    w.memo(vec![]);
    let msg = message(1801, &w.g, &sid(40), &sid(42), "x", |_| {});
    assert!(!feat_automations::plugin::filter_message_create(&message_create(&w, msg.clone())));
    w.memo(vec![lite("a1", "MESSAGE_SENT", json!([]), json!([{ "type": "PIN" }]))]);
    assert!(feat_automations::plugin::filter_message_create(&message_create(&w, msg)));
    assert!(svc::peek_has_trigger(&w.t, "never-seen", "MESSAGE_SENT"), "unknown guild: the run loads it");
    assert!(!svc::peek_has_trigger(&w.t, &w.g, "constructor"));
    assert!(!svc::peek_has_trigger(&w.t, &w.g, "MESSAGE_EDITED"));
    w.t.clock.advance(svc::MEMO_TTL_MS);
    assert!(svc::peek_has_trigger(&w.t, &w.g, "MESSAGE_EDITED"), "a stale memo passes");
}

// ── /automation option mapping and the configuration-time checks ─────────────

fn opt_ch(w: &World, id: &str) -> Option<svc::OptChannel> {
    let ch = w.channel(id);
    Some(svc::OptChannel { id: ch.id, kind: ch.kind })
}

fn rid(n: u64) -> serenity::all::RoleId {
    serenity::all::RoleId::new(id(n))
}

#[tokio::test]
async fn condition_params_from_options() {
    let w = world("955028100000000015", |g| {
        g.channel(&sid(100), 4, "cat", &[]).role(&sid(101), 1, Permissions::empty()).member(&sid(102), &[])
    });
    let guild = w.guild();
    let cond = |type_: &'static str, o: svc::ConditionOptions| {
        let (app, guild) = (w.t.app.clone(), guild.clone());
        async move { svc::condition_params_from_options(&app, type_, &o, &guild).await }
    };
    let text = |t: &str| svc::ConditionOptions { text: Some(t.into()), ..Default::default() };
    let r = cond("CHANNEL_IN", svc::ConditionOptions { channel: opt_ch(&w, &sid(40)), text: Some(format!("<#{}>", sid(41))), ..Default::default() }).await;
    assert_eq!(r.params, json!({ "channelIds": [sid(40), sid(41)] }));
    let r = cond("CHANNEL_IN", svc::ConditionOptions { channel: opt_ch(&w, &sid(100)), ..Default::default() }).await;
    assert_eq!(r.error.as_deref(), Some("That is a category. Use \"Category is one of\" instead."));
    assert_eq!(cond("CHANNEL_IN", Default::default()).await.error.as_deref(), Some("Pick a channel or mention channels in text."));
    let r = cond("CATEGORY_IN", svc::ConditionOptions { channel: opt_ch(&w, &sid(100)), ..Default::default() }).await;
    assert_eq!(r.params, json!({ "categoryIds": [sid(100)] }));
    let r = cond("CATEGORY_IN", svc::ConditionOptions { channel: opt_ch(&w, &sid(40)), ..Default::default() }).await;
    assert_eq!(r.error.as_deref(), Some("Pick a category (not a channel)."));
    assert_eq!(cond("CHANNEL_IN", text(&format!("<#{}>", sid(999)))).await.error, Some(format!("Unknown channel: {}", sid(999))));
    assert_eq!(cond("CONTAINS", text("a, B,,a")).await.params, json!({ "words": ["a", "B"] }));
    assert_eq!(cond("CONTAINS", text(" , ")).await.error.as_deref(), Some("Give words in text, separated by commas."));
    assert_eq!(cond("CONTAINS", text(&"x".repeat(101))).await.error.as_deref(), Some("Each word can be at most 100 characters."));
    assert_eq!(cond("EXACT", text(" Hi there ")).await.params, json!({ "texts": ["Hi there"] }));
    assert_eq!(cond("MATCHES_REGEX", text("/^hi/im")).await.params, json!({ "pattern": "^hi", "flags": "im" }));
    assert_eq!(cond("MATCHES_REGEX", text("^hi")).await.params, json!({ "pattern": "^hi", "flags": "i" }));
    assert!(cond("MATCHES_REGEX", text("(a+)+")).await.error.unwrap_or_default().starts_with("Invalid regex: "));
    assert!(cond("MATCHES_REGEX", text("/a/g")).await.error.unwrap_or_default().starts_with("Invalid regex: "));
    assert_eq!(cond("REACTED_WITH", text("⭐ 👍")).await.params, json!({ "emojis": ["⭐", "👍"] }));
    assert_eq!(cond("REACTED_WITH", text("")).await.error.as_deref(), Some("Give one or more emojis in text."));
    let r = cond("HAS_REACTIONS", svc::ConditionOptions { number: Some(3), text: Some("⭐".into()), ..Default::default() }).await;
    assert_eq!(r.params, json!({ "min": 3, "emoji": "⭐" }));
    assert_eq!(cond("HAS_REACTIONS", Default::default()).await.error.as_deref(), Some("Give the minimum count in number."));
    let r = cond("HAS_ANY_ROLE", svc::ConditionOptions { role: Some(rid(101)), text: Some(format!("<@&{}>", sid(101))), ..Default::default() }).await;
    assert_eq!(r.params, json!({ "roleIds": [sid(101)] }));
    assert_eq!(cond("HAS_ANY_ROLE", text(&format!("<@&{}>", sid(999)))).await.error, Some(format!("Unknown role: {}", sid(999))));
    assert_eq!(cond("HAS_ANY_ROLE", text(&format!("<@&{}>", w.g))).await.error.as_deref(), Some("The @everyone role cannot be used here."));
    assert_eq!(cond("HAS_ANY_ROLE", Default::default()).await.error.as_deref(), Some("Pick a role or mention roles in text."));
    assert_eq!(cond("USER_IS", text(&format!("<@{}>", sid(102)))).await.params, json!({ "userIds": [sid(102)] }));
    assert_eq!(cond("USER_IS", text(&format!("<@{}>", sid(777)))).await.error, Some(format!("Unknown user: {}", sid(777))));
    assert_eq!(cond("BUTTON_IS", svc::ConditionOptions { button: Some("nope".into()), ..Default::default() }).await.error.as_deref(), Some("Pick a button from the list."));
    let r = cond("IS_REPLY", svc::ConditionOptions { text: Some("ignored".into()), role: Some(rid(101)), ..Default::default() }).await;
    assert_eq!(r.ignored, vec!["role", "text"]);
    assert!(!cond("NOPE", Default::default()).await.ok);
}

#[tokio::test]
async fn action_params_from_options() {
    let deny_send = [(BOT_ROLE, false, Permissions::empty(), Permissions::SEND_MESSAGES)];
    let w = world("955028100000000016", |g| {
        g.channel(&sid(100), 4, "cat", &[])
            .role(&sid(101), 1, Permissions::empty())
            .role(&sid(105), 40, Permissions::empty())
            .role(&sid(106), 120, Permissions::empty())
            .member(&sid(102), &[])
            .member_with(&sid(103), &[], |m| m["user"]["bot"] = json!(true))
            .member(&sid(104), &[&sid(105)])
            .channel(&sid(107), 0, "locked", &deny_send)
    });
    let guild = w.guild();
    let executor = w.member(&sid(104));
    let act = |type_: &'static str, o: svc::ActionOptions| {
        let (app, guild, executor) = (w.t.app.clone(), guild.clone(), executor.clone());
        async move {
            let ctx = svc::ActionContext { guild: &guild, member: Some(&executor), settings: None, fallback_channel: None };
            svc::action_params_from_options(&app, type_, &o, ctx).await
        }
    };
    let text = |t: &str| svc::ActionOptions { text: Some(t.into()), ..Default::default() };
    assert_eq!(act("CREATE_THREAD", Default::default()).await.params, json!({ "name": svc::DEFAULT_THREAD_NAME, "channelId": null, "autoArchiveMinutes": 1440 }));
    let r = act("CREATE_THREAD", svc::ActionOptions { channel: opt_ch(&w, &sid(100)), ..Default::default() }).await;
    assert_eq!(r.error.as_deref(), Some("Pick a text or announcement channel."));
    let send = act(
        "SEND_MESSAGE",
        svc::ActionOptions { text: Some("hi".into()), channel: opt_ch(&w, &sid(40)), button_label: Some("Go".into()), button_url: Some("https://x.y".into()), ..Default::default() },
    )
    .await;
    assert_eq!(s(&send.params), s(&json!({ "content": "hi", "channelId": sid(40), "button": { "label": "Go", "url": "https://x.y", "style": "PRIMARY", "index": null } })));
    let own = act("SEND_MESSAGE", svc::ActionOptions { text: Some("hi".into()), button_label: Some("Go".into()), ..Default::default() }).await;
    assert_eq!(own.params["button"], json!({ "label": "Go", "url": null, "style": "PRIMARY", "index": null }));
    let r = act("SEND_MESSAGE", svc::ActionOptions { text: Some("hi".into()), button_url: Some("https://x.y".into()), ..Default::default() }).await;
    assert_eq!(r.error.as_deref(), Some("Give button_label too."));
    let r = act("SEND_MESSAGE", svc::ActionOptions { text: Some("hi".into()), button_label: Some("Go".into()), button_url: Some("ftp://x".into()), ..Default::default() }).await;
    assert_eq!(r.error.as_deref(), Some("The button link must start with http:// or https://."));
    assert_eq!(act("SEND_MESSAGE", Default::default()).await.error.as_deref(), Some("Give the message in text."));
    let denied = act("SEND_MESSAGE", svc::ActionOptions { text: Some("hi".into()), channel: opt_ch(&w, &sid(107)), ..Default::default() }).await;
    assert_eq!((denied.ok, denied.error_kind, denied.missing.clone()), (false, Some(svc::ErrorKind::Perms), vec!["SendMessages".to_string()]));
    assert_eq!(act("REPOST", Default::default()).await.error.as_deref(), Some("Pick the channel to repost to."));
    assert_eq!(act("REPOST", svc::ActionOptions { channel: opt_ch(&w, &sid(107)), ..Default::default() }).await.error_kind, Some(svc::ErrorKind::Perms));
    assert_eq!(act("REACT", svc::ActionOptions { emoji: Some("👍".into()), ..Default::default() }).await.params, json!({ "emoji": "👍" }));
    assert!(!act("REACT", Default::default()).await.ok);
    assert_eq!(act("REMOVE_REACTION", Default::default()).await.params, json!({ "emoji": null }));
    assert_eq!(act("GIVE_ROLES", svc::ActionOptions { role: Some(rid(101)), ..Default::default() }).await.params, json!({ "roleIds": [sid(101)] }));
    assert!(!act("GIVE_ROLES", text(&format!("<@&{}>", w.g))).await.ok);
    let refused = act("GIVE_ROLES", svc::ActionOptions { role: Some(rid(106)), ..Default::default() }).await;
    assert_eq!((refused.ok, refused.error_kind), (false, Some(svc::ErrorKind::Role)));
    assert_eq!(refused.error.as_deref(), Some("That role is above my highest role."));
    assert_eq!(act("ADD_TO_THREAD", text(&format!("<@{}>", sid(102)))).await.params, json!({ "userIds": [sid(102)] }));
    let bot_user = svc::OptUser { id: serenity::all::UserId::new(id(103)), bot: true };
    let r = act("ADD_TO_THREAD", svc::ActionOptions { user: Some(bot_user), ..Default::default() }).await;
    assert_eq!(r.error.as_deref(), Some("Bots cannot be added to threads by automations."));
    assert_eq!(act("ADD_TO_THREAD", Default::default()).await.params, json!({ "userIds": [] }));
    assert_eq!(act("GIVE_COINS", Default::default()).await.error.as_deref(), Some("Give the amount."));
    let no_rewards = act("GIVE_XP", svc::ActionOptions { amount: Some(5), ..Default::default() }).await;
    assert_eq!((no_rewards.error_kind, no_rewards.reward_kind), (Some(svc::ErrorKind::Reward), Some("xp")));
    assert_eq!(act("PIN", text("x")).await.ignored, vec!["text"]);
    assert!(!act("NOPE", Default::default()).await.ok);

    // The JS had the option's resolved channel object: a channel the cache does not hold yet is fetched over REST
    // (and judged like a cached one), not refused as "Channel not found".
    let remote = sid(150);
    w.rest.on(
        "GET",
        &format!("/channels/{remote}"),
        200,
        json!({ "id": remote, "type": 0, "guild_id": w.g, "name": "remote", "position": 9, "permission_overwrites": [], "nsfw": false, "parent_id": null }),
    );
    let far = || Some(svc::OptChannel { id: serenity::all::ChannelId::new(id(150)), kind: serenity::all::ChannelType::Text });
    let r = act("SEND_MESSAGE", svc::ActionOptions { text: Some("hi".into()), channel: far(), ..Default::default() }).await;
    assert_eq!((r.ok, r.params["channelId"].clone()), (true, json!(remote)), "{:?}", r.error);
    let r = act("CREATE_THREAD", svc::ActionOptions { channel: far(), ..Default::default() }).await;
    assert_eq!((r.ok, r.params["channelId"].clone()), (true, json!(remote)), "{:?}", r.error);
    let gone = act("REPOST", svc::ActionOptions { channel: Some(svc::OptChannel { id: serenity::all::ChannelId::new(id(151)), kind: serenity::all::ChannelType::Text }), ..Default::default() }).await;
    assert!(!gone.ok, "a channel Discord does not know either is still refused");
}

/// A guild for the access checks: roles at 2, 5 and 8, an admin at 5, open / secret / chatty channels.
fn access_world(g: &str) -> World {
    world(g, |f| {
        f.role(&sid(425), 2, Permissions::empty())
            .role(&sid(426), 5, Permissions::empty())
            .role(&sid(427), 8, Permissions::empty())
            .role_with(&sid(435), 1, Permissions::empty(), |r| r["mentionable"] = json!(true))
            .role(&sid(434), 1, Permissions::empty())
            .role(&sid(437), 3, Permissions::ADMINISTRATOR)
            .member(&sid(436), &[&sid(426)])
            .member(&sid(438), &[&sid(437)])
            .member(OWNER, &[])
            .channel(&sid(431), 0, "open", &[])
            .channel(&sid(432), 0, "secret", &[(&sid(436), true, Permissions::empty(), Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)])
            .channel(&sid(433), 0, "chatty", &[(&sid(436), true, Permissions::MENTION_EVERYONE, Permissions::empty())])
    })
}

#[tokio::test]
async fn import_role_hierarchy_owner_and_unknown_roles() {
    let w = access_world("955028900000000020");
    let guild = w.guild();
    let me = w.member(&sid(436));
    let give = |ids: &[u64]| vec![json!({ "type": "SEND_MESSAGE", "content": "x" }), json!({ "type": "GIVE_ROLES", "roleIds": ids.iter().map(|n| sid(*n)).collect::<Vec<_>>() })];
    assert_eq!(svc::import_role_hierarchy_problem(&guild, Some(&me), &give(&[425])), None, "below the importer");
    assert_eq!(svc::import_role_hierarchy_problem(&guild, Some(&me), &give(&[427])).as_deref(), Some("Action 2: That role is above your highest role."));
    assert!(svc::import_role_hierarchy_problem(&guild, Some(&me), &give(&[425, 426])).is_some(), "equal position counts as above");
    assert!(svc::import_role_hierarchy_problem(&guild, Some(&me), &[json!({ "type": "TAKE_ROLES", "roleIds": [sid(427)] })]).unwrap_or_default().starts_with("Action 1"));
    assert_eq!(svc::import_role_hierarchy_problem(&guild, Some(&w.member(OWNER)), &give(&[427])), None, "owner");
    assert_eq!(svc::import_role_hierarchy_problem(&guild, Some(&me), &give(&[477])), None, "a deleted role");
    assert_eq!(svc::import_role_hierarchy_problem(&guild, Some(&me), &[json!({ "type": "PIN" })]), None);
    assert_eq!(svc::import_role_hierarchy_problem(&guild, None, &give(&[427])), None, "no member: nothing to compare with");
}

#[tokio::test]
async fn channel_access_role_pings_and_imported_actions() {
    let w = access_world("955028900000000021");
    let guild = w.guild();
    let me = w.member(&sid(436));
    let actions = vec![json!({ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(431) }), json!({ "type": "REPOST", "channelId": sid(432) })];
    assert_eq!(svc::action_channel_access_problem(&guild, Some(&me), &actions[..1], 1), None);
    assert_eq!(
        svc::action_channel_access_problem(&guild, Some(&me), &actions, 1),
        Some(format!("Action 2: You need **View Channel, Send Messages** in <#{}> to do that.", sid(432)))
    );
    assert!(svc::action_channel_access_problem(&guild, Some(&me), &actions, 5).unwrap_or_default().starts_with("Action 6:"));
    assert_eq!(svc::action_channel_access_problem(&guild, Some(&w.member(&sid(438))), &actions, 1), None, "Administrator");
    assert_eq!(svc::action_channel_access_problem(&guild, Some(&w.member(OWNER)), &actions, 1), None, "owner");
    assert_eq!(svc::action_channel_access_problem(&guild, Some(&me), &[json!({ "type": "REPOST", "channelId": sid(499) })], 1), None, "a deleted channel");
    assert_eq!(svc::action_channel_access_problem(&guild, Some(&me), &[json!({ "type": "SEND_MESSAGE", "content": "x", "channelId": null }), json!({ "type": "PIN" })], 1), None);
    assert!(svc::action_channel_access_problem(&guild, Some(&me), &[json!({ "type": "CREATE_THREAD", "channelId": sid(432) })], 1).unwrap_or_default().contains("Create Public Threads"));
    assert!(svc::action_channel_access_problem(&guild, Some(&me), &[json!({ "type": "SEND_EMBED", "embedName": "e", "channelId": sid(432) })], 1).unwrap_or_default().starts_with("Action 1:"));

    let open = w.channel(&sid(431));
    let chatty = w.channel(&sid(433));
    assert_eq!(svc::role_ping_refusal(&guild, Some(&me), Some(&open), &format!("hi <@&{}>", sid(435))), None, "a mentionable role");
    assert_eq!(
        svc::role_ping_refusal(&guild, Some(&me), Some(&open), &format!("hi <@&{}>", sid(434))),
        Some(format!("<@&{}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{}>.", sid(434), sid(431)))
    );
    assert!(svc::role_ping_refusal(&guild, Some(&me), Some(&open), &format!("<@&{}> <@&{}>", sid(434), sid(498))).unwrap_or_default().contains("are not mentionable"));
    assert_eq!(svc::role_ping_refusal(&guild, Some(&me), Some(&chatty), &format!("hi <@&{}>", sid(434))), None, "Mention Everyone in the channel");
    assert_eq!(svc::role_ping_refusal(&guild, None, Some(&open), &format!("hi <@&{}>", sid(434))), None);
    assert_eq!(svc::role_ping_refusal(&guild, Some(&me), Some(&open), "no roles here"), None);

    let reply = vec![json!({ "type": "SEND_MESSAGE", "content": format!("<@&{}>", sid(434)), "channelId": null })];
    assert!(svc::import_role_ping_problem(&guild, Some(&me), &reply, Some(&open)).unwrap_or_default().starts_with("Action 1: "));
    assert_eq!(svc::import_role_ping_problem(&guild, Some(&me), &reply, Some(&chatty)), None);
    let in_chatty = vec![json!({ "type": "SEND_MESSAGE", "content": format!("<@&{}>", sid(434)), "channelId": sid(433) })];
    assert_eq!(svc::import_role_ping_problem(&guild, Some(&me), &in_chatty, Some(&open)), None, "the action channel wins");
}

#[tokio::test]
async fn descriptions_flag_deleted_entities_in_a_known_guild() {
    let w = world("955028100000000019", |g| g);
    let guild = w.guild();
    assert!(svc::describe_condition(&json!({ "type": "CHANNEL_IN", "channelIds": [sid(999)] }), Some(&guild)).contains("deleted"));
    assert_eq!(svc::describe_condition(&json!({ "type": "CHANNEL_IN", "channelIds": [sid(40)] }), Some(&guild)), format!("Channel is one of: <#{}>", sid(40)));
    assert_eq!(svc::describe_condition(&Value::Null, Some(&guild)), "Unknown condition");
    let vars = svc::build_vars(&w.base("MESSAGE_SENT", &sid(42), None, None), "my-auto", &Arc::new(std::sync::Mutex::new(None)), Some(&guild));
    let names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, vec!["automation", "reaction", "thread", "thread.name"]);
}

// ── Database (TEST_DATABASE_URL) ─────────────────────────────────────────────

async fn db_app(guilds: &[&str]) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds(guilds.iter().map(|g| g.to_string()));
    t.reset_guilds(guilds).await;
    Some(t)
}

fn input(name: &str, extra: Value) -> svc::AutomationInput {
    let mut base = json!({ "trigger": "MESSAGE_SENT", "conditions": [], "actions": [{ "type": "PIN" }], "enabled": true, "cooldownSeconds": 0 });
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    svc::AutomationInput {
        name: Some(name.to_string()),
        trigger: base["trigger"].as_str().unwrap_or("").to_string(),
        conditions: arr(&base["conditions"]),
        actions: arr(&base["actions"]),
        enabled: base["enabled"] == true,
        cooldown_seconds: base["cooldownSeconds"].as_i64().unwrap_or(0),
    }
}

fn code_of(e: &anyhow::Error) -> Option<svc::ErrorCode> {
    svc::as_automation_error(e).map(|a| a.code)
}

#[tokio::test]
async fn db_create_enforces_the_cap_under_the_lock_the_unique_name_and_the_shape() {
    let g = "955028200000000001";
    let Some(t) = db_app(&[g]).await else { return };
    let made = svc::create_automation(&t, g, &input("First Rule", json!({})), "creator-1", None).await.expect("create");
    assert_eq!((made.name.as_str(), made.created_by.as_str(), made.enabled), ("first-rule", "creator-1", true));
    let taken = svc::create_automation(&t, g, &input("first-rule", json!({})), "creator-1", None).await.expect_err("taken");
    assert!(svc::is_unique_violation(&taken));
    let bad_name = svc::create_automation(&t, g, &input("!!!", json!({})), "creator-1", None).await.expect_err("name");
    assert_eq!(code_of(&bad_name), Some(svc::ErrorCode::Name));
    let shape = input("bad", json!({ "trigger": "MEMBER_JOINED", "actions": [{ "type": "REPLY", "content": "x" }] }));
    assert!(code_of(&svc::create_automation(&t, g, &shape, "c", None).await.expect_err("shape")).is_some());
    let no_creator = svc::create_automation(&t, g, &input("nocreator", json!({})), "", None).await.expect_err("creator");
    assert_eq!(code_of(&no_creator), None);
    let blank = svc::create_automation(&t, g, &input("blank", json!({ "actions": [], "enabled": true })), "c", None).await.expect("blank");
    assert!(!blank.enabled, "a row with no actions cannot be enabled");

    // Fill to the cap concurrently: the lock lets exactly LIMITS.AUTOMATIONS rows through.
    let attempts = futures::future::join_all((0..bot_config::limits::AUTOMATIONS + 5).map(|i| {
        let app = t.app.clone();
        async move { svc::create_automation(&app, g, &input(&format!("bulk-{i}"), json!({})), "c", None).await }
    }))
    .await;
    let (count,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Automation" WHERE "guildId" = $1"#).bind(g).fetch_one(&t.pool).await.expect("count");
    assert_eq!(count, bot_config::limits::AUTOMATIONS as i64);
    let limited: Vec<&anyhow::Error> = attempts.iter().filter_map(|a| a.as_ref().err()).collect();
    assert!(limited.len() >= 7, "{}", limited.len());
    assert!(limited.iter().all(|e| code_of(e) == Some(svc::ErrorCode::Limit)), "{:?}", limited.iter().map(|e| e.to_string()).collect::<Vec<_>>());
    t.reset_guilds(&[g]).await;
}

fn new_button_action(label: &str) -> Value {
    json!({ "type": "SEND_MESSAGE", "content": label, "channelId": sid(1), "button": { "label": label, "url": null, "style": "PRIMARY", "index": null } })
}

async fn add_button(t: &DbTestApp, g: &str, label: &str) -> anyhow::Result<(svc::AutomationRecord, svc::AppendedAction)> {
    let action = new_button_action(label);
    svc::mutate_automation(t, g, "panel", move |cur| {
        let r = svc::append_action(&cur.actions, Some(&json!(cur.button_seq)), &action)?;
        Ok(svc::Mutation::new(vec![("actions", Value::Array(r.actions.clone())), ("buttonSeq", json!(r.button_seq))], r))
    })
    .await
}

#[tokio::test]
async fn db_mutate_serialises_concurrent_button_adds_and_numbers_are_never_reused() {
    let g = "955028200000000002";
    let Some(t) = db_app(&[g]).await else { return };
    svc::create_automation(&t, g, &input("panel", json!({ "actions": [], "enabled": false })), "c", None).await.expect("create");
    let results = futures::future::join_all(["a", "b", "c", "d"].map(|l| add_button(&t, g, l))).await;
    let mut indexes: Vec<i64> = results.iter().map(|r| r.as_ref().expect("added").1.button_index.unwrap_or(0)).collect();
    indexes.sort_unstable();
    assert_eq!(indexes, vec![1, 2, 3, 4]);
    let row = svc::get_automation_by_name(&t.pool, g, "panel").await.expect("read").expect("row");
    assert_eq!((row.actions.len(), row.button_seq), (4, 4));
    // Remove the highest button, add another: the number is not reused.
    let position = row.actions.iter().position(|a| svc::automation_button_index(a) == Some(4)).expect("button 4") as i64 + 1;
    svc::mutate_automation(&t, g, "panel", move |cur| Ok(svc::Mutation::new(vec![("actions", Value::Array(svc::remove_at(&cur.actions, position)?.0))], ())))
        .await
        .expect("remove");
    assert_eq!(add_button(&t, g, "e").await.expect("add").1.button_index, Some(5));
    let missing = svc::mutate_automation(&t, g, "missing", |_| Ok(svc::Mutation::new(Vec::new(), ()))).await.expect_err("missing");
    assert_eq!(code_of(&missing), Some(svc::ErrorCode::NotFound));
    assert_eq!(svc::as_automation_error(&missing).map(|e| e.message.clone()), Some(svc::error_texts::not_found("missing")));
    // A shape error rolls back and changes nothing.
    let shape = svc::mutate_automation(&t, g, "panel", |_| {
        Ok(svc::Mutation::new(vec![("actions", json!([{ "type": "ADD_TO_THREAD", "userIds": [] }])), ("trigger", json!("MEMBER_JOINED"))], ()))
    })
    .await
    .expect_err("shape");
    assert!(code_of(&shape).is_some());
    assert_eq!(svc::get_automation_by_name(&t.pool, g, "panel").await.expect("read").expect("row").actions.len(), 4);
    // The ten-action cap holds under the lock.
    for _ in 0..6 {
        let _ = svc::mutate_automation(&t, g, "panel", |cur| {
            Ok(svc::Mutation::new(vec![("actions", Value::Array(svc::append_action(&cur.actions, None, &json!({ "type": "PIN" }))?.actions))], ()))
        })
        .await;
    }
    assert_eq!(svc::get_automation_by_name(&t.pool, g, "panel").await.expect("read").expect("row").actions.len(), bot_config::limits::AUTOMATION_ACTIONS);
    // Rename onto a taken name: a unique violation and the transaction rolls back.
    svc::create_automation(&t, g, &input("other", json!({})), "c", None).await.expect("other");
    let renamed = svc::mutate_automation(&t, g, "panel", |_| Ok(svc::Mutation::new(vec![("name", json!("other"))], ()))).await.expect_err("taken");
    assert!(svc::is_unique_violation(&renamed));
    assert!(svc::get_automation_by_name(&t.pool, g, "panel").await.expect("read").is_some());
    let buttons: Vec<i64> = svc::list_automation_buttons(&t.pool, g).await.iter().map(|b| b.index).collect();
    assert_eq!(buttons, vec![1, 2, 3, 5]);
    assert_eq!(svc::search_automations(&t.pool, g, "pan").await[0].name, "panel");
    assert!(svc::search_automations(&t.pool, g, "zzz").await.is_empty());
    let mut names: Vec<String> = svc::list_automations(&t.pool, g).await.expect("list").into_iter().map(|r| r.name).collect();
    names.sort();
    assert_eq!(names, vec!["other", "panel"]);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_loading_caching_and_invalidation() {
    let g = "955028200000000003";
    let Some(t) = db_app(&[g]).await else { return };
    assert!(svc::get_automations(&t, g).await.expect("loads").is_empty());
    svc::create_automation(&t, g, &input("on-one", json!({ "trigger": "MESSAGE_EDITED" })), "c", None).await.expect("on");
    svc::create_automation(&t, g, &input("off-one", json!({ "trigger": "MESSAGE_EDITED", "enabled": false })), "c", None).await.expect("off");
    let loaded = svc::get_automations(&t, g).await.expect("loads");
    assert_eq!(loaded.keys().cloned().collect::<Vec<_>>(), vec!["MESSAGE_EDITED"]);
    assert_eq!(loaded["MESSAGE_EDITED"].iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["on-one"]);
    assert!(svc::peek_has_trigger(&t, g, "MESSAGE_EDITED"));
    assert!(!svc::peek_has_trigger(&t, g, "MESSAGE_SENT"));
    let cached = t.app.cache().get(&svc::automations_cache_key(g)).await.expect("cached");
    assert_eq!(cached["v"], 1);
    assert_eq!(cached["byTrigger"]["MESSAGE_EDITED"][0]["name"], "on-one");
    svc::mutate_automation(&t, g, "off-one", |_| Ok(svc::Mutation::new(vec![("enabled", json!(true))], ()))).await.expect("enable");
    assert_eq!(svc::get_automations(&t, g).await.expect("loads")["MESSAGE_EDITED"].len(), 2);
    // A row whose stored actions are all invalid is not cached; invalid elements are dropped, not fatal.
    sqlx::query(r#"UPDATE "Automation" SET "actions" = '[{"type":"NOPE"}]'::jsonb WHERE "guildId" = $1 AND "name" = 'off-one'"#).bind(g).execute(&t.pool).await.expect("update");
    svc::invalidate_automations(&t, g).await;
    assert_eq!(svc::get_automations(&t, g).await.expect("loads")["MESSAGE_EDITED"].iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["on-one"]);
    let hydrated = svc::get_automation_by_name(&t.pool, g, "off-one").await.expect("read").expect("row");
    assert_eq!(hydrated.invalid_actions.iter().map(|x| x.index).collect::<Vec<_>>(), vec![1]);
    assert!(hydrated.actions.is_empty());
    assert_eq!(svc::delete_automation(&t, g, &hydrated.id).await.expect("delete"), 1);
    assert_eq!(svc::delete_automation(&t, g, &hydrated.id).await.expect("delete"), 0);
    assert_eq!(svc::get_automations(&t, g).await.expect("loads")["MESSAGE_EDITED"].len(), 1);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_replace_keeps_uses_and_creator_and_the_self_reference_is_rewritten() {
    let g = "955028200000000004";
    let Some(t) = db_app(&[g]).await else { return };
    let btn = |index: i64| json!({ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(1), "button": { "label": "B", "url": null, "style": "PRIMARY", "index": index } });
    let data = input(
        "selfref",
        json!({ "trigger": "BUTTON_CLICKED", "conditions": [{ "type": "BUTTON_IS", "buttons": [{ "automationId": "clxexported1", "index": 7 }] }], "actions": [btn(7)] }),
    );
    let created = svc::create_automation(&t, g, &data, "creator-x", Some("clxexported1")).await.expect("create");
    assert_eq!(created.button_seq, 7);
    assert_eq!(created.conditions[0]["buttons"][0]["automationId"], json!(created.id));
    sqlx::query(r#"UPDATE "Automation" SET "uses" = 41 WHERE "id" = $1"#).bind(&created.id).execute(&t.pool).await.expect("uses");
    let replaced = svc::replace_automation(&t, g, &created.id, &input("selfref-renamed", json!({ "actions": [btn(2)], "cooldownSeconds": 9 }))).await.expect("replace");
    assert_eq!(replaced.name, "selfref-renamed");
    assert_eq!((replaced.uses, replaced.created_by.as_str(), replaced.button_seq, replaced.cooldown_seconds), (41, "creator-x", 7, 9), "buttonSeq never shrinks");
    let nf = svc::replace_automation(&t, g, "doesnotexist", &input("x", json!({}))).await.expect_err("missing");
    assert_eq!(code_of(&nf), Some(svc::ErrorCode::NotFound));
    svc::create_automation(&t, g, &input("on-one", json!({})), "c", None).await.expect("on-one");
    assert!(svc::is_unique_violation(&svc::replace_automation(&t, g, &created.id, &input("on-one", json!({}))).await.expect_err("taken")));
    assert!(svc::list_automation_refs(&t.pool, g).await.iter().any(|r| r.id == created.id));

    // resolveImportReferences: a remap by name is informational, a dangling reference is a warning, the self reference
    // follows the row.
    let w = world(g, |f| f);
    let guild = w.guild();
    let import = svc::ImportData {
        name: None,
        trigger: "BUTTON_CLICKED".into(),
        enabled: true,
        cooldown_seconds: 0,
        exported_id: Some("clxexported2".into()),
        conditions: vec![json!({ "type": "BUTTON_IS", "buttons": [
            { "automationId": "clxexported2", "index": 1 },
            { "automationId": "clxgoneother", "index": 1, "automationName": "selfref-renamed" },
            { "automationId": "clxdangling1", "index": 3 }
        ] })],
        actions: vec![json!({ "type": "PIN" })],
    };
    let r = svc::resolve_import_references(&t, &guild, &import, None, Some(&created.id)).await;
    let pairs: Vec<(Value, Value)> = arr(&r.data.conditions[0]["buttons"]).iter().map(|b| (b["automationId"].clone(), b["index"].clone())).collect();
    assert_eq!(pairs, vec![(json!(created.id), json!(1)), (json!("clxdangling1"), json!(3))], "the duplicate is collapsed");
    assert_eq!((r.info.len(), r.warnings.len()), (1, 1));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_resolve_import_references_flags_what_does_not_exist_here() {
    let g = "955028200000000005";
    let Some(t) = db_app(&[g]).await else { return };
    let w = world(g, |f| f.role(&sid(110), 1, Permissions::empty()).role(&sid(111), 200, Permissions::empty()).member(&sid(112), &[]));
    t.app.set_discord(w.rest.discord(&[]));
    let guild = w.guild();
    let data = svc::ImportData {
        name: Some("x".into()),
        trigger: "MESSAGE_SENT".into(),
        enabled: true,
        cooldown_seconds: 0,
        exported_id: Some("clxselfid001".into()),
        conditions: svc::normalize_items(
            &json!([
                { "type": "CHANNEL_IN", "channelIds": [sid(40), sid(900)] }, { "type": "HAS_ANY_ROLE", "roleIds": [sid(110), sid(901)] },
                { "type": "USER_IS", "userIds": [sid(112), sid(902)] }, { "type": "CATEGORY_IN", "categoryIds": [sid(40)] }
            ]),
            ListKind::Condition,
        ),
        actions: svc::normalize_items(
            &json!([
                { "type": "GIVE_ROLES", "roleIds": [sid(110), sid(111)] }, { "type": "GIVE_XP", "amount": 5 },
                { "type": "SEND_MESSAGE", "content": "x", "channelId": sid(903) }
            ]),
            ListKind::Action,
        ),
    };
    let r = svc::resolve_import_references(&t, &guild, &data, None, None).await;
    let text = r.warnings.join("\n");
    assert!(text.contains(&format!("Condition 1: Unknown channel {}", sid(900))), "{text}");
    assert!(text.contains(&format!("Condition 2: Unknown role {}", sid(901))), "{text}");
    assert!(text.contains(&format!("Unknown member {}", sid(902))), "{text}");
    assert!(text.contains(&format!("Condition 4: {} is not a category", sid(40))), "{text}");
    assert!(text.contains("Action 1: That role is above my highest role"), "{text}");
    assert!(text.contains("Action 2: XP rewards are not available (module off)"), "{text}");
    assert!(text.contains(&format!("Action 3: Unknown channel {}", sid(903))), "{text}");
    assert_eq!(r.warnings.len(), 7, "{text}");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_raw_statements_run_and_the_run_counter_clamps() {
    let g = "955028200000000006";
    let Some(t) = db_app(&[g]).await else { return };
    let row = svc::create_automation(&t, g, &input("counter", json!({})), "c", None).await.expect("create");
    // Every raw statement runs against the database, inside a transaction that is rolled back.
    let mut tx = t.pool.begin().await.expect("tx");
    for raw in svc::RAW_SQL {
        let mut q = sqlx::query(raw.sql);
        for p in raw.params {
            q = q.bind(*p);
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{} runs: {e}", raw.name));
    }
    tx.rollback().await.expect("rollback");
    let w = world(g, |f| f);
    t.app.set_discord(w.rest.discord(&[w.guild_json()]));
    let a = AutomationLite { id: row.id.clone(), ..lite("x", "MESSAGE_SENT", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(40) }])) };
    let mut base = w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), None);
    base.guild_id = w.gid;
    sqlx::query(r#"UPDATE "Automation" SET "uses" = 2147483647 WHERE "id" = $1"#).bind(&row.id).execute(&t.pool).await.expect("uses");
    svc::execute_automation(&t, &a, &base).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let uses = |t: &DbTestApp| {
        let (pool, id) = (t.pool.clone(), row.id.clone());
        async move { sqlx::query_as::<_, (i32,)>(r#"SELECT "uses" FROM "Automation" WHERE "id" = $1"#).bind(id).fetch_one(&pool).await.expect("uses").0 }
    };
    assert_eq!(uses(&t).await, 2147483647, "clamped, no overflow error");
    sqlx::query(r#"UPDATE "Automation" SET "uses" = 10 WHERE "id" = $1"#).bind(&row.id).execute(&t.pool).await.expect("uses");
    svc::execute_automation(&t, &a, &base).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(uses(&t).await, 11);
    assert_eq!(w.posts(&sid(40)).len(), 2);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_send_embed_renders_a_saved_embed_through_the_embeds_contract() {
    let g = "955028200000000007";
    let Some(t) = db_app(&[g]).await else { return };
    let tpl = json!({ "content": "Hello {user}", "embed": { "title": "Rules for {user.name}", "description": "Be nice in {channel}" }, "buttons": [] });
    feat_embeds::embed_service::create_saved_embed(&t, g, "rules-embed", &tpl, "c").await.expect("embed");
    let w = world(g, |f| f);
    t.app.set_discord(w.rest.discord(&[w.guild_json()]));
    let a = lite(
        "embed-a001",
        "MEMBER_JOINED",
        json!([]),
        json!([{ "type": "SEND_EMBED", "embedName": "Rules Embed", "channelId": sid(40) }, { "type": "SEND_EMBED", "embedName": "missing-embed", "channelId": sid(40) }]),
    );
    let base = w.base("MEMBER_JOINED", &sid(42), None, None);
    svc::execute_automation(&t, &a, &base).await;
    let posts = w.posts(&sid(40));
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["content"], format!("Hello <@{}>", sid(42)));
    assert_eq!(posts[0]["embeds"][0]["title"], format!("Rules for user{}", sid(42)));
    assert_eq!(posts[0]["embeds"][0]["description"], format!("Be nice in <#{}>", sid(40)), "{{channel}} is the target for a trigger with no channel");
    assert_eq!(posts[0]["allowed_mentions"]["parse"], json!(["users"]));
    assert_eq!(svc::search_saved_embed_names(&t.pool, g, "rules").await, vec!["rules-embed"]);
    // The problem list notices an embed that disappeared.
    let mut row = svc::create_automation(&t, g, &input("embedder", json!({ "trigger": "MEMBER_JOINED", "actions": [{ "type": "SEND_EMBED", "embedName": "missing-embed", "channelId": sid(40) }] })), "c", None)
        .await
        .expect("row");
    row.enabled = true;
    let problems = svc::automation_problems(&t, Some(&w.guild()), &row, None).await;
    assert!(problems.iter().any(|p| p.contains("saved embed \"missing-embed\" was not found")), "{problems:?}");
    t.reset_guilds(&[g]).await;
}

// ── /automation through the router (TEST_DATABASE_URL) ───────────────────────

use bot_core::command::option_type as ot;

const ADMIN_ROLE: &str = "955028000000000902";

/// The database app with a fake Discord (the world's guild and REST) and the registry installed.
async fn cmd_world(g: &str, edit: impl FnOnce(FakeGuild) -> FakeGuild) -> Option<(DbTestApp, World)> {
    let t = db_app(&[g]).await?;
    let w = world(g, |f| edit(f.role(ADMIN_ROLE, 60, Permissions::MANAGE_GUILD).member(&sid(500), &[ADMIN_ROLE])));
    t.app.set_discord(w.rest.discord(&[w.guild_json()]));
    t.install(registry());
    Some((t, w))
}

struct Reply {
    title: String,
    description: String,
    body: Value,
}

/// Runs `/automation` as the admin (500) and returns the last answer.
async fn run_cmd(t: &DbTestApp, w: &World, user: &str, roles: &[&str], path: &[&str], opts: &[(&str, u8, Value)]) -> Reply {
    let mut f = FakeInteraction::slash("automation").guild(&w.g).user(user).roles(roles).channel(&sid(40));
    f = match path {
        [sub] => f.subcommand(sub),
        [group, sub] => f.group(group, sub),
        _ => f,
    };
    for (name, kind, value) in opts {
        f = f.option(name, *kind, value.clone());
    }
    let before = t.responses.calls().len();
    bot_core::router::route_interaction(t, f.resolve_from(&w.fake).build()).await;
    let calls = t.responses.calls();
    let last = calls.get(before..).and_then(|c| c.last()).cloned().map(|c| c.body).unwrap_or(Value::Null);
    let embed = &last["embeds"][0];
    Reply {
        title: embed["title"].as_str().unwrap_or("").to_string(),
        description: embed["description"].as_str().unwrap_or("").to_string(),
        body: last.clone(),
    }
}

async fn admin(t: &DbTestApp, w: &World, path: &[&str], opts: &[(&str, u8, Value)]) -> Reply {
    run_cmd(t, w, &sid(500), &[ADMIN_ROLE], path, opts).await
}

fn st(v: &str) -> Value {
    json!(v)
}

#[tokio::test]
async fn db_automation_walks_through_create_blank_condition_action_enable_info_export_and_edit() {
    let g = "955028300000000001";
    let Some((t, w)) = cmd_world(g, |f| {
        f.channel(&sid(510), 0, "rules", &[])
            .role(&sid(511), 1, Permissions::empty())
            .role(&sid(512), 2, Permissions::empty())
            .role_with(&sid(514), 3, Permissions::empty(), |r| r["mentionable"] = json!(true))
            .channel(&sid(530), 0, "quiet", &[])
    })
    .await
    else {
        return;
    };
    t.app.settings().ensure_guild_settings(g).await.expect("settings");
    t.app.settings().update_settings(g, SettingsPatch::new().set("automationsEnabled", false)).await.expect("module off");

    let r = admin(&t, &w, &["create-blank"], &[("name", ot::STRING, st("Rules Panel")), ("trigger", ot::STRING, st("BUTTON_CLICKED"))]).await;
    assert!(r.title.contains("Automation Created"), "{}", r.title);
    assert!(r.description.contains("created **off**"));
    assert!(r.description.contains("module is off"), "automationsEnabled is false");
    assert_eq!(t.responses.calls()[0].method, "deferReply");
    assert!(!t.responses.calls()[0].ephemeral(), "the admin convention: public");
    let r = admin(&t, &w, &["create-blank"], &[("name", ot::STRING, st("rules-panel")), ("trigger", ot::STRING, st("BUTTON_CLICKED"))]).await;
    assert!(r.title.contains("Name Taken"), "{}", r.title);
    let r = admin(&t, &w, &["create-blank"], &[("name", ot::STRING, st("🎉")), ("trigger", ot::STRING, st("BUTTON_CLICKED"))]).await;
    assert!(r.title.contains("Invalid Name"));
    assert!(admin(&t, &w, &["info"], &[("name", ot::STRING, st("nope"))]).await.title.contains("Automation Not Found"));
    assert!(admin(&t, &w, &["enable"], &[("name", ot::STRING, st("rules-panel"))]).await.title.contains("No Actions"));
    let add = |opts: Vec<(&'static str, u8, Value)>| {
        let (t, w) = (&t, &w);
        async move { admin(t, w, &["action", "add"], &opts).await }
    };
    assert!(add(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("Frobnicate"))]).await.title.contains("Unknown Type"));
    let r = add(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("GIVE_ROLES")), ("role", ot::ROLE, json!(sid(511)))]).await;
    assert!(r.title.contains("Action Added"), "{} {}", r.title, r.description);
    assert!(r.description.contains("This automation is **off**"));
    assert!(add(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("REACT")), ("emoji", ot::STRING, st("👍"))]).await.title.contains("Not Available"));
    let r = add(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("GIVE_XP")), ("amount", ot::INTEGER, json!(5))]).await;
    assert!(r.title.contains("Not Available"));
    assert!(r.description.contains("XP rewards need the Levels module"), "{}", r.description);
    let cond = |opts: Vec<(&'static str, u8, Value)>| {
        let (t, w) = (&t, &w);
        async move { admin(t, w, &["condition", "add"], &opts).await }
    };
    assert!(cond(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("CONTAINS")), ("text", ot::STRING, st("x"))]).await.title.contains("Not Available"));

    // A second automation posts the button.
    admin(&t, &w, &["create-blank"], &[("name", ot::STRING, st("panel")), ("trigger", ot::STRING, st("MEMBER_JOINED"))]).await;
    let r = add(vec![
        ("name", ot::STRING, st("panel")),
        ("type", ot::STRING, st("SEND_MESSAGE")),
        ("channel", ot::CHANNEL, json!(sid(510))),
        ("text", ot::STRING, st("Accept the rules, {user}")),
        ("button_label", ot::STRING, st("Accept")),
        ("emoji", ot::STRING, st("⭐")),
    ])
    .await;
    assert!(r.title.contains("Action Added"), "{} {}", r.title, r.description);
    assert!(r.description.contains("Its button number is **1**"));
    assert!(r.description.contains("Ignored for this type: `emoji`"));
    let panel = svc::get_automation_by_name(&t.pool, g, "panel").await.expect("read").expect("panel");
    let r = cond(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("BUTTON_IS")), ("button", ot::STRING, json!(format!("{}:1", panel.id)))]).await;
    assert!(r.title.contains("Condition Added"), "{} {}", r.title, r.description);
    assert!(cond(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("HAS_NOT_ANY_ROLE")), ("role", ot::ROLE, json!(sid(511)))]).await.title.contains("Condition Added"));
    let r = cond(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("has not any role")), ("text", ot::STRING, json!(format!("<@&{}>", sid(513))))]).await;
    assert!(r.title.contains("Invalid Value"));
    let r = cond(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("HAS_NOT_ANY_ROLE")), ("text", ot::STRING, json!(format!("<@&{}>", sid(512))))]).await;
    assert!(r.title.contains("Condition Updated"));
    assert!(r.description.contains("Condition 2 of **rules-panel** is now"));
    let r = cond(vec![("name", ot::STRING, st("rules-panel")), ("type", ot::STRING, st("HAS_NOT_ANY_ROLE")), ("text", ot::STRING, json!(format!("<@&{}>", sid(512))))]).await;
    assert!(r.title.contains("Already There"));

    assert!(admin(&t, &w, &["enable"], &[("name", ot::STRING, st("rules-panel"))]).await.title.contains("Automation Enabled"));
    assert!(admin(&t, &w, &["enable"], &[("name", ot::STRING, st("rules-panel"))]).await.title.contains("No Change"));
    let r = admin(&t, &w, &["info"], &[("name", ot::STRING, st("rules-panel"))]).await;
    assert!(r.description.contains("🟢 On"));
    let fields = arr(&r.body["embeds"][0]["fields"]);
    assert_eq!(fields.iter().map(|f| f["name"].clone()).collect::<Vec<_>>(), vec![json!("If (2)"), json!("Do (1)")]);
    assert!(fields[0]["value"].as_str().unwrap_or("").contains("1. Clicked this button"));
    let r = admin(&t, &w, &["export"], &[("name", ot::STRING, st("rules-panel"))]).await;
    assert!(r.title.contains("Exported"));
    assert_eq!(r.body["attachments"][0]["filename"], "automation-rules-panel.json");

    assert!(admin(&t, &w, &["edit"], &[("name", ot::STRING, st("rules-panel"))]).await.title.contains("Nothing To Change"));
    let r = admin(&t, &w, &["edit"], &[("name", ot::STRING, st("rules-panel")), ("cooldown", ot::INTEGER, json!(30))]).await;
    assert!(r.description.contains("Cooldown:** 0 s → 30 s"), "{}", r.description);
    assert!(admin(&t, &w, &["edit"], &[("name", ot::STRING, st("rules-panel")), ("rename", ot::STRING, st("panel"))]).await.title.contains("Name Taken"));
    let r = admin(&t, &w, &["edit"], &[("name", ot::STRING, st("rules-panel")), ("rename", ot::STRING, st("Rules Gate"))]).await;
    assert!(r.description.contains("rules-panel → rules-gate"), "{}", r.description);

    let r = admin(&t, &w, &["condition", "remove"], &[("name", ot::STRING, st("rules-gate")), ("index", ot::INTEGER, json!(5))]).await;
    assert!(r.title.contains("No Such Condition"));
    assert!(r.description.contains("has 2 condition(s)"));
    assert!(admin(&t, &w, &["condition", "remove"], &[("name", ot::STRING, st("rules-gate")), ("index", ot::INTEGER, json!(2))]).await.title.contains("Condition Removed"));
    let r = admin(&t, &w, &["action", "remove"], &[("name", ot::STRING, st("rules-gate")), ("index", ot::INTEGER, json!(1))]).await;
    assert!(r.title.contains("Action Removed"));
    assert!(r.description.contains("turned **off**"));
    assert!(admin(&t, &w, &["disable"], &[("name", ot::STRING, st("rules-gate"))]).await.title.contains("No Change"));

    // The button number survives removal.
    admin(&t, &w, &["action", "remove"], &[("name", ot::STRING, st("panel")), ("index", ot::INTEGER, json!(1))]).await;
    let r = add(vec![
        ("name", ot::STRING, st("panel")),
        ("type", ot::STRING, st("SEND_MESSAGE")),
        ("channel", ot::CHANNEL, json!(sid(510))),
        ("text", ot::STRING, st("Again")),
        ("button_label", ot::STRING, st("Accept")),
    ])
    .await;
    assert!(r.description.contains("Its button number is **2**"), "{}", r.description);

    // A typed role mention the admin could not ping is refused at configuration time.
    let r = add(vec![("name", ot::STRING, st("panel")), ("type", ot::STRING, st("SEND_MESSAGE")), ("channel", ot::CHANNEL, json!(sid(530))), ("text", ot::STRING, json!(format!("Hey <@&{}>", sid(511))))]).await;
    assert!(r.title.contains("Role Not Pingable"), "{}", r.title);
    assert!(r.description.contains("not mentionable and you lack"));
    let r = add(vec![("name", ot::STRING, st("panel")), ("type", ot::STRING, st("SEND_MESSAGE")), ("channel", ot::CHANNEL, json!(sid(530))), ("text", ot::STRING, json!(format!("Hey <@&{}>", sid(514))))]).await;
    assert!(r.title.contains("Action Added"), "a mentionable role is fine");

    let r = admin(&t, &w, &["list"], &[]).await;
    assert!(r.title.contains("Automations (2/50)"), "{}", r.title);
    assert!(r.description.contains("`panel` · Member joined · 0 if · 2 do · 0 runs"), "{}", r.description);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_automation_delete_asks_privately_then_confirms() {
    let g = "955028300000000002";
    let Some((t, _w)) = cmd_world(g, |f| f).await else { return };
    svc::create_automation(&t, g, &input("gone", json!({})), "c", None).await.expect("create");
    let task = {
        let (app, interaction) = (t.app.clone(), FakeInteraction::slash("automation").guild(g).user(&sid(500)).roles(&[ADMIN_ROLE]).channel(&sid(40)).subcommand("delete").option("name", ot::STRING, st("gone")).build());
        tokio::spawn(async move { bot_core::router::route_interaction(&app, interaction).await })
    };
    for _ in 0..200 {
        if !t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let calls = t.responses.calls();
    assert!(calls[0].ephemeral(), "the confirm dialog is private");
    let prompt = calls.iter().rev().find(|c| c.body.get("components").is_some()).expect("prompt");
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().expect("yes").to_string();
    assert!(prompt.body["embeds"][0]["description"].as_str().unwrap_or("").contains("Delete **gone**?"));
    bot_core::router::route_interaction(&t, FakeInteraction::button(&yes).guild(g).user(&sid(500)).message_id("1000000000000000100").build()).await;
    task.await.expect("route");
    let last = t.responses.calls().last().cloned().expect("answer");
    assert_eq!(last.method, "followUp");
    assert!(last.embed_title().unwrap_or("").contains("Automation Deleted"));
    assert!(!last.ephemeral());
    assert!(svc::get_automation_by_name(&t.pool, g, "gone").await.expect("read").is_none());
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_automation_create_presets_validate_their_inputs_and_autocomplete_answers() {
    let g = "955028300000000003";
    let Some((t, w)) = cmd_world(g, |f| {
        f.channel(&sid(520), 0, "welcome", &[])
            .channel(&sid(521), 15, "forum", &[])
            .role(&sid(522), 1, Permissions::empty())
            .role(&sid(523), 300, Permissions::empty())
            .member(&sid(524), &[])
            .channel(&sid(525), 0, "locked", &[(BOT_ROLE, false, Permissions::empty(), Permissions::MANAGE_MESSAGES)])
    })
    .await
    else {
        return;
    };
    let create = |opts: Vec<(&'static str, u8, Value)>| {
        let (t, w) = (&t, &w);
        async move { admin(t, w, &["create"], &opts).await }
    };
    let p = |name: &'static str| ("preset", ot::STRING, st(name));
    let n = |name: &'static str| ("name", ot::STRING, st(name));
    assert!(create(vec![p("welcomer"), n("welcome")]).await.title.contains("Channel Needed"));
    assert!(create(vec![p("welcomer"), n("welcome"), ("channel", ot::CHANNEL, json!(sid(521)))]).await.title.contains("Wrong Channel Type"));
    assert!(create(vec![p("welcomer"), n("welcome"), ("channel", ot::CHANNEL, json!(sid(520))), ("role", ot::ROLE, json!(sid(523)))]).await.title.contains("Role Not Allowed"));
    let r = create(vec![
        p("welcomer"),
        n("welcome"),
        ("channel", ot::CHANNEL, json!(sid(520))),
        ("role", ot::ROLE, json!(sid(522))),
        ("message", ot::STRING, st("Hey {user}")),
        ("threshold", ot::INTEGER, json!(4)),
    ])
    .await;
    assert!(r.title.contains("Automation Created"), "{} {}", r.title, r.description);
    assert!(r.description.contains("Ignored for this type: `threshold`"));
    assert!(r.description.contains("**If:** always"));
    assert!(create(vec![p("starboard"), n("stars"), ("channel", ot::CHANNEL, json!(sid(520))), ("emoji", ot::STRING, st("banana")), ("threshold", ot::INTEGER, json!(2))]).await.title.contains("Invalid Emoji"));
    assert!(create(vec![p("starboard"), n("stars"), ("channel", ot::CHANNEL, json!(sid(520))), ("threshold", ot::INTEGER, json!(2))]).await.title.contains("Automation Created"));
    assert!(create(vec![p("thread-joiner"), n("joiner")]).await.title.contains("Member Needed"));
    assert!(create(vec![p("thread-joiner"), n("joiner"), ("user", ot::USER, json!(sid(777)))]).await.title.contains("Not A Member"));
    assert!(create(vec![p("thread-joiner"), n("joiner"), ("user", ot::USER, json!(TEST_BOT_ID))]).await.description.contains("Pick a person, not a bot"));
    assert!(create(vec![p("thread-joiner"), n("joiner"), ("user", ot::USER, json!(sid(524)))]).await.title.contains("Automation Created"));
    assert!(create(vec![p("commands-only"), n("clean")]).await.title.contains("Channel Needed"));
    assert!(create(vec![p("commands-only"), n("clean"), ("channel", ot::CHANNEL, json!(sid(521)))]).await.title.contains("Wrong Channel Type"));
    assert!(create(vec![p("commands-only"), n("clean"), ("channel", ot::CHANNEL, json!(sid(525)))]).await.description.contains("Manage Messages"));
    let r = create(vec![p("commands-only"), n("clean"), ("channel", ot::CHANNEL, json!(sid(520))), ("role", ot::ROLE, json!(sid(523)))]).await;
    assert!(r.title.contains("Automation Created"), "a staff role above the bot is fine: nothing is given");
    assert!(r.description.contains("do not start with `!` are deleted"), "{}", r.description);
    let clean = svc::get_automation_by_name(&t.pool, g, "clean").await.expect("read").expect("clean");
    assert_eq!(clean.conditions.iter().map(|c| c["type"].clone()).collect::<Vec<_>>(), vec![json!("CHANNEL_IN"), json!("MATCHES_REGEX"), json!("HAS_NOT_ANY_ROLE")]);
    assert_eq!(clean.actions, vec![json!({ "type": "DELETE" })]);

    let complete = |path: &[&str], opts: Vec<(&str, u8, Value)>, focused: (&str, &str)| {
        let mut f = FakeInteraction::autocomplete("automation").guild(g).user(&sid(500)).roles(&[ADMIN_ROLE]);
        f = match path {
            [sub] => f.subcommand(sub),
            [group, sub] => f.group(group, sub),
            _ => f,
        };
        for (name, kind, value) in opts {
            f = f.option(name, kind, value);
        }
        f.focused(focused.0, focused.1).build()
    };
    let choices = |t: &DbTestApp| t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    bot_core::router::route_interaction(&t, complete(&["info"], vec![], ("name", "sta"))).await;
    let names = choices(&t);
    assert_eq!(names.iter().map(|c| c["value"].clone()).collect::<Vec<_>>(), vec![json!("stars")]);
    assert!(names[0]["name"].as_str().unwrap_or("").contains("Reaction added"));
    bot_core::router::route_interaction(&t, complete(&["condition", "add"], vec![("name", ot::STRING, st("stars"))], ("type", "role"))).await;
    let types = choices(&t);
    assert!(!types.is_empty());
    assert!(types.iter().all(|c| c["value"].as_str().unwrap_or("").contains("ROLE") || c["name"].as_str().unwrap_or("").to_lowercase().contains("role")));
    assert!(!types.iter().any(|c| c["value"] == "CONTAINS"));
    bot_core::router::route_interaction(&t, complete(&["action", "add"], vec![("name", ot::STRING, st("stars"))], ("type", ""))).await;
    let acts = choices(&t);
    assert!(acts.iter().any(|c| c["value"] == "DELETE"));
    assert_eq!(acts.iter().find(|c| c["value"] == "GIVE_XP").map(|c| c["name"].clone()), Some(json!("Give XP (module off)")));
    assert!(acts.iter().any(|c| c["value"] == "SEND_IN_THREAD"), "REACTION_ADDED is a thread-capable trigger");
    bot_core::router::route_interaction(&t, complete(&["condition", "remove"], vec![("name", ot::STRING, st("stars"))], ("index", ""))).await;
    assert_eq!(choices(&t).iter().map(|c| c["value"].clone()).collect::<Vec<_>>(), vec![json!(1), json!(2), json!(3)]);
    bot_core::router::route_interaction(&t, complete(&["action", "remove"], vec![("name", ot::STRING, st("ghost"))], ("index", ""))).await;
    assert!(choices(&t).is_empty());
    bot_core::router::route_interaction(&t, complete(&["action", "add"], vec![], ("embed", "x"))).await;
    assert_eq!(t.responses.calls().last().map(|c| c.method), Some("respond"));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_a_lower_admin_cannot_change_an_automation_that_hands_out_a_role_above_their_own() {
    let g = "955028300000000004";
    let lower_role = "955028000000000903";
    let Some((t, w)) = cmd_world(g, |f| {
        f.role(&sid(450), 2, Permissions::empty())
            .role(&sid(451), 8, Permissions::empty())
            .role(lower_role, 5, Permissions::MANAGE_GUILD)
            .member(&sid(460), &[lower_role])
            .channel(&sid(454), 0, "secret", &[(&sid(460), true, Permissions::empty(), Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)])
    })
    .await
    else {
        return;
    };
    svc::create_automation(&t, g, &input("gate", json!({ "actions": [{ "type": "GIVE_ROLES", "roleIds": [sid(451)] }], "enabled": false })), "c", None).await.expect("gate");
    svc::create_automation(&t, g, &input("low", json!({ "actions": [{ "type": "GIVE_ROLES", "roleIds": [sid(450)] }], "enabled": false })), "c", None).await.expect("low");
    let before = svc::get_automation_by_name(&t.pool, g, "gate").await.expect("read").expect("gate");
    type Spec<'a> = (Vec<&'a str>, Vec<(&'a str, u8, Value)>);
    let specs: Vec<Spec> = vec![
        (vec!["condition", "add"], vec![("name", ot::STRING, st("gate")), ("type", ot::STRING, st("CONTAINS")), ("text", ot::STRING, st("x"))]),
        (vec!["enable"], vec![("name", ot::STRING, st("gate"))]),
        (vec!["disable"], vec![("name", ot::STRING, st("gate"))]),
        (vec!["edit"], vec![("name", ot::STRING, st("gate")), ("cooldown", ot::INTEGER, json!(30))]),
        (vec!["condition", "remove"], vec![("name", ot::STRING, st("gate")), ("index", ot::INTEGER, json!(1))]),
        (vec!["action", "add"], vec![("name", ot::STRING, st("gate")), ("type", ot::STRING, st("SEND_MESSAGE")), ("text", ot::STRING, st("hi"))]),
        (vec!["action", "remove"], vec![("name", ot::STRING, st("gate")), ("index", ot::INTEGER, json!(1))]),
    ];
    for (path, opts) in specs {
        let r = run_cmd(&t, &w, &sid(460), &[lower_role], &path, &opts).await;
        assert!(r.title.contains("Role Not Allowed"), "{path:?}: {} {}", r.title, r.description);
        assert!(r.description.contains("at or above your highest role"));
    }
    let after = svc::get_automation_by_name(&t.pool, g, "gate").await.expect("read").expect("gate");
    assert_eq!(after, before, "nothing was written");
    // Roles below the admin's own are theirs to change; the owner (and a higher admin) may change anything.
    let r = run_cmd(&t, &w, &sid(460), &[lower_role], &["edit"], &[("name", ot::STRING, st("low")), ("cooldown", ot::INTEGER, json!(30))]).await;
    assert!(r.title.contains("Automation Updated"), "{} {}", r.title, r.description);
    let r = run_cmd(&t, &w, &sid(500), &[ADMIN_ROLE], &["enable"], &[("name", ot::STRING, st("gate"))]).await;
    assert!(r.title.contains("Automation Enabled"), "{} {}", r.title, r.description);
    // The admin also needs View Channel and Send Messages where the action posts.
    let r = run_cmd(&t, &w, &sid(460), &[lower_role], &["action", "add"], &[("name", ot::STRING, st("low")), ("type", ot::STRING, st("SEND_MESSAGE")), ("channel", ot::CHANNEL, json!(sid(454))), ("text", ot::STRING, st("hi"))]).await;
    assert!(r.title.contains("No Access"), "{} {}", r.title, r.description);
    assert!(r.description.starts_with("Action 2: You need"), "{}", r.description);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_import_checks_access_pings_and_the_replaced_row_before_saving() {
    let g = "955028300000000005";
    let lower_role = "955028000000000904";
    let Some((t, w)) = cmd_world(g, |f| {
        f.role(&sid(450), 2, Permissions::empty())
            .role(&sid(451), 8, Permissions::empty())
            .role(&sid(456), 1, Permissions::empty())
            .role(lower_role, 5, Permissions::MANAGE_GUILD)
            .member(&sid(460), &[lower_role])
            .channel(&sid(454), 0, "secret", &[(&sid(460), true, Permissions::empty(), Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)])
    })
    .await
    else {
        return;
    };
    let guild = w.guild();
    let lower = w.member(&sid(460));
    let top = w.member(&sid(500));
    let general = w.channel(&sid(40));
    let import = |member: &serenity::all::Member, text: String, name: Option<&str>, overwrite: bool| {
        let (app, guild, member, general, name) = (t.app.clone(), guild.clone(), member.clone(), general.clone(), name.map(str::to_string));
        async move {
            let req = feat_automations::commands::automation::ImportRequest {
                guild: &guild,
                member: Some(&member),
                fallback_channel: Some(&general),
                settings: None,
                module_off: false,
                user_id: "importer",
                name,
                overwrite,
            };
            let mut out = None;
            let embed = feat_automations::commands::automation::import_from_text(&app, req, &text, &mut out).await.expect("import");
            serde_json::to_value(embed).expect("embed")
        }
    };
    let file = |actions: Value| json!({ "format": "automation", "version": 1, "name": "imported", "trigger": "MESSAGE_SENT", "actions": actions }).to_string();
    let r = import(&lower, file(json!([{ "type": "GIVE_ROLES", "roleIds": [sid(451)] }])), None, false).await;
    assert_eq!(r["title"].as_str().map(|s| s.contains("Role Not Allowed")), Some(true), "{r}");
    let r = import(&lower, file(json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(454) }])), None, false).await;
    assert!(r["title"].as_str().unwrap_or("").contains("No Access"), "{r}");
    let r = import(&lower, file(json!([{ "type": "REPLY", "content": format!("<@&{}>", sid(456)) }])), None, false).await;
    assert!(r["title"].as_str().unwrap_or("").contains("Role Not Pingable"), "{r}");
    assert!(svc::get_automation_by_name(&t.pool, g, "imported").await.expect("read").is_none(), "nothing was saved");
    let r = import(&lower, "{".into(), None, false).await;
    assert!(r["title"].as_str().unwrap_or("").contains("Invalid JSON"));
    assert_eq!(r["description"], "• That is not valid JSON.");

    // A good import, then the overwrite rules.
    let r = import(&top, file(json!([{ "type": "PIN" }])), None, false).await;
    assert!(r["title"].as_str().unwrap_or("").contains("Automation Imported"), "{r}");
    let r = import(&top, file(json!([{ "type": "PIN" }])), None, false).await;
    assert!(r["title"].as_str().unwrap_or("").contains("Name Taken"));
    assert!(r["description"].as_str().unwrap_or("").contains("Add overwrite:True to replace it."));
    let r = import(&top, file(json!([{ "type": "GIVE_ROLES", "roleIds": [sid(451)] }])), None, true).await;
    assert!(r["title"].as_str().unwrap_or("").contains("Automation Replaced"), "{r}");
    let r = import(&lower, file(json!([{ "type": "PIN" }])), None, true).await;
    assert!(r["title"].as_str().unwrap_or("").contains("Role Not Allowed"), "{r}");
    assert!(r["description"].as_str().unwrap_or("").contains("The automation you would replace gives or takes a role"));
    // Problems save it off.
    let r = import(&top, file(json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(999) }])), Some("Second One"), false).await;
    assert!(r["description"].as_str().unwrap_or("").contains("It was saved **off** because:"), "{r}");
    assert!(!svc::get_automation_by_name(&t.pool, g, "second-one").await.expect("read").expect("row").enabled);

    // Through the router: the size and type checks, and a download that is refused.
    let attach = |name: &str, size: u64, url: &str| {
        let mut f = FakeInteraction::slash("automation").guild(g).user(&sid(500)).roles(&[ADMIN_ROLE]).channel(&sid(40)).subcommand("import").option("file", ot::ATTACHMENT, json!("1"));
        f.json_mut()["data"]["resolved"] = json!({ "attachments": { "1": { "id": "1", "filename": name, "size": size, "url": url, "proxy_url": url, "content_type": "application/json" } } });
        f.build()
    };
    bot_core::router::route_interaction(&t, attach("a.json", 40_000, "https://cdn.test/a.json")).await;
    assert!(t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("File Too Large"));
    bot_core::router::route_interaction(&t, attach("a.json", 100, "http://127.0.0.1:1/a.json")).await;
    assert!(t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("Download Failed"));
    t.reset_guilds(&[g]).await;
}

// ── Cross-language: the JS service and the Rust port read each other's data ──

const JS: &str = "rust/fixtures/gen/feat-automations-js.mjs";

async fn redis_app(guilds: &[&str]) -> Option<DbTestApp> {
    let url = database_url()?;
    let redis = redis_url()?;
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds(guilds.iter().map(|g| g.to_string()));
    t.reset_guilds(guilds).await;
    for g in guilds {
        t.app.cache().del_one(&svc::automations_cache_key(g)).await;
    }
    Some(t)
}

fn js(args: &[&str]) -> Value {
    let out = run_node(JS, args);
    assert!(out.get("error").is_none(), "JS failed: {out}");
    out
}

/// The columns both sides return, compared as JSON.
fn row_view(v: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["id", "guildId", "name", "trigger", "conditions", "actions", "enabled", "cooldownSeconds", "buttonSeq", "uses", "createdBy", "createdAt", "updatedAt"] {
        out.insert(k.into(), v[k].clone());
    }
    Value::Object(out)
}

#[tokio::test]
async fn cross_js_writes_and_rust_reads_rows_cache_cooldowns_and_problems() {
    let g = "955028400000000001";
    let Some(t) = redis_app(&[g]).await else { return };
    let panel = js(&[
        "create",
        g,
        &json!({ "name": "JS Panel", "trigger": "MEMBER_JOINED", "enabled": true, "cooldownSeconds": 0, "actions": [
            { "type": "SEND_MESSAGE", "content": "Accept {user}", "channelId": sid(40), "button": { "label": "Accept", "url": null, "style": "SUCCESS", "index": 3 } },
            { "type": "GIVE_COINS", "amount": 5 }
        ] })
        .to_string(),
        "js-creator",
    ]);
    js(&[
        "create",
        g,
        &json!({ "name": "js-gate", "trigger": "BUTTON_CLICKED", "enabled": true, "cooldownSeconds": 60,
                 "conditions": [{ "type": "BUTTON_IS", "buttons": [{ "automationId": panel["id"], "index": 3 }] }],
                 "actions": [{ "type": "REPLY", "content": "Thanks", "button": null }] })
        .to_string(),
        "js-creator",
    ]);
    let gate = js(&["mutate", g, "js-gate", &json!({ "cooldownSeconds": 90 }).to_string()]);
    for (name, js_row) in [("js-panel", &panel), ("js-gate", &gate)] {
        let row = svc::get_automation_by_name(&t.pool, g, name).await.expect("read").expect("row");
        assert_eq!(row_view(&row.to_json()), row_view(js_row), "{name}");
    }
    let buttons = serde_json::to_value(svc::list_automation_buttons(&t.pool, g).await).expect("buttons");
    assert_eq!(buttons, js(&["buttons", g]));
    let search = serde_json::to_value(svc::search_automations(&t.pool, g, "js").await).expect("search");
    assert_eq!(search, js(&["search", g, "js"]));
    let names: HashMap<String, String> = svc::list_automation_refs(&t.pool, g).await.into_iter().map(|r| (r.id, r.name)).collect();
    let gate_row = svc::get_automation_by_name(&t.pool, g, "js-gate").await.expect("read").expect("row");
    assert_eq!(json!(svc::to_export_json(&gate_row.to_json(), &names)), js(&["export", g, "js-gate"]), "the export file, byte for byte");

    // The JS cache of enabled automations is read by Rust (the rows are gone from Postgres: only Redis has them).
    let by_js = js(&["load", g]);
    assert_eq!(by_js["BUTTON_CLICKED"][0]["name"], "js-gate");
    sqlx::query(r#"DELETE FROM "Automation" WHERE "guildId" = $1"#).bind(g).execute(&t.pool).await.expect("delete");
    let by_rs = svc::get_automations(&t, g).await.expect("loads");
    assert_eq!(serde_json::to_value(&*by_rs).expect("json"), by_js, "a Redis hit, JSON-equal");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn cross_js_cooldowns_and_problems_reach_rust() {
    let g = "955028400000000002";
    let Some(t) = redis_app(&[g]).await else { return };
    let cool = js(&[
        "create",
        g,
        &json!({ "name": "js-cool", "trigger": "MESSAGE_SENT", "enabled": true, "cooldownSeconds": 600,
                 "actions": [{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(40) }] })
        .to_string(),
        "c",
    ]);
    assert_eq!(js(&["fire", g, &sid(42), &sid(40)])["sent"], json!(["x"]));
    let key = svc::cooldown_key(g, cool["id"].as_str().unwrap_or(""), &sid(42));
    let raw = js(&["raw", &key]);
    assert_eq!(raw["value"], "\"1\"");
    let w = world(g, |f| f);
    t.app.set_discord(w.rest.discord(&[w.guild_json()]));
    let msg = message(4001, g, &sid(40), &sid(42), "hello", |_| {});
    svc::dispatch(&t, "MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg)), true).await;
    assert!(w.posts(&sid(40)).is_empty(), "the JS cooldown holds in Rust");

    js(&[
        "create",
        g,
        &json!({ "name": "js-broken", "trigger": "MEMBER_JOINED", "enabled": true, "actions": [{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(999) }] }).to_string(),
        "c",
    ]);
    let executed = js(&["execute", g, "js-broken"]);
    assert_eq!(executed["problem"]["text"], "Action 1 (Send a message): The channel was deleted");
    let row = svc::get_automation_by_name(&t.pool, g, "js-broken").await.expect("read").expect("row");
    let problems = svc::automation_problems(&t, Some(&w.guild()), &row, None).await;
    let last = problems.iter().find(|p| p.starts_with("Last problem <t:")).cloned().unwrap_or_default();
    assert!(last.ends_with("R>: Action 1 (Send a message): The channel was deleted"), "{problems:?}");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn cross_rust_writes_and_js_reads_rows_cache_cooldowns_and_problems() {
    let g = "955028400000000003";
    let Some(t) = redis_app(&[g]).await else { return };
    let panel = svc::create_automation(
        &t,
        g,
        &input(
            "RS Panel",
            json!({ "trigger": "MEMBER_JOINED", "actions": [
                { "type": "SEND_MESSAGE", "content": "Accept {user}", "channelId": sid(40), "button": { "label": "Accept", "url": null, "style": "DANGER", "index": 4 } },
                { "type": "CREATE_THREAD", "name": "T", "channelId": sid(40), "autoArchiveMinutes": 60 },
                { "type": "SEND_IN_THREAD", "content": "hi" }
            ] }),
        ),
        "rs-creator",
        None,
    )
    .await
    .expect("panel");
    let exported = "clxexported9";
    let gate = svc::create_automation(
        &t,
        g,
        &input(
            "rs-gate",
            json!({ "trigger": "BUTTON_CLICKED", "cooldownSeconds": 60, "conditions": [
                { "type": "BUTTON_IS", "buttons": [{ "automationId": panel.id, "index": 4 }, { "automationId": exported, "index": 1 }] },
                { "type": "HAS_ANY_ROLE", "roleIds": [sid(7)] }
            ], "actions": [{ "type": "REPLY", "content": "Thanks", "button": { "label": "More", "url": null, "index": 1 } }] }),
        ),
        "rs-creator",
        Some(exported),
    )
    .await
    .expect("gate");
    assert_eq!(gate.conditions[0]["buttons"][1]["automationId"], json!(gate.id), "the self reference was rewritten");
    let (gate, _) = svc::mutate_automation(&t, g, "rs-gate", |_| Ok(svc::Mutation::new(vec![("cooldownSeconds", json!(90))], ()))).await.expect("mutate");
    for (name, row) in [("rs-panel", &panel), ("rs-gate", &gate)] {
        let from_js = js(&["get", g, name]);
        assert_eq!(row_view(&from_js), row_view(&row.to_json()), "{name}");
        assert_eq!(from_js["invalidActions"], json!([]));
    }
    let listed = js(&["list", g]);
    let rs_list = svc::list_automations(&t.pool, g).await.expect("list");
    assert_eq!(arr(&listed).len(), rs_list.len());
    for (j, r) in arr(&listed).iter().zip(rs_list.iter()) {
        assert_eq!((j["name"].clone(), j["trigger"].clone(), j["enabled"].clone(), j["uses"].clone()), (json!(r.name), json!(r.trigger), json!(r.enabled), json!(r.uses)));
        assert_eq!((j["conditions"].clone(), j["actions"].clone()), (json!(r.conditions), json!(r.actions)));
    }
    let names: HashMap<String, String> = svc::list_automation_refs(&t.pool, g).await.into_iter().map(|r| (r.id, r.name)).collect();
    assert_eq!(js(&["export", g, "rs-gate"]), json!(svc::to_export_json(&gate.to_json(), &names)));

    // The Rust cache of enabled automations is what JS writes and reads.
    let by_rs = svc::get_automations(&t, g).await.expect("loads");
    let raw = js(&["raw", &svc::automations_cache_key(g)]);
    let value: Value = serde_json::from_str(raw["value"].as_str().unwrap_or("null")).expect("cached JSON");
    assert_eq!(s(&value), s(&json!({ "v": 1, "byTrigger": &*by_rs })), "key order included");
    let ttl = raw["ttl"].as_i64().unwrap_or(0);
    assert!((295..=300).contains(&ttl), "{ttl}");
    assert_eq!(js(&["load", g]), serde_json::to_value(&*by_rs).expect("json"));

    // A Rust cooldown is a JS cooldown.
    let cool = svc::create_automation(&t, g, &input("rs-cool", json!({ "cooldownSeconds": 600, "actions": [{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(40) }] })), "c", None)
        .await
        .expect("cool");
    let w = world(g, |f| f);
    t.app.set_discord(w.rest.discord(&[w.guild_json()]));
    let msg = message(4101, g, &sid(40), &sid(42), "hello", |_| {});
    svc::dispatch(&t, "MESSAGE_SENT", w.base("MESSAGE_SENT", &sid(42), Some(&sid(40)), Some(msg)), true).await;
    assert_eq!(w.posts(&sid(40)).len(), 1);
    let raw = js(&["raw", &svc::cooldown_key(g, &cool.id, &sid(42))]);
    assert_eq!(raw["value"], "\"1\"");
    assert!((595..=600).contains(&raw["ttl"].as_i64().unwrap_or(0)), "{raw}");
    assert_eq!(js(&["fire", g, &sid(42), &sid(40)])["sent"], json!([]), "the Rust cooldown holds in JS");

    // A Rust problem is shown by JS.
    let broken = lite("rsbroken01", "MEMBER_JOINED", json!([]), json!([{ "type": "SEND_MESSAGE", "content": "x", "channelId": sid(999) }]));
    svc::execute_automation(&t, &broken, &w.base("MEMBER_JOINED", &sid(42), None, None)).await;
    let raw = js(&["raw", &svc::problem_key("rsbroken01")]);
    let problem: Value = serde_json::from_str(raw["value"].as_str().unwrap_or("null")).expect("problem JSON");
    assert_eq!(problem["text"], "Action 1 (Send a message): The channel was deleted");
    assert!(bot_util::js::date_parse(problem["at"].as_str().unwrap_or("")).is_finite());
    assert!((604_790..=604_800).contains(&raw["ttl"].as_i64().unwrap_or(0)), "{raw}");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn custom_emojis_resolve_from_every_server_the_bot_is_in() {
    let g = "955028100000000022";
    let other = FakeGuild::new("955028100000000023", OWNER).field(
        "emojis",
        json!([{ "id": "123456789012345678", "name": "pog", "animated": false, "available": true, "managed": false, "require_colons": true, "roles": [] }]),
    );
    let home = FakeGuild::new(g, OWNER).everyone(everyone()).role(BOT_ROLE, 50, bot_perms()).member(TEST_BOT_ID, &[BOT_ROLE]).channel(&sid(40), 0, "general", &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(&[home, other], &rest);
    let guild = bot_core::interaction_helpers::cached_guild(&t, GuildId::new(g.parse().expect("id"))).expect("guild");
    let opts = |text: &str| svc::ConditionOptions { text: Some(text.into()), ..Default::default() };
    let r = svc::condition_params_from_options(&t, "REACTED_WITH", &opts(":pog: ⭐"), &guild).await;
    assert_eq!(r.params, json!({ "emojis": ["<:pog:123456789012345678>", "⭐"] }));
    let r = svc::condition_params_from_options(&t, "REACTED_WITH", &opts("123456789012345678"), &guild).await;
    assert_eq!(r.params, json!({ "emojis": ["<:pog:123456789012345678>"] }));
    let r = svc::condition_params_from_options(&t, "REACTED_WITH", &opts(":nope:"), &guild).await;
    assert_eq!(r.error.as_deref(), Some("I can only use custom emojis from servers I am in."));
    let ctx = svc::ActionContext { guild: &guild, member: None, settings: None, fallback_channel: None };
    let r = svc::action_params_from_options(&t, "REACT", &svc::ActionOptions { emoji: Some(":pog:".into()), ..Default::default() }, ctx).await;
    assert_eq!(r.params, json!({ "emoji": "<:pog:123456789012345678>" }));
}
