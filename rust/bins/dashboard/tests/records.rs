//! src/dashboard/recordsBridge.js ported to bins/dashboard `records_bridge`: the store registry is complete, the
//! bridge's own refusals, and the same op list run through the JS bridge (guild A) and the Rust bridge (guild B) on the
//! same database, compared answer by answer; then each side lists what the other wrote.

mod common;

use std::sync::Arc;

use common::*;
use dash_collections::testing::{FakeDiscord, MemoryTasks};
use dash_collections::{Actor, DashboardDiscord, DashboardMember, ListOpts, Mode, RecordsBridge, StoreError, TaskRunner};
use dashboard::records_bridge::{DashboardRecords, build_store_registry};
use serde_json::{Map, Value, json};

const A: &str = "960000000000000001"; // the JS bridge
const B: &str = "960000000000000002"; // the Rust bridge

fn actor() -> Actor {
    Actor { user_id: "600000000000000001".into(), user: None, member: Some(DashboardMember { owner: true, permissions: "8".into(), highest_role_position: None }) }
}

fn error_json(e: &StoreError) -> Value {
    if let Some(r) = e.as_record() {
        return json!({ "name": "RecordError", "code": r.code.as_str(), "message": r.message, "path": r.path, "saved": r.saved });
    }
    if let Some(f) = e.as_field() {
        return json!({ "name": "FieldError", "code": null, "message": f.message, "path": f.path, "saved": false });
    }
    json!({ "name": "Error", "message": e.to_string() })
}

fn item_id(v: &Value) -> Option<String> {
    v.get("item").and_then(|i| i.get("id")).or_else(|| v.get("id")).and_then(Value::as_str).map(str::to_string)
}

/// The JS script's op runner, through the Rust bridge.
async fn run_ops(bridge: &DashboardRecords, ops: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for o in ops {
        let g = o["guild"].as_str().unwrap();
        let key = o["key"].as_str().unwrap();
        let id = match o.get("id").and_then(Value::as_str) {
            Some(s) if s.starts_with('$') => out[s[1..].parse::<usize>().unwrap()].get("ok").and_then(item_id).unwrap_or_default(),
            Some(s) => s.to_string(),
            None => String::new(),
        };
        let data = o.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
        let validated = |mode: Mode| -> Result<Map<String, Value>, StoreError> {
            dash_collections::validate_record_input(key, &Value::Object(data.clone()), mode, None, None, chrono::Utc::now().timestamp_millis() as f64).map(|v| v.data)
        };
        let res: Result<Value, StoreError> = match o["op"].as_str().unwrap() {
            "list" => bridge
                .list(g, key, ListOpts { query: o["query"].as_str().unwrap_or("").into(), cursor: o["cursor"].as_str().unwrap_or("").into() })
                .await
                .map(|l| match l.next_cursor {
                    Some(c) => json!({ "items": l.items, "nextCursor": c }),
                    None => json!({ "items": l.items }),
                }),
            "get" => bridge.get(g, key, &id).await.map(|v| v.unwrap_or(Value::Null)),
            "remove" => bridge.remove(g, key, &id, &actor()).await.map(|()| Value::Null),
            "action" => bridge.action(g, key, &id, o["actionId"].as_str().unwrap(), data.clone(), &actor()).await.map(|a| json!({ "item": a.item, "message": a.message })),
            "create" => match validated(Mode::Create) {
                Ok(d) => bridge.create(g, key, d, &actor()).await.map(|s| s.item),
                Err(e) => Err(e),
            },
            "update" => match validated(Mode::Update) {
                Ok(d) => bridge.update(g, key, &id, d, &actor()).await.map(|s| s.item),
                Err(e) => Err(e),
            },
            other => panic!("op {other}"),
        };
        out.push(match res {
            Ok(v) => json!({ "ok": v }),
            Err(e) => json!({ "error": error_json(&e) }),
        });
    }
    out
}

/// JS lists answer an array or `{ items, nextCursor }`; both as `{ items[, nextCursor] }`.
fn js_list_shape(v: &Value) -> Value {
    match v {
        Value::Array(_) => json!({ "items": v }),
        other => other.clone(),
    }
}

fn ops(g: &str) -> Vec<Value> {
    vec![
        json!({ "op": "create", "guild": g, "key": "levelRoles", "data": { "level": 5, "roleId": ROLE_PLAIN } }),
        json!({ "op": "create", "guild": g, "key": "shopItems", "data": { "name": "Badge", "price": 100, "description": "A shiny badge." } }),
        json!({ "op": "create", "guild": g, "key": "economyBoosts", "data": { "name": "Weekend", "percent": 25, "period": "DAILY" } }),
        json!({ "op": "create", "guild": g, "key": "triggers", "data": { "trigger": "hello", "response": "Hi there, {user}!", "matchType": "CONTAINS" } }),
        json!({ "op": "create", "guild": g, "key": "translateChannels", "data": { "channelId": CH_TEXT, "targetLangs": ["de", "fr"] } }),
        json!({ "op": "list", "guild": g, "key": "levelRoles" }),
        json!({ "op": "update", "guild": g, "key": "shopItems", "id": "$1", "data": { "price": 150 } }),
        json!({ "op": "update", "guild": g, "key": "triggers", "id": "$3", "data": { "response": "Hey!", "cooldown": 30 } }),
        json!({ "op": "get", "guild": g, "key": "economyBoosts", "id": "$2" }),
        json!({ "op": "create", "guild": g, "key": "levelRoles", "data": { "level": 10, "roleId": ROLE_MOD } }),
        json!({ "op": "remove", "guild": g, "key": "levelRoles", "id": "$0" }),
        json!({ "op": "get", "guild": g, "key": "levelRoles", "id": "$0" }),
        json!({ "op": "remove", "guild": g, "key": "levelRoles", "id": "$0" }),
        json!({ "op": "action", "guild": g, "key": "levelRoles", "id": "$9", "actionId": "nope" }),
        json!({ "op": "list", "guild": g, "key": "noSuchList" }),
        json!({ "op": "create", "guild": g, "key": "shopItems", "data": { "name": "Bad", "price": 0 } }),
        json!({ "op": "list", "guild": g, "key": "shopItems" }),
        json!({ "op": "list", "guild": g, "key": "economyBoosts" }),
        json!({ "op": "list", "guild": g, "key": "triggers" }),
        json!({ "op": "list", "guild": g, "key": "translateChannels" }),
        json!({ "op": "remove", "guild": g, "key": "shopItems", "id": "$1" }),
        json!({ "op": "list", "guild": g, "key": "shopItems" }),
    ]
}

/// Answers with the guild and the generated ids replaced, dates dropped.
fn normalised(results: &[Value], g: &str) -> Value {
    let mut pairs = vec![(g.to_string(), "<guild>".to_string())];
    for (i, r) in results.iter().enumerate() {
        if let Some(id) = r.get("ok").and_then(item_id) {
            pairs.push((id, format!("<id{i}>")));
        }
    }
    let lists: Vec<Value> = results.iter().map(|r| r.get("ok").map(|ok| json!({ "ok": js_list_shape(ok) })).unwrap_or_else(|| r.clone())).collect();
    replace_strings(&drop_keys(&Value::Array(lists), &["createdAt", "updatedAt"]), &pairs)
}

async fn bridge(app: &bot_core::App) -> (DashboardRecords, Arc<FakeDiscord>, Arc<MemoryTasks>) {
    let discord = Arc::new(FakeDiscord::new());
    let tasks = Arc::new(MemoryTasks::new());
    let d: Arc<dyn DashboardDiscord> = discord.clone();
    let t: Arc<dyn TaskRunner> = tasks.clone();
    (DashboardRecords::new(build_store_registry(app).expect("every collection has a store"), Some(d), t), discord, tasks)
}

#[tokio::test]
async fn every_collection_has_a_store() {
    let Some((db, redis)) = urls() else { return };
    let app = dashboard_app(&db, &redis).await;
    let registry = build_store_registry(&app).unwrap();
    assert!(registry.missing().is_empty());
    let keys: Vec<String> = dash_collections::Collections::builtin().keys().map(|k| k.to_string()).collect();
    assert_eq!(keys.len(), 22);
    for k in &keys {
        assert!(registry.store(k).is_ok(), "{k}");
    }
}

#[tokio::test]
async fn the_same_ops_through_both_bridges() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    for g in [A, B] {
        sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
        app.settings().invalidate_settings(g).await;
    }
    let (rust_bridge, discord, tasks) = bridge(&app).await;

    let js = node(JS, &["records", &Value::Array(ops(A)).to_string(), "null"]);
    let rust = run_ops(&rust_bridge, &ops(B)).await;
    let js_results = js["results"].as_array().unwrap();
    assert_eq!(js_results.len(), rust.len());
    let (want, got) = (normalised(js_results, A), normalised(&rust, B));
    for (i, (g, w)) in got.as_array().unwrap().iter().zip(want.as_array().unwrap()).enumerate() {
        assert_json_eq(&format!("op {i} {}", ops(B)[i]), g, w);
    }
    // These collections need no Discord write and no bot task.
    assert_eq!(js["tasks"], json!([]));
    assert!(tasks.calls().is_empty());
    let _ = discord;

    // The comparison saw real answers: created items, a refused price, a second remove of a removed row.
    for i in [0, 1, 2, 3, 4, 9] {
        assert!(rust[i]["ok"]["id"].is_string(), "op {i}: {}", rust[i]);
    }
    assert_eq!(rust[6]["ok"]["price"], json!(150));
    assert!(rust[11]["ok"].is_null(), "{}", rust[11]);
    assert!(rust[12]["error"].is_object() && rust[15]["error"].is_object(), "{} {}", rust[12], rust[15]);
    assert_eq!(rust[16]["ok"]["items"].as_array().map(Vec::len), Some(1));

    // The refusals the bridge itself answers.
    assert_eq!(rust[13]["error"], json!({ "name": "RecordError", "code": "not_found", "message": "Unknown action.", "path": null, "saved": false }));
    assert_eq!(rust[14]["error"]["message"], json!("Unknown list."));

    // Each side lists what the other wrote.
    for key in ["levelRoles", "shopItems", "economyBoosts", "triggers", "translateChannels"] {
        let rust_on_a = run_ops(&rust_bridge, &[json!({ "op": "list", "guild": A, "key": key })]).await;
        let js_on_a = node(JS, &["records", &json!([{ "op": "list", "guild": A, "key": key }]).to_string(), "null"]);
        assert_json_eq(&format!("Rust lists the JS {key}"), &js_list_shape(&rust_on_a[0]["ok"]), &js_list_shape(&js_on_a["results"][0]["ok"]));
        let js_on_b = node(JS, &["records", &json!([{ "op": "list", "guild": B, "key": key }]).to_string(), "null"]);
        let rust_on_b = run_ops(&rust_bridge, &[json!({ "op": "list", "guild": B, "key": key })]).await;
        assert_json_eq(&format!("JS lists the Rust {key}"), &js_list_shape(&js_on_b["results"][0]["ok"]), &js_list_shape(&rust_on_b[0]["ok"]));
    }

    for g in [A, B] {
        sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
    }
}

#[tokio::test]
async fn writes_hand_the_actor_and_the_runner_to_the_store() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let g = "960000000000000003";
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
    let (b, _discord, tasks) = bridge(&app).await;
    // levels' create is "Give XP": a xp.adjust task for this guild, acting as the dashboard user.
    let created = b.create(g, "levels", json!({ "userId": ALICE, "amount": 10 }).as_object().unwrap().clone(), &actor()).await;
    let calls = tasks.calls();
    assert_eq!(calls.len(), 1, "{created:?}");
    let (guild, kind, payload, opts) = &calls[0];
    assert_eq!((guild.as_str(), kind.as_str()), (g, "xp.adjust"));
    assert_eq!(payload, &json!({ "op": "give", "amount": 10, "userId": ALICE }));
    assert_eq!(opts.actor_id, "600000000000000001");
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
}
