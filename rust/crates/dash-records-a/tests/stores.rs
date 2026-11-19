//! dash-records-a: the stores against the mock database and Redis, both ways against the JS stores
//! (rust/fixtures/gen/dash-records-a-js.mjs), plus the store cases of test/dashboard*.test.js.

#![recursion_limit = "512"]

mod harness;

use harness::{code, scenario};
use serde_json::{Value, json};

fn snow(i: u64) -> String {
    format!("8{i:017}")
}

fn ok(v: &Value) -> &Value {
    v.get("ok").unwrap_or_else(|| panic!("expected ok, got {v}"))
}

// ── levelRoles ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn level_roles_both_ways() {
    let Some(t) = harness::app().await else { return };
    let ops = json!([
        { "op": "create", "store": "levelRoles", "data": { "level": 5, "roleId": snow(1) } },
        { "op": "create", "store": "levelRoles", "data": { "level": 5, "roleId": snow(2) } },
        { "op": "create", "store": "levelRoles", "data": { "level": 6, "roleId": snow(1) } },
        { "op": "create", "store": "levelRoles", "data": { "level": 10, "roleId": snow(2) } },
        { "op": "list", "store": "levelRoles" },
        { "op": "update", "store": "levelRoles", "id": "$ID3", "data": { "level": 12 } },
        { "op": "update", "store": "levelRoles", "id": "$ID3", "data": { "level": 5 } },
        { "op": "update", "store": "levelRoles", "id": "$ID3", "data": { "roleId": snow(1) } },
        { "op": "update", "store": "levelRoles", "id": "$ID3", "data": { "level": 12, "roleId": snow(2) } },
        { "op": "get", "store": "levelRoles", "id": "$ID0", "guild": "$O" },
        { "op": "update", "store": "levelRoles", "id": "$ID0", "guild": "$O", "data": { "level": 9 } },
        { "op": "remove", "store": "levelRoles", "id": "$ID0", "guild": "$O" },
        { "op": "create", "store": "levelRoles", "data": { "level": 1, "roleId": snow(9) } },
        { "op": "remove", "store": "levelRoles", "id": "$ID12" },
        { "op": "get", "store": "levelRoles", "id": "$ID12" },
        { "op": "remove", "store": "levelRoles", "id": "$ID12" },
        { "op": "get", "store": "levelRoles", "id": "$ID0" }
    ]);
    let rs = scenario(&t, 1, json!({ "ops": ops }), json!([]), Some("levelRoles")).await;
    let r = rs["results"].as_array().unwrap();
    assert_eq!(ok(&r[0])["item"], json!({ "id": ok(&r[0])["item"]["id"], "level": 5, "roleId": snow(1), "title": "Level 5" }));
    let codes: Vec<String> = r.iter().map(code).collect();
    assert_eq!(&codes[1..4], ["conflict", "conflict", "ok"]);
    assert_eq!(ok(&r[4])["items"].as_array().unwrap().iter().map(|i| i["level"].as_i64().unwrap()).collect::<Vec<_>>(), [5, 10]);
    assert_eq!(ok(&r[5])["item"]["level"], 12);
    assert_eq!(&codes[6..12], ["conflict", "conflict", "ok", "ok", "not_found", "not_found"]);
    assert_eq!(ok(&r[8])["item"]["level"], 12);
    assert_eq!(ok(&r[9]), &Value::Null);
    assert_eq!(&codes[13..16], ["ok", "ok", "not_found"]);
    assert_eq!(ok(&r[14]), &Value::Null);
}

// ── shared fixtures ───────────────────────────────────────────────────────────

const ALICE: &str = "500000000000000001";
const BOB: &str = "500000000000000002";
const CAROL: &str = "500000000000000003";
const ROBOT: &str = "500000000000000004";

fn user(id: &str, username: &str, global: Option<&str>, bot: bool) -> Value {
    json!({ "id": id, "username": username, "globalName": global, "nick": null, "avatarUrl": format!("https://cdn.example/{id}.png"), "bot": bot })
}

fn member_id(i: u64) -> String {
    format!("51{i:016}")
}

fn people() -> Vec<Value> {
    let mut u = vec![user(ALICE, "alice", Some("Alice"), false), user(BOB, "bob", None, false), user(ROBOT, "robot", None, true)];
    u.extend((10..=30).map(|i| user(&member_id(i), &format!("member{i}"), None, false)));
    u
}

fn resources() -> Value {
    json!({
        "guild": { "id": "0", "name": "Test Guild", "icon": null, "memberCount": 42, "ownerId": "700000000000000009", "premiumTier": 0,
                   "premiumSubscriptionCount": 0, "verificationLevel": 0 },
        "channels": [
            { "id": "600000000000000001", "name": "general", "type": 0, "kind": "text", "parentId": null, "position": 0, "nsfw": false },
            { "id": "600000000000000002", "name": "news", "type": 5, "kind": "announcement", "parentId": null, "position": 1, "nsfw": false },
            { "id": "600000000000000003", "name": "Voice", "type": 2, "kind": "voice", "parentId": null, "position": 2, "nsfw": false },
            { "id": "600000000000000004", "name": "hidden", "type": 0, "kind": "text", "parentId": null, "position": 3, "nsfw": false }
        ],
        "roles": [
            { "id": "610000000000000001", "name": "Quiet", "color": null, "position": 1, "managed": false, "everyone": false, "mentionable": false, "assignable": true, "dangerous": false },
            { "id": "610000000000000002", "name": "Ping", "color": null, "position": 2, "managed": false, "everyone": false, "mentionable": true, "assignable": true, "dangerous": false },
            { "id": "610000000000000003", "name": "Mod", "color": null, "position": 3, "managed": false, "everyone": false, "mentionable": true, "assignable": false, "dangerous": true },
            { "id": "610000000000000004", "name": "Bot role", "color": null, "position": 4, "managed": true, "everyone": false, "mentionable": false, "assignable": false, "dangerous": false }
        ],
        "emojis": [],
        "bot": { "userId": "700000000000000099", "topRolePosition": 3, "manageRoles": true, "createInvite": true }
    })
}

fn discord() -> Value {
    json!({ "resources": resources(), "users": people(), "members": {} })
}

fn actor(member: Value) -> Value {
    json!({ "userId": BOB, "user": { "id": BOB, "username": "bob", "globalName": null, "avatar": null }, "member": member })
}

fn owner() -> Value {
    json!({ "owner": true, "permissions": "0" })
}

fn codes_of(results: &[Value]) -> Vec<String> {
    results.iter().map(code).collect()
}

async fn concurrent_codes(t: &bot_core::testing::DbTestApp, key: &str, g: &str, inputs: Vec<Value>) -> Vec<String> {
    let store = dash_records_a::store_for(key, t).unwrap();
    let ctx = dash_collections::StoreCtx { guild_id: g.into(), user_id: "700000000000000001".into(), ..Default::default() };
    let runs = inputs.into_iter().map(|data| {
        let (store, ctx) = (store.clone(), ctx.clone());
        async move { store.create(g, data.as_object().unwrap().clone(), &ctx).await }
    });
    futures::future::join_all(runs)
        .await
        .iter()
        .map(|r| match r {
            Ok(_) => "ok".to_string(),
            Err(e) => e.as_record().map_or_else(|| format!("{e:?}"), |e| e.code.as_str().to_string()),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn level_roles_cap_under_concurrency() {
    let Some(t) = harness::app().await else { return };
    let g = "940000990000000002";
    t.reset_guilds(&[g]).await;
    t.settings().ensure_guild_settings(g).await.unwrap();
    for i in 0..98i32 {
        sqlx::query(r#"INSERT INTO "LevelRole" ("id", "guildId", "level", "roleId") VALUES ($1, $2, $3, $4)"#)
            .bind(bot_db::new_cuid())
            .bind(g)
            .bind(i + 1)
            .bind(snow(100 + i as u64))
            .execute(&t.pool)
            .await
            .unwrap();
    }
    let codes = concurrent_codes(&t, "levelRoles", g, (0..5).map(|i| json!({ "level": 200 + i, "roleId": snow(500 + i) })).collect()).await;
    assert_eq!(codes.iter().filter(|c| *c == "ok").count(), 2, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| *c == "limit").count(), 3, "{codes:?}");
    let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM "LevelRole" WHERE "guildId" = $1"#).bind(g).fetch_one(&t.pool).await.unwrap();
    assert_eq!(n, 100);
    t.reset_guilds(&[g]).await;
}

// ── economyBoosts ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn economy_boosts_both_ways() {
    let Some(t) = harness::app().await else { return };
    let use_sql = "INSERT INTO \"EconomyBoostUse\" (\"id\", \"boostId\", \"userId\", \"periodKey\") SELECT 'u' || substr(\"id\", 2), \"id\", '500000000000000005', 'D:2026-01-01' FROM \"EconomyBoost\" WHERE \"guildId\" = '$G' AND \"name\" = 'NITRO'";
    let ops = json!([
        { "op": "create", "store": "economyBoosts", "data": { "name": " Nitro  boost ", "percent": 50, "period": "DAILY", "roleId": snow(1), "enabled": true } },
        { "op": "create", "store": "economyBoosts", "data": { "name": "NITRO BOOST", "percent": 5, "period": "DAILY", "roleId": null, "enabled": true } },
        { "op": "create", "store": "economyBoosts", "data": { "name": "Everyone", "percent": 10, "period": "WEEKLY", "roleId": null, "enabled": false } },
        { "op": "update", "store": "economyBoosts", "id": "$ID0", "data": { "percent": 80, "roleId": null, "enabled": false, "name": "Nitro" } },
        { "op": "update", "store": "economyBoosts", "id": "$ID0", "data": { "name": "everyone" } },
        { "op": "update", "store": "economyBoosts", "id": "$ID0", "data": { "name": "NITRO" } },
        { "op": "update", "store": "economyBoosts", "id": "$ID0", "data": { "name": "   " } },
        { "op": "sql", "sql": use_sql },
        { "op": "get", "store": "economyBoosts", "id": "$ID0" },
        { "op": "list", "store": "economyBoosts" },
        { "op": "update", "store": "economyBoosts", "id": "$ID0", "guild": "$O", "data": { "percent": 5 } },
        { "op": "remove", "store": "economyBoosts", "id": "$ID0", "guild": "$O" },
        { "op": "get", "store": "economyBoosts", "id": "nope" },
        { "op": "create", "store": "economyBoosts", "data": { "name": "Third", "percent": 7, "period": "MONTHLY", "roleId": null, "enabled": true } },
        { "op": "remove", "store": "economyBoosts", "id": "$ID13" },
        { "op": "remove", "store": "economyBoosts", "id": "$ID13" }
    ]);
    let rs = scenario(&t, 2, json!({ "ops": ops }), json!([]), Some("economyBoosts")).await;
    let r = rs["results"].as_array().unwrap();
    let b = &ok(&r[0])["item"];
    assert_eq!((b["name"].as_str(), b["period"].as_str(), b["uses"].as_i64()), (Some("Nitro boost"), Some("DAILY"), Some(0)));
    assert!(b["summary"].as_str().unwrap().starts_with("+50% · daily · one role"));
    assert_eq!(code(&r[1]), "conflict");
    assert_eq!(ok(&r[2])["item"]["enabled"], false);
    let e = &ok(&r[3])["item"];
    assert_eq!(json!([e["percent"], e["roleId"], e["enabled"], e["name"], e["period"]]), json!([80, null, false, "Nitro", "DAILY"]));
    assert_eq!(code(&r[4]), "conflict");
    assert_eq!(ok(&r[5])["item"]["name"], "NITRO");
    assert_eq!(code(&r[6]), "invalid");
    assert_eq!(ok(&r[8])["uses"], 1);
    assert_eq!([code(&r[10]), code(&r[11]), code(&r[15])], ["not_found", "not_found", "not_found"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn economy_boosts_cap_under_concurrency() {
    let Some(t) = harness::app().await else { return };
    let g = "940000990000000001";
    t.reset_guilds(&[g]).await;
    let inputs = (0..13).map(|i| json!({ "name": format!("b{i}"), "percent": 5, "period": "DAILY", "roleId": null, "enabled": true })).collect();
    let codes = concurrent_codes(&t, "economyBoosts", g, inputs).await;
    assert_eq!(codes.iter().filter(|c| *c == "ok").count(), 10, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| *c == "limit").count(), 3, "{codes:?}");
    t.reset_guilds(&[g]).await;
}

// ── balances ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn balances_both_ways() {
    let Some(t) = harness::app().await else { return };
    let mut seeds = vec![json!({ "fn": "settings", "data": { "economyMaxBalance": 5000 } })];
    for i in 1..=30u64 {
        seeds.push(json!({ "fn": "setBalance", "userId": member_id(i), "balance": if i == 30 { 290 } else { i * 10 } }));
    }
    let ops = json!([
        { "op": "create", "store": "balances", "data": { "userId": ALICE, "amount": 500 } },
        { "op": "create", "store": "balances", "data": { "userId": ROBOT, "amount": 5 } },
        { "op": "create", "store": "balances", "data": { "userId": CAROL, "amount": 5 } },
        { "op": "create", "store": "balances", "data": { "userId": ALICE, "amount": 5 }, "ctx": { "discord": false } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "take", "data": { "amount": 1_000_000_000 } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "set", "data": { "balance": 6000 } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "set", "data": { "balance": 4000 } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "give", "data": { "amount": 5000 } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "reset", "data": { "what": "coins" } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "reset", "data": { "what": "all" } },
        { "op": "action", "store": "balances", "id": "nope", "action": "give", "data": { "amount": 5 } },
        { "op": "action", "store": "balances", "id": ROBOT, "action": "give", "data": { "amount": 5 } },
        { "op": "update", "store": "balances", "id": ALICE, "data": {} },
        { "op": "remove", "store": "balances", "id": ALICE },
        { "op": "get", "store": "balances", "id": "nope" },
        { "op": "get", "store": "balances", "id": ALICE },
        { "op": "list", "store": "balances" },
        { "op": "list", "store": "balances", "cursor": "$C16" },
        { "op": "list", "store": "balances", "query": member_id(3) },
        { "op": "list", "store": "balances", "query": "member2" },
        { "op": "list", "store": "balances", "query": "nobody" },
        { "op": "list", "store": "balances", "query": format!("<@{}>", member_id(7)) },
        { "op": "fail", "method": "searchMembers", "error": { "message": "boom", "status": 500, "code": "unavailable" } },
        { "op": "list", "store": "balances", "query": "member" },
        { "op": "fail", "method": "getUsers", "error": { "message": "boom", "status": 500, "code": "unavailable" } },
        { "op": "action", "store": "balances", "id": ALICE, "action": "give", "data": { "amount": 5 } }
    ]);
    let spec = json!({ "discord": discord(), "ops": ops, "actor": actor(owner()) });
    let rs = scenario(&t, 3, spec, json!(seeds), Some("balances")).await;
    let r = rs["results"].as_array().unwrap();
    assert_eq!(ok(&r[0])["item"]["balance"], 1500);
    assert_eq!(ok(&r[0])["item"]["name"], "Alice");
    assert_eq!(ok(&r[0])["item"]["rank"], 1);
    assert_eq!(codes_of(&r[1..4]), ["invalid", "invalid", "unavailable"]);
    assert_eq!(ok(&r[4])["item"]["balance"], 0);
    assert!(ok(&r[4])["message"].as_str().unwrap().contains("They only had"));
    assert_eq!(code(&r[5]), "invalid");
    assert_eq!(ok(&r[6])["item"]["balance"], 4000);
    assert_eq!(ok(&r[7])["item"]["balance"], 5000);
    assert!(ok(&r[7])["message"].as_str().unwrap().contains("server maximum reached"));
    assert_eq!(ok(&r[8])["message"], "Reset 1 balance.");
    assert_eq!(ok(&r[8])["item"]["balance"], 1000);
    assert_eq!(codes_of(&r[10..14]), ["not_found", "invalid", "invalid", "invalid"]);
    assert_eq!(ok(&r[14]), &Value::Null);
    let first = ok(&r[16]);
    assert_eq!(first["items"].as_array().unwrap().len(), 25);
    assert_eq!(json!([first["items"][1]["userId"], first["items"][1]["rank"], first["items"][2]["rank"]]), json!([member_id(29), 2, 2]));
    let second = ok(&r[17]);
    assert_eq!(second["items"].as_array().unwrap().len(), 6);
    assert!(second.get("nextCursor").is_none());
    assert_eq!(code(&r[23]), "discord");
    assert_eq!(code(&r[25]), "discord");
}

// ── levels ────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn levels_both_ways() {
    let Some(t) = harness::app().await else { return };
    let mut seeds: Vec<Value> = (1..=30u64).map(|i| json!({ "fn": "applyXpDelta", "userId": member_id(i), "delta": i * 100, "messages": i })).collect();
    seeds.push(json!({ "fn": "applyXpDelta", "userId": ALICE, "delta": 5000 }));
    let ops = json!([
        { "op": "list", "store": "levels" },
        { "op": "list", "store": "levels", "cursor": "$C0" },
        { "op": "list", "store": "levels", "cursor": "garbage" },
        { "op": "list", "store": "levels", "query": "member3" },
        { "op": "list", "store": "levels", "query": "nobody" },
        { "op": "get", "store": "levels", "id": ALICE },
        { "op": "get", "store": "levels", "id": "nope" },
        { "op": "get", "store": "levels", "id": CAROL },
        { "op": "answer", "kind": "xp.adjust", "result": { "status": "done", "message": "Gave 5,000 XP to alice.", "data": null } },
        { "op": "create", "store": "levels", "data": { "userId": ALICE, "amount": 5000 } },
        { "op": "create", "store": "levels", "data": { "userId": CAROL, "amount": 5 } },
        { "op": "action", "store": "levels", "id": ALICE, "action": "take", "data": { "amount": 1000 } },
        { "op": "action", "store": "levels", "id": ALICE, "action": "set", "data": { "xp": 42 } },
        { "op": "action", "store": "levels", "id": ALICE, "action": "reset", "data": {} },
        { "op": "action", "store": "levels", "id": "nope", "action": "reset", "data": {} },
        { "op": "answer", "kind": "xp.adjust", "result": { "status": "failed", "message": "That member is not in this server." } },
        { "op": "create", "store": "levels", "data": { "userId": CAROL, "amount": 5 } },
        { "op": "answer", "kind": "xp.adjust", "result": { "status": "pending", "message": "Still working" } },
        { "op": "create", "store": "levels", "data": { "userId": ALICE, "amount": 5 } },
        { "op": "action", "store": "levels", "id": ALICE, "action": "give", "data": { "amount": 5 } },
        { "op": "answer", "kind": "xp.adjust", "result": { "status": "offline", "message": "off" } },
        { "op": "create", "store": "levels", "data": { "userId": ALICE, "amount": 5 } },
        { "op": "create", "store": "levels", "data": { "userId": ALICE, "amount": 5 }, "ctx": { "tasks": false } },
        { "op": "answer", "kind": "xp.adjust", "result": { "throw": "database down" } },
        { "op": "create", "store": "levels", "data": { "userId": ALICE, "amount": 5 } },
        { "op": "update", "store": "levels", "id": ALICE, "data": {} },
        { "op": "remove", "store": "levels", "id": ALICE }
    ]);
    let spec = json!({ "discord": discord(), "ops": ops, "actor": actor(owner()) });
    let rs = scenario(&t, 4, spec, json!(seeds), Some("levels")).await;
    let r = rs["results"].as_array().unwrap();
    let first = &ok(&r[0])["items"];
    assert_eq!(json!([first[0]["userId"], first[0]["rank"], first[1]["userId"], first[1]["rank"]]), json!([ALICE, 1, member_id(30), 2]));
    assert_eq!(ok(&r[1])["items"].as_array().unwrap().iter().map(|i| i["rank"].as_i64().unwrap()).collect::<Vec<_>>(), [26, 27, 28, 29, 30, 31]);
    assert_eq!(ok(&r[2])["items"].as_array().unwrap().len(), 25, "a bad cursor is the first page");
    assert_eq!(ok(&r[3])["items"][0]["rank"], 2, "member30 leads the search");
    assert_eq!(ok(&r[6]), &Value::Null);
    assert_eq!(ok(&r[9])["item"]["xp"], 5000);
    assert_eq!(ok(&r[10])["item"]["xp"], 0, "no row: the empty item");
    assert_eq!(code(&r[14]), "not_found");
    assert_eq!(code(&r[16]), "failed");
    assert_eq!(
        r[18]["err"],
        json!({ "code": "unavailable", "message": "The bot is still working on it. Refresh the list in a moment to see the result.", "saved": true })
    );
    assert_eq!(ok(&r[19])["message"], "Still working", "a pending action answers the item");
    assert_eq!(codes_of(&r[21..23]), ["unavailable", "unavailable"]);
    assert!(r[24]["err"].get("internal").is_some());
    assert_eq!(rs["tasks"][0]["payload"], json!({ "op": "give", "amount": 5000, "userId": ALICE }));
    assert_eq!(rs["tasks"][4]["payload"], json!({ "op": "reset", "userId": ALICE }));
}

// ── cases ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cases_both_ways() {
    let Some(t) = harness::app().await else { return };
    let inf = |target: &str, ty: &str, reason: &str| json!({ "fn": "recordInfraction", "data": { "targetUserId": target, "moderatorId": BOB, "type": ty, "reason": reason } });
    let mut seeds = vec![inf(ALICE, "WARN", "spam")];
    seeds.push(json!({ "fn": "recordInfraction", "expiresInMs": 7 * 86_400_000, "data": { "targetUserId": ALICE, "moderatorId": BOB, "type": "BAN", "reason": "raid", "proofUrl": "https://x.test/p" } }));
    seeds.push(inf(ALICE, "UNBAN", "appeal"));
    for i in 0..30 {
        seeds.push(inf(CAROL, "NOTE", &format!("note {i}")));
    }
    let done = |message: &str, data: Value| json!({ "status": "done", "message": message, "data": data });
    let ops = json!([
        { "op": "list", "store": "cases" },
        { "op": "list", "store": "cases", "cursor": "$C0" },
        { "op": "list", "store": "cases", "query": "#2" },
        { "op": "list", "store": "cases", "query": ALICE },
        { "op": "list", "store": "cases", "query": "ali" },
        { "op": "list", "store": "cases", "query": "999" },
        { "op": "get", "store": "cases", "id": "2" },
        { "op": "get", "store": "cases", "id": "abc" },
        { "op": "get", "store": "cases", "id": "0" },
        { "op": "answer", "kind": "mod.action", "result": done("Warned alice.", json!({ "caseId": 1 })) },
        { "op": "create", "store": "cases", "data": { "action": "WARN", "userId": ALICE, "reason": "spam" } },
        { "op": "answer", "kind": "mod.action", "result": done("Done.", Value::Null) },
        { "op": "create", "store": "cases", "data": { "action": "KICK", "userId": CAROL, "reason": "x" } },
        { "op": "create", "store": "cases", "data": { "action": "WARN", "userId": ROBOT } },
        { "op": "answer", "kind": "mod.action", "result": { "status": "pending", "message": "Still working" } },
        { "op": "create", "store": "cases", "data": { "action": "BAN", "userId": ALICE, "duration": "7d", "deleteDays": 1, "proofUrl": "https://x.test/p" } },
        { "op": "answer", "kind": "mod.action", "result": { "status": "failed", "message": "You can't moderate yourself." } },
        { "op": "create", "store": "cases", "data": { "action": "KICK", "userId": BOB } },
        { "op": "create", "store": "cases", "data": { "action": "KICK", "userId": BOB }, "ctx": { "tasks": false } },
        { "op": "update", "store": "cases", "id": "1", "data": { "reason": "spam" } },
        { "op": "update", "store": "cases", "id": "1", "data": { "reason": "spam links", "proofUrl": "https://x.test/q" } },
        { "op": "update", "store": "cases", "id": "999", "data": { "reason": "x" } },
        { "op": "update", "store": "cases", "id": "1", "data": { "proofUrl": null } },
        { "op": "answer", "kind": "mod.clearWarnings", "result": done("Cleared 1 warning(s) for alice (case #34).", Value::Null) },
        { "op": "action", "store": "cases", "id": "1", "action": "clearWarnings", "data": { "clearReason": "fresh start" } },
        { "op": "action", "store": "cases", "id": "abc", "action": "clearWarnings", "data": {} },
        { "op": "remove", "store": "cases", "id": "1" },
        { "op": "answer", "kind": "mod.editCase", "result": { "status": "pending", "message": "Still working on it" } },
        { "op": "update", "store": "cases", "id": "1", "data": { "reason": "other" } }
    ]);
    let spec = json!({ "discord": discord(), "ops": ops, "actor": actor(owner()) });
    let rs = scenario(&t, 5, spec, json!(seeds), Some("cases")).await;
    let r = rs["results"].as_array().unwrap();
    let first = ok(&r[0]);
    assert_eq!(first["items"].as_array().unwrap().len(), 25);
    assert_eq!(first["items"][0]["caseNumber"], 33);
    assert_eq!(first["nextCursor"], "c9");
    assert_eq!(ok(&r[1])["items"].as_array().unwrap().iter().map(|i| i["caseNumber"].as_i64().unwrap()).collect::<Vec<_>>(), [8, 7, 6, 5, 4, 3, 2, 1]);
    let ids = |v: &Value| v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(ids(ok(&r[2])), ["2"]);
    assert_eq!(ids(ok(&r[3])), ["3", "2", "1"]);
    assert_eq!(ids(ok(&r[4])), ["3", "2", "1"]);
    assert!(ids(ok(&r[5])).is_empty());
    assert_eq!(ok(&r[6])["type"], "TEMPBAN");
    assert_eq!(ok(&r[6])["duration"], "1w");
    assert_eq!(ok(&r[7]), &Value::Null);
    let warn = &ok(&r[10])["item"];
    assert_eq!(json!([warn["id"], warn["title"], warn["member"], warn["moderator"]]), json!(["1", "#1 · Warn", "Alice (@alice)", "@bob"]));
    assert_eq!(ok(&r[12])["item"]["id"], "33");
    assert_eq!(r[13]["err"], json!({ "code": "unavailable", "message": "Done. Refresh the list to see the case." }));
    assert_eq!(r[15]["err"]["saved"], true);
    assert_eq!(codes_of(&r[17..19]), ["failed", "unavailable"]);
    assert_eq!(ok(&r[19])["item"]["reason"], "spam");
    assert_eq!(codes_of(&r[21..22]), ["not_found"]);
    assert_eq!(codes_of(&r[25..27]), ["not_found", "invalid"]);
    assert_eq!(r[28]["err"], json!({ "code": "unavailable", "message": "Still working on it", "saved": true }));
    let kinds: Vec<&str> = rs["tasks"].as_array().unwrap().iter().map(|c| c["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["mod.action", "mod.action", "mod.action", "mod.action", "mod.action", "mod.editCase", "mod.clearWarnings", "mod.editCase"]);
    assert_eq!(rs["tasks"][5]["payload"], json!({ "caseId": 1, "reason": "spam links", "proofUrl": "https://x.test/q" }));
    assert_eq!(rs["tasks"][6]["payload"], json!({ "userId": ALICE, "reason": "fresh start" }));
}

// ── giveaways ─────────────────────────────────────────────────────────────────

fn giveaway_seeds() -> Value {
    let base = |prize: &str, status: &str, extra: Value| {
        let mut d = json!({ "channelId": "600000000000000001", "hostId": BOB, "prize": prize, "status": status, "endsAt": { "$date": 86_400_000 } });
        for (k, v) in extra.as_object().unwrap() {
            d[k] = v.clone();
        }
        json!({ "fn": "create", "model": "giveaway", "data": d })
    };
    let mut s = vec![
        base("Nitro", "ACTIVE", json!({ "messageId": "620000000000000001", "roleWeights": [{ "roleId": "610000000000000002", "weight": 3 }], "xpPrize": 100, "coinPrize": 0, "requiredRoleIds": ["610000000000000001"] })),
        base("Drafted", "DRAFT", json!({ "endsAt": { "$date": 2 * 86_400_000 } })),
        base("Second", "ACTIVE", json!({ "endsAt": { "$date": 3_600_000 } })),
    ];
    for i in 1..=27 {
        s.push(base(&format!("Old {i}"), "ENDED", json!({
            "createdAt": { "$at": format!("2026-01-{i:02}T10:00:00.000Z") }, "endsAt": { "$at": format!("2026-01-{i:02}T12:00:00.000Z") },
            "endedAt": { "$at": format!("2026-01-{i:02}T12:00:01.000Z") }, "winnerIds": [ALICE]
        })));
    }
    s.push(base("Gone", "CANCELLED", json!({ "createdAt": { "$at": "2026-02-01T10:00:00.000Z" } })));
    for u in [ALICE, CAROL, ROBOT] {
        s.push(json!({ "fn": "create", "model": "giveawayEntry", "noGuild": true, "data": { "giveawayId": "$S0", "userId": u } }));
    }
    json!(s)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn giveaways_both_ways() {
    let Some(t) = harness::app().await else { return };
    let done = |message: &str, data: Value| json!({ "status": "done", "message": message, "data": data });
    let started = "INSERT INTO \"Giveaway\" (\"id\", \"guildId\", \"channelId\", \"hostId\", \"prize\", \"endsAt\", \"createdAt\", \"updatedAt\") VALUES ('gs$G', '$G', '600000000000000001', '500000000000000002', 'Fresh', (now() AT TIME ZONE 'UTC') + interval '1 day', (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'))";
    let ops = json!([
        { "op": "list", "store": "giveaways" },
        { "op": "list", "store": "giveaways", "cursor": "$C0" },
        { "op": "list", "store": "giveaways", "cursor": "garbage" },
        { "op": "list", "store": "giveaways", "query": "nitro" },
        { "op": "list", "store": "giveaways", "query": "old 2" },
        { "op": "get", "store": "giveaways", "id": "$S0" },
        { "op": "get", "store": "giveaways", "id": "cnotthere00000000000000000" },
        { "op": "get", "store": "giveaways", "id": "$S0", "guild": "$O" },
        { "op": "answer", "kind": "giveaway.start", "result": done("Started.", json!({ "giveawayId": "$S1" })) },
        { "op": "create", "store": "giveaways", "data": { "prize": "Nitro", "channelId": "600000000000000001", "winnerCount": 1, "duration": "1d", "roleWeights": [] } },
        { "op": "create", "store": "giveaways", "data": { "prize": "Nitro" }, "ctx": { "tasks": false } },
        { "op": "answer", "kind": "giveaway.start", "result": done("Started.", Value::Null) },
        { "op": "create", "store": "giveaways", "data": { "prize": "Lost", "channelId": "600000000000000001", "winnerCount": 2 }, "ctx": { "userId": CAROL } },
        { "op": "sql", "sql": started },
        { "op": "create", "store": "giveaways", "data": { "prize": "Fresh", "channelId": "600000000000000001", "winnerCount": 1 } },
        { "op": "answer", "kind": "giveaway.start", "result": { "status": "pending", "message": "Still working" } },
        { "op": "create", "store": "giveaways", "data": { "prize": "Later" }, "ctx": { "userId": CAROL } },
        { "op": "update", "store": "giveaways", "id": "$S0", "data": { "roleWeights": [{ "roleId": "610000000000000002", "weight": 5 }] } },
        { "op": "update", "store": "giveaways", "id": "$S0", "data": {} },
        { "op": "update", "store": "giveaways", "id": "cnotthere00000000000000000", "data": { "roleWeights": [] } },
        { "op": "remove", "store": "giveaways", "id": "$S0" },
        { "op": "answer", "kind": "giveaway.end", "result": done("The giveaway ended.", Value::Null) },
        { "op": "action", "store": "giveaways", "id": "$S0", "action": "end", "data": {} },
        { "op": "action", "store": "giveaways", "id": "$S0", "action": "reroll", "data": { "winners": 1 } },
        { "op": "action", "store": "giveaways", "id": "$S0", "action": "reroll", "data": {} },
        { "op": "action", "store": "giveaways", "id": "$S1", "action": "restart", "data": { "duration": "1d" } },
        { "op": "action", "store": "giveaways", "id": "$S2", "action": "cancel", "data": {} },
        { "op": "answer", "kind": "giveaway.end", "result": { "status": "failed", "message": "This giveaway already ended." } },
        { "op": "action", "store": "giveaways", "id": "$S0", "action": "end", "data": {} },
        { "op": "action", "store": "giveaways", "id": "cnotthere00000000000000000", "action": "end", "data": {} },
        { "op": "action", "store": "giveaways", "id": "$S0", "guild": "$O", "action": "end", "data": {} },
        { "op": "action", "store": "giveaways", "id": "bad id!", "action": "end", "data": {} },
        { "op": "action", "store": "giveaways", "id": "$S0", "action": "end", "data": {}, "ctx": { "tasks": false } },
        // An empty giveawayId is falsy in the JS (`if (!id)`): the new giveaway is looked up instead.
        { "op": "sql", "sql": started.replace("'gs$G'", "'ge$G'").replace("'Fresh'", "'Empty id'") },
        { "op": "answer", "kind": "giveaway.start", "result": done("Started.", json!({ "giveawayId": "" })) },
        { "op": "create", "store": "giveaways", "data": { "prize": "Empty id", "channelId": "600000000000000001", "winnerCount": 1 } }
    ]);
    let spec = json!({ "discord": discord(), "ops": ops, "actor": actor(owner()) });
    let rs = scenario(&t, 6, spec, giveaway_seeds(), Some("giveaways")).await;
    let r = rs["results"].as_array().unwrap();
    let prizes = |v: &Value| v["items"].as_array().unwrap().iter().map(|i| i["prize"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let first = prizes(ok(&r[0]));
    assert_eq!(&first[..4], ["Second", "Nitro", "Drafted", "Gone"], "running (soonest first), drafts, then the newest finished");
    assert_eq!(first.len(), 28);
    assert_eq!(prizes(ok(&r[1])), ["Old 3", "Old 2", "Old 1"]);
    assert!(prizes(ok(&r[2])).is_empty());
    assert_eq!(prizes(ok(&r[3])), ["Nitro"]);
    assert_eq!(prizes(ok(&r[4])), ["Old 27", "Old 26", "Old 25", "Old 24", "Old 23", "Old 22", "Old 21", "Old 20", "Old 2"]);
    let nitro = ok(&r[5]);
    assert_eq!(json!([nitro["entries"], nitro["host"], nitro["xpPrize"], nitro["coinPrize"]]), json!([3, BOB, 100, null]));
    assert_eq!(ok(&r[0])["items"][1]["host"], "bob", "list resolves names, get does not");
    assert_eq!([ok(&r[6]), ok(&r[7])], [&Value::Null, &Value::Null]);
    assert_eq!(ok(&r[9])["item"]["prize"], "Drafted");
    assert_eq!(code(&r[10]), "unavailable");
    assert_eq!(r[12]["err"], json!({ "code": "unavailable", "message": "The giveaway was started, but it could not be loaded. Reload the list.", "saved": true }));
    assert_eq!(ok(&r[14])["item"]["prize"], "Fresh");
    assert_eq!(r[16]["err"], json!({ "code": "unavailable", "message": "Still working", "saved": true }));
    assert_eq!(codes_of(&r[19..21]), ["not_found", "invalid"]);
    assert_eq!(ok(&r[22])["message"], "The giveaway ended.");
    assert_eq!(codes_of(&r[28..33]), ["failed", "not_found", "not_found", "not_found", "unavailable"]);
    assert_eq!(ok(&r[35])["item"]["prize"], "Empty id");
    let payloads: Vec<&Value> = rs["tasks"].as_array().unwrap().iter().map(|c| &c["payload"]).collect();
    assert_eq!(payloads[0]["requiredRoleIds"], json!([]));
    assert_eq!(payloads[0]["duration"], "1d");
    assert!(payloads.iter().any(|p| p.get("winners").is_none() && p.get("giveawayId").is_some() && p.as_object().unwrap().len() == 1));
}

// ── polls ─────────────────────────────────────────────────────────────────────

fn poll_seeds() -> Value {
    let poll = |q: &str, options: Value, max: i64, extra: Value| {
        let mut s = json!({ "fn": "createPollRow", "pending": { "channelId": "600000000000000001", "userId": BOB, "durationMs": 86_400_000 },
                            "input": { "question": q, "options": options, "maxChoices": max } });
        if !extra.is_null() {
            s["update"] = extra;
        }
        s
    };
    let mut s = vec![
        poll("Best pizza?", json!(["Margherita", "Pepperoni", "Hawaii"]), 2, Value::Null),
        poll("Open ended", json!(["Yes", "No"]), 1, json!({ "endsAt": null, "messageId": "620000000000000009" })),
    ];
    for i in 1..=27 {
        s.push(poll(&format!("Old {i}"), json!(["A", "B"]), 1, json!({ "status": "ENDED", "createdAt": { "$at": format!("2026-01-{i:02}T10:00:00.000Z") }, "endsAt": { "$at": format!("2026-01-{i:02}T12:00:00.000Z") } })));
    }
    for (u, idx) in [(ALICE, 0), (BOB, 0), (BOB, 1), (CAROL, 2)] {
        s.push(json!({ "fn": "applyVote", "poll": 0, "userId": u, "maxChoices": 2, "idx": idx }));
    }
    json!(s)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polls_both_ways() {
    let Some(t) = harness::app().await else { return };
    let done = |message: &str, data: Value| json!({ "status": "done", "message": message, "data": data });
    let ops = json!([
        { "op": "list", "store": "polls" },
        { "op": "list", "store": "polls", "cursor": "$C0" },
        { "op": "list", "store": "polls", "query": "PIZZA" },
        { "op": "list", "store": "polls", "cursor": "1:x!" },
        { "op": "get", "store": "polls", "id": "$S0" },
        { "op": "get", "store": "polls", "id": "cnotthere00000000000000000" },
        { "op": "get", "store": "polls", "id": "$S0", "guild": "$O" },
        { "op": "answer", "kind": "poll.start", "result": done("Posted.", json!({ "pollId": "$S1" })) },
        { "op": "create", "store": "polls", "data": { "question": "Q?", "options": ["a", "b"], "channelId": "600000000000000001", "maxChoices": 1, "duration": null, "hideResults": true } },
        { "op": "create", "store": "polls", "data": { "question": "Q?" }, "ctx": { "tasks": false } },
        { "op": "answer", "kind": "poll.start", "result": done("Posted.", Value::Null) },
        { "op": "create", "store": "polls", "data": { "question": "Q?", "options": ["a", "b"], "channelId": "600000000000000001", "maxChoices": 1 }, "ctx": { "userId": CAROL } },
        { "op": "answer", "kind": "poll.start", "result": { "status": "pending", "message": "Still posting" } },
        { "op": "create", "store": "polls", "data": { "question": "Q?" }, "ctx": { "userId": CAROL } },
        { "op": "update", "store": "polls", "id": "$S0", "data": {} },
        { "op": "answer", "kind": "poll.end", "result": done("The poll ended.", Value::Null) },
        { "op": "action", "store": "polls", "id": "$S0", "action": "end", "data": {} },
        { "op": "action", "store": "polls", "id": "nothere", "action": "end", "data": {} },
        { "op": "answer", "kind": "poll.end", "result": { "status": "failed", "message": "This poll already ended." } },
        { "op": "action", "store": "polls", "id": "$S0", "action": "end", "data": {} },
        { "op": "remove", "store": "polls", "id": "$S1" },
        { "op": "remove", "store": "polls", "id": "nothere" },
        { "op": "remove", "store": "polls", "id": "$S1", "ctx": { "tasks": false } },
        // An empty pollId is falsy in the JS (`if (!id)`): the new poll is looked up instead.
        { "op": "sql", "sql": "INSERT INTO \"Poll\" (\"id\", \"guildId\", \"channelId\", \"authorId\", \"question\", \"options\", \"createdAt\", \"updatedAt\") VALUES ('pe$G', '$G', '600000000000000001', '500000000000000002', 'Empty id?', ARRAY['a','b'], (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC'))" },
        { "op": "answer", "kind": "poll.start", "result": done("Posted.", json!({ "pollId": "" })) },
        { "op": "create", "store": "polls", "data": { "question": "Empty id?", "options": ["a", "b"], "channelId": "600000000000000001", "maxChoices": 1 } }
    ]);
    let spec = json!({ "discord": discord(), "ops": ops, "actor": actor(owner()) });
    let rs = scenario(&t, 7, spec, poll_seeds(), Some("polls")).await;
    let r = rs["results"].as_array().unwrap();
    let qs = |v: &Value| v["items"].as_array().unwrap().iter().map(|i| i["question"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let first = ok(&r[0]);
    assert_eq!(qs(first).len(), 27);
    assert_eq!(qs(ok(&r[1])), ["Old 2", "Old 1"]);
    assert_eq!(qs(ok(&r[2])), ["Best pizza?"]);
    assert!(qs(ok(&r[3])).is_empty());
    let pizza = ok(&r[4]);
    assert_eq!(json!([pizza["votes"], pizza["voters"], pizza["author"]]), json!([4, 3, BOB]));
    assert_eq!(pizza["results"][0], "1. Margherita: 2 votes (50%)");
    assert_eq!(ok(&r[8])["item"]["question"], "Open ended");
    assert_eq!(code(&r[9]), "unavailable");
    assert_eq!(r[11]["err"]["message"], "The poll was posted, but it could not be loaded. Reload the list.");
    assert_eq!(r[13]["err"], json!({ "code": "unavailable", "message": "Still posting", "saved": true }));
    assert_eq!(code(&r[14]), "invalid");
    assert_eq!(ok(&r[16])["message"], "The poll ended.");
    assert_eq!(codes_of(&r[17..18]), ["not_found"]);
    assert_eq!(code(&r[19]), "failed");
    assert_eq!(ok(&r[20]), &Value::Null);
    assert_eq!(codes_of(&r[21..23]), ["not_found", "unavailable"]);
    assert_eq!(ok(&r[25])["item"]["question"], "Empty id?");
    assert_eq!(rs["tasks"][0]["payload"], json!({ "question": "Q?", "options": ["a", "b"], "channelId": "600000000000000001", "maxChoices": 1, "duration": null, "allowedRoleIds": [], "deniedRoleIds": [], "hideResults": true }));
}

// ── achievements ──────────────────────────────────────────────────────────────

fn tier(goal: i64, extra: Value) -> Value {
    let mut t = json!({ "goal": goal, "roleAdd": null, "roleRemove": null, "xp": null, "coins": null });
    for (k, v) in extra.as_object().unwrap() {
        t[k] = v.clone();
    }
    t
}

fn ach(name: &str, tiers: Value, extra: Value) -> Value {
    let mut d = json!({ "description": null, "emoji": null, "trigger": "MESSAGES", "trackPastProgress": true, "deadline": null, "enabled": true, "name": name, "tiers": tiers });
    for (k, v) in extra.as_object().unwrap() {
        d[k] = v.clone();
    }
    d
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn achievements_both_ways() {
    let Some(t) = harness::app().await else { return };
    let now = "(now() AT TIME ZONE 'UTC')";
    let progress = format!(
        "INSERT INTO \"AchievementProgress\" (\"id\", \"guildId\", \"achievementId\", \"userId\", \"progress\", \"tier\", \"completedAt\", \"almostNotified\", \"updatedAt\") VALUES \
         ('p1$G', '$G', '$ID4', '{}', 20, 2, {now}, true, {now}), ('p2$G', '$G', '$ID4', '{}', 10, 1, NULL, true, {now}), ('p3$G', '$G', '$ID4', '{}', 0, 0, NULL, true, {now})",
        snow(11),
        snow(12),
        snow(13)
    );
    let query = "SELECT \"userId\", \"tier\", \"completedAt\" IS NOT NULL AS done, \"almostNotified\" AS almost FROM \"AchievementProgress\" WHERE \"achievementId\" = '$ID4' ORDER BY \"userId\"";
    let deadline = bot_db::format_js_date(&(chrono::Utc::now() + chrono::Duration::days(365)));
    let deadline = format!("{}.000Z", &deadline[..19]);
    let r1 = json!({ "roleAdd": snow(1) });
    let none = json!({});
    let ops = json!([
        { "op": "create", "store": "achievements", "data": ach("Xp", json!([tier(10, json!({ "xp": 5 }))]), json!({})) },
        { "op": "settings", "data": { "xpEnabled": true, "economyEnabled": false } },
        { "op": "create", "store": "achievements", "data": ach("Xp", json!([tier(10, json!({ "xp": 5 }))]), json!({})) },
        { "op": "create", "store": "achievements", "data": ach("Coins", json!([tier(10, json!({ "coins": 5 }))]), json!({})) },
        { "op": "create", "store": "achievements", "data": ach("Chatterbox", json!([tier(10, r1.clone()), tier(20, none.clone())]), json!({ "description": "Talk", "emoji": "💬" })) },
        { "op": "create", "store": "achievements", "data": ach("CHATTERBOX", json!([tier(1, none.clone())]), json!({})) },
        { "op": "create", "store": "achievements", "data": ach("Bad", json!([tier(5, none.clone()), tier(5, none.clone())]), json!({})) },
        { "op": "sql", "sql": progress },
        { "op": "get", "store": "achievements", "id": "$ID4" },
        { "op": "update", "store": "achievements", "id": "$ID4", "data": { "tiers": [tier(10, r1.clone()), tier(20, none.clone()), tier(30, none.clone())] } },
        { "op": "query", "sql": query },
        { "op": "update", "store": "achievements", "id": "$ID4", "data": { "tiers": [tier(15, r1.clone()), tier(20, none.clone()), tier(30, none.clone())] } },
        { "op": "query", "sql": query },
        { "op": "update", "store": "achievements", "id": "$ID4", "data": { "tiers": [tier(15, r1.clone())] } },
        { "op": "query", "sql": query },
        { "op": "update", "store": "achievements", "id": "$ID4", "data": { "name": "Talker", "enabled": false, "trackPastProgress": false, "deadline": deadline, "emoji": null } },
        { "op": "update", "store": "achievements", "id": "$ID4", "data": { "deadline": "2099-01-01T00:00:00.000Z" } },
        { "op": "update", "store": "achievements", "id": "$ID4", "data": { "name": "xp" } },
        { "op": "update", "store": "achievements", "id": "$ID4", "guild": "$O", "data": { "enabled": true } },
        { "op": "remove", "store": "achievements", "id": "$ID4", "guild": "$O" },
        { "op": "settings", "data": { "xpEnabled": false } },
        { "op": "update", "store": "achievements", "id": "$ID2", "data": { "tiers": [tier(10, json!({ "xp": 5 }))], "enabled": false } },
        { "op": "update", "store": "achievements", "id": "$ID2", "data": { "tiers": [tier(10, json!({ "xp": 6 }))] } },
        { "op": "cache", "key": "guild:$G:achievements" },
        { "op": "list", "store": "achievements" },
        { "op": "remove", "store": "achievements", "id": "$ID4" },
        { "op": "query", "sql": "SELECT COUNT(*)::int AS n FROM \"AchievementProgress\" WHERE \"achievementId\" = '$ID4'" },
        { "op": "update", "store": "achievements", "id": "nope", "data": { "tiers": [tier(1, none.clone())] } },
        { "op": "update", "store": "achievements", "id": "nope", "data": { "enabled": true } },
        { "op": "get", "store": "achievements", "id": "nope" },
        { "op": "remove", "store": "achievements", "id": "nope" },
        { "op": "create", "store": "achievements", "data": ach("   ", json!([tier(1, none.clone())]), json!({})) },
        { "op": "create", "store": "achievements", "data": ach("Big", json!([tier(1, none.clone()), tier(100_000_000, none.clone())]), json!({})) }
    ]);
    let rs = scenario(&t, 8, json!({ "ops": ops }), json!([]), Some("achievements")).await;
    let r = rs["results"].as_array().unwrap();
    assert_eq!(codes_of(&r[0..7]), ["invalid", "ok", "ok", "invalid", "ok", "conflict", "invalid"]);
    assert_eq!(ok(&r[2])["item"]["tiers"][0]["xp"], 5);
    let a = &ok(&r[4])["item"];
    assert_eq!(a["tiers"], json!([tier(10, r1.clone()), tier(20, none.clone())]));
    assert_eq!(a["triggerLabel"], "Messages sent");
    assert_eq!(ok(&r[8])["members"], 3);
    assert_eq!(ok(&r[9])["item"]["tiers"].as_array().unwrap().iter().map(|x| x["goal"].as_i64().unwrap()).collect::<Vec<_>>(), [10, 20, 30]);
    let p = |k: usize| ok(&r[k]).as_array().unwrap().iter().map(|x| json!([x["tier"], x["done"], x["almost"]])).collect::<Vec<_>>();
    assert_eq!(p(10)[0], json!([2, false, true]));
    assert_eq!(p(12)[2], json!([0, false, false]));
    assert_eq!(p(12)[1], json!([1, false, true]));
    assert_eq!(p(14), [json!([1, true, false]), json!([1, true, false]), json!([0, false, false])]);
    let e = &ok(&r[15])["item"];
    assert_eq!(json!([e["name"], e["enabled"], e["trackPastProgress"], e["deadline"], e["emoji"]]), json!(["Talker", false, false, deadline, null]));
    assert_eq!(codes_of(&r[16..20]), ["invalid", "conflict", "not_found", "not_found"]);
    assert_eq!(ok(&r[21])["item"]["enabled"], false);
    assert_eq!(code(&r[22]), "invalid");
    assert_eq!(ok(&r[23])["v"], 1);
    assert_eq!(ok(&r[26])[0]["n"], 0);
    assert_eq!(codes_of(&r[27..29]), ["not_found", "not_found"]);
    assert_eq!(ok(&r[29]), &Value::Null);
    assert_eq!(codes_of(&r[30..33]), ["not_found", "invalid", "invalid"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn achievements_cap_under_concurrency() {
    let Some(t) = harness::app().await else { return };
    let g = "940000990000000003";
    t.reset_guilds(&[g]).await;
    t.settings().ensure_guild_settings(g).await.unwrap();
    for i in 0..48 {
        sqlx::query(r#"INSERT INTO "Achievement" ("id", "guildId", "name", "trigger", "tiers", "updatedAt") VALUES ($1, $2, $3, 'MESSAGES', $4, now())"#)
            .bind(bot_db::new_cuid())
            .bind(g)
            .bind(format!("a{i}"))
            .bind(json!([{ "goal": 1, "rewards": [] }]))
            .execute(&t.pool)
            .await
            .unwrap();
    }
    let inputs = (0..5).map(|i| ach(&format!("new{i}"), json!([tier(1, json!({}))]), json!({}))).collect();
    let codes = concurrent_codes(&t, "achievements", g, inputs).await;
    assert_eq!(codes.iter().filter(|c| *c == "ok").count(), 2, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| *c == "limit").count(), 3, "{codes:?}");
    t.reset_guilds(&[g]).await;
}

// ── feeds ─────────────────────────────────────────────────────────────────────

const RSS: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Example &amp; Co</title><link>https://example.com/</link>
<item><title>Second</title><link>https://example.com/2</link><guid isPermaLink="false">post-2</guid><pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate></item>
<item><title>First</title><link>https://example.com/1</link><pubDate>Thu, 08 Oct 2026 10:00:00 GMT</pubDate></item></channel></rss>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feeds_both_ways() {
    let Some(t) = harness::app().await else { return };
    let (text, news, gone) = ("600000000000000001", "600000000000000002", "955017000000000888");
    let (quiet, ping) = ("610000000000000001", "610000000000000002");
    // Distinct creation times: the list orders by createdAt only, so ties would come back in any order.
    let bulk = format!(
        "INSERT INTO \"Feed\" (\"id\", \"guildId\", \"type\", \"source\", \"displayName\", \"fetchUrl\", \"channelId\", \"createdBy\", \"updatedAt\", \"createdAt\") \
         SELECT 'bulk' || i || '$G', '$G', 'RSS', 'https://bulk.test/' || i, 'b' || i, 'https://bulk.test/' || i, '{text}', '{BOB}', now(), \
         (now() AT TIME ZONE 'UTC') + i * interval '1 second' FROM generate_series(1, 29) AS i"
    );
    let ops = json!([
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "feed.test/$G/f.xml", "channelId": text, "messageTemplate": "New: {title}\\n{link}", "enabled": true } },
        { "op": "query", "sql": "SELECT \"initialized\", cardinality(\"seenIds\") AS seen, \"createdBy\", \"etag\", \"lastError\", \"lastSuccessAt\" IS NOT NULL AS ok, abs(extract(epoch from ((now() AT TIME ZONE 'UTC') - \"lastCheckedAt\"))) < 60 AS \"checkedNow\", abs(extract(epoch from ((now() AT TIME ZONE 'UTC') - \"lastSuccessAt\"))) < 60 AS \"succeededNow\" FROM \"Feed\" WHERE \"id\" = '$ID0'" },
        { "op": "job", "key": "feed.poll:$ID0" },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/f.xml", "channelId": text } },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "messageTemplate": null, "channelId": news, "pingRoleId": ping } },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "pingRoleId": quiet }, "ctx": { "member": { "owner": false, "permissions": "0" } } },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "includeReposts": true } },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "enabled": false } },
        { "op": "job", "key": "feed.poll:$ID0" },
        { "op": "sql", "sql": "UPDATE \"Feed\" SET \"failureCount\" = 7, \"lastError\" = 'HTTP 500' WHERE \"id\" = '$ID0'" },
        { "op": "get", "store": "feeds", "id": "$ID0" },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "enabled": true } },
        { "op": "query", "sql": "SELECT \"enabled\", \"failureCount\", \"lastError\", \"disabledReason\", \"initialized\" FROM \"Feed\" WHERE \"id\" = '$ID0'" },
        { "op": "job", "key": "feed.poll:$ID0" },
        { "op": "sql", "sql": format!("UPDATE \"Feed\" SET \"enabled\" = false, \"channelId\" = '{gone}' WHERE \"id\" = '$ID0'") },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "enabled": true } },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "channelId": text, "enabled": true } },
        { "op": "update", "store": "feeds", "id": "$ID0", "data": { "channelId": text, "messageTemplate": "  " } },
        { "op": "action", "store": "feeds", "id": "$ID0", "action": "test", "data": {} },
        { "op": "action", "store": "feeds", "id": "$ID0", "action": "test", "data": {} },
        { "op": "fetch", "status": 404 },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/missing.xml", "channelId": text } },
        { "op": "fetch", "status": 200 },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "not a url", "channelId": text } },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/r.xml", "channelId": text, "includeReposts": true } },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/q.xml", "channelId": text, "pingRoleId": quiet }, "ctx": { "member": { "owner": false, "permissions": "0" } } },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/q.xml", "channelId": text, "pingRoleId": ping }, "ctx": { "discord": false } },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/m.xml", "channelId": text, "messageTemplate": "x".repeat(1001) } },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/off.xml", "channelId": text, "enabled": false } },
        { "op": "job", "key": "feed.poll:$ID28" },
        { "op": "remove", "store": "feeds", "id": "$ID28" },
        { "op": "sql", "sql": bulk },
        { "op": "fetched" },
        { "op": "create", "store": "feeds", "data": { "type": "RSS", "source": "https://feed.test/$G/other.xml", "channelId": text } },
        { "op": "fetched" },
        { "op": "list", "store": "feeds" },
        { "op": "sql", "sql": "DELETE FROM \"Feed\" WHERE \"guildId\" = '$G' AND \"id\" <> '$ID0'" },
        { "op": "remove", "store": "feeds", "id": "$ID0" },
        { "op": "job", "key": "feed.poll:$ID0" },
        { "op": "remove", "store": "feeds", "id": "$ID0" },
        { "op": "get", "store": "feeds", "id": "$ID0" },
        { "op": "action", "store": "feeds", "id": "$ID0", "action": "test", "data": {} }
    ]);
    let spec = json!({ "discord": discord(), "fetch": { "status": 200, "body": RSS }, "ops": ops, "actor": actor(owner()) });
    let rs = scenario(&t, 9, spec, json!([]), Some("feeds")).await;
    let r = rs["results"].as_array().unwrap();
    let item = &ok(&r[0])["item"];
    assert_eq!(item["displayName"], "Example & Co");
    assert_eq!(item["messageTemplate"], "New: {title}\n{link}");
    assert!(item["sourceUrl"].as_str().unwrap().starts_with("https://feed.test/"));
    assert_eq!(ok(&r[1])[0]["initialized"], true);
    assert_eq!(ok(&r[1])[0]["seen"], 2);
    assert_eq!(ok(&r[1])[0]["createdBy"], BOB);
    assert_eq!((ok(&r[1])[0]["checkedNow"].as_bool(), ok(&r[1])[0]["succeededNow"].as_bool()), (Some(true), Some(true)), "UTC timestamps");
    assert_eq!(ok(&r[2])["type"], "feed.poll");
    assert!((ok(&r[2])["runInMs"].as_i64().unwrap() - 15 * 60_000).abs() < 60_000);
    assert_eq!(code(&r[3]), "conflict");
    let upd = &ok(&r[4])["item"];
    assert_eq!(json!([upd["messageTemplate"], upd["channelId"], upd["pingRoleId"]]), json!([null, news, ping]));
    assert_eq!(r[5]["err"]["path"], "pingRoleId");
    assert_eq!(r[6]["err"]["path"], "includeReposts");
    assert!(ok(&r[7])["item"]["statusText"].as_str().unwrap().contains("Paused by an admin"));
    assert_eq!(ok(&r[8]), &Value::Null);
    assert_eq!(ok(&r[12])[0], json!({ "enabled": true, "failureCount": 0, "lastError": null, "disabledReason": null, "initialized": false }));
    assert!(ok(&r[13])["runInMs"].as_i64().unwrap() < 30_000);
    assert_eq!(r[15]["err"]["path"], "channelId");
    assert!(ok(&r[18])["message"].as_str().unwrap().starts_with("The feed works: 2 item(s). Newest: \"Second\""));
    assert!(r[19]["err"]["message"].as_str().unwrap().contains("just checked"));
    assert_eq!(r[21]["err"]["path"], "source");
    assert!(r[21]["err"]["message"].as_str().unwrap().contains("HTTP 404"));
    assert_eq!(codes_of(&r[23..28]), ["invalid", "invalid", "invalid", "unavailable", "invalid"]);
    assert_eq!(ok(&r[28])["item"]["enabled"], false);
    assert_eq!(ok(&r[29]), &Value::Null);
    assert_eq!(code(&r[33]), "limit");
    assert_eq!(ok(&r[32]), ok(&r[34]), "the cap is checked before any request");
    assert_eq!(ok(&r[38]), &Value::Null, "remove cancels the poll job");
    assert_eq!(codes_of(&r[39..42]), ["not_found", "ok", "not_found"]);
    assert_eq!(ok(&r[40]), &Value::Null);
}

// ── embeds ────────────────────────────────────────────────────────────────────

const CH_TEXT: &str = "600000000000000001";
const CH_NEWS: &str = "600000000000000002";
const CH_HIDDEN: &str = "600000000000000004";
const ROLE_QUIET: &str = "610000000000000001";
const ROLE_PING: &str = "610000000000000002";

fn api_error(code: &str, status: u16, discord_code: i64) -> Value {
    json!({ "message": code, "status": status, "code": code, "discordCode": discord_code })
}

fn access(problems: Value) -> Value {
    json!({ "status": "done", "message": "ok", "data": { "problems": problems } })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embeds_both_ways() {
    let Some(t) = harness::app().await else { return };
    let staff = json!({ "owner": false, "permissions": "0" });
    let ctx_owner = json!({ "member": owner() });
    let tpl = json!({ "content": format!("Read the rules <@&{ROLE_QUIET}> <@&{ROLE_PING}>"), "embed": { "title": "{server} rules" }, "buttons": [] });
    let publish = |ch: &str, del: bool| json!({ "op": "action", "store": "embeds", "id": "$ID0", "action": "publish", "data": { "channelId": ch, "deletePrevious": del } });
    let automation = format!(
        "INSERT INTO \"Automation\" (\"id\", \"guildId\", \"name\", \"trigger\", \"actions\", \"createdBy\", \"updatedAt\") VALUES ('au$G', '$G', 'poster', 'MEMBER_JOINED', \
         '[{{\"type\":\"SEND_EMBED\",\"embedName\":\"auto-post\",\"channelId\":\"{CH_HIDDEN}\"}}]', '{BOB}', now())"
    );
    let hidden = json!([{ "index": 0, "text": "You need View Channel, Send Messages in #hidden." }]);
    let ops = json!([
        { "op": "create", "store": "embeds", "data": { "name": "rules", "template": tpl } },
        { "op": "create", "store": "embeds", "data": { "name": "rules", "template": tpl } },
        { "op": "list", "store": "embeds" },
        { "op": "get", "store": "embeds", "id": "$ID0", "guild": "$O" },
        { "op": "update", "store": "embeds", "id": "$ID0", "guild": "$O", "data": { "template": tpl } },
        { "op": "create", "store": "embeds", "data": { "name": "bad", "template": { "content": null, "embed": null, "buttons": [{ "label": "x", "url": "{user.avatar}" }] } } },
        { "op": "create", "store": "embeds", "data": { "name": "Bad Name", "template": { "content": "x" } } },
        { "op": "update", "store": "embeds", "id": "$ID0", "data": { "template": { "content": format!("Rules <@&{ROLE_QUIET}> <@&{ROLE_PING}>"), "embed": { "title": "{server} rules" }, "buttons": [] } } },
        { "op": "cache", "key": "guild:$G:embed:rules" },
        publish(CH_TEXT, true),
        { "op": "update", "store": "embeds", "id": "$ID0", "data": { "template": { "content": null, "embed": { "title": "New title" }, "buttons": [] } } },
        { "op": "fail", "method": "editMessage", "error": api_error("forbidden", 403, 50013) },
        { "op": "update", "store": "embeds", "id": "$ID0", "data": { "template": { "content": "never saved" } } },
        { "op": "get", "store": "embeds", "id": "$ID0" },
        publish(CH_NEWS, true),
        publish(CH_TEXT, false),
        { "op": "fail", "method": "sendMessage", "error": api_error("forbidden", 403, 50001) },
        publish(CH_NEWS, true),
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "refresh", "data": {} },
        { "op": "fail", "method": "editMessage", "error": api_error("not_found", 404, 10008) },
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "refresh", "data": {} },
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "refresh", "data": {} },
        publish(CH_TEXT, true),
        { "op": "fail", "method": "editMessage", "error": api_error("not_found", 404, 10003) },
        { "op": "update", "store": "embeds", "id": "$ID0", "data": { "template": { "content": "saved anyway" } } },
        publish(CH_NEWS, true),
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "unpublish", "data": {} },
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "unpublish", "data": {} },
        publish(CH_NEWS, true),
        { "op": "fail", "method": "deleteMessage", "error": api_error("not_found", 404, 10008) },
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "unpublish", "data": {} },
        publish("600000000000000999", true),
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "publish", "data": { "channelId": CH_TEXT }, "ctx": { "discord": false } },
        { "op": "answer", "kind": "access.channels", "result": access(hidden.clone()) },
        publish(CH_HIDDEN, true),
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "publish", "data": { "channelId": CH_TEXT, "deletePrevious": true }, "ctx": ctx_owner },
        { "op": "update", "store": "embeds", "id": "$ID0", "data": { "template": { "content": "hidden?" } } },
        { "op": "answer", "kind": "access.channels", "result": { "status": "pending", "message": "wait" } },
        publish(CH_TEXT, true),
        { "op": "action", "store": "embeds", "id": "$ID0", "action": "publish", "data": { "channelId": CH_TEXT }, "ctx": { "tasks": false } },
        { "op": "answer", "kind": "access.channels", "result": { "status": "failed", "message": "You are not a member of this server any more." } },
        publish(CH_TEXT, true),
        { "op": "answer", "kind": "access.channels", "result": access(json!([])) },
        { "op": "create", "store": "embeds", "data": { "name": "auto-post", "template": { "content": "x" } } },
        { "op": "sql", "sql": automation },
        { "op": "update", "store": "embeds", "id": "$ID43", "data": { "template": { "content": "y" } } },
        { "op": "answer", "kind": "access.channels", "result": access(hidden.clone()) },
        { "op": "update", "store": "embeds", "id": "$ID43", "data": { "template": { "content": "z" } } },
        { "op": "update", "store": "embeds", "id": "$ID43", "data": { "template": { "content": "z" } }, "ctx": ctx_owner },
        { "op": "update", "store": "embeds", "id": "$ID43", "data": {} },
        { "op": "remove", "store": "embeds", "id": "$ID0" },
        { "op": "remove", "store": "embeds", "id": "$ID0", "ctx": ctx_owner },
        { "op": "cache", "key": "guild:$G:embed:rules" },
        { "op": "remove", "store": "embeds", "id": "$ID0", "ctx": ctx_owner },
        { "op": "get", "store": "embeds", "id": "$ID0" },
        { "op": "list", "store": "embeds" },
        { "op": "answer", "kind": "access.channels", "result": access(json!([])) },
        { "op": "action", "store": "embeds", "id": "$ID43", "action": "publish", "data": { "channelId": CH_TEXT, "deletePrevious": true } },
        { "op": "answer", "kind": "access.channels", "result": access(hidden.clone()) },
        { "op": "action", "store": "embeds", "id": "$ID43", "action": "refresh", "data": {} },
        { "op": "action", "store": "embeds", "id": "$ID43", "action": "unpublish", "data": {} },
        { "op": "answer", "kind": "access.channels", "result": access(json!([{ "index": 1, "text": "You need View Channel in #general." }])) },
        { "op": "action", "store": "embeds", "id": "$ID43", "action": "publish", "data": { "channelId": CH_NEWS, "deletePrevious": true } },
        { "op": "answer", "kind": "access.channels", "result": access(json!([])) },
        { "op": "action", "store": "embeds", "id": "$ID43", "action": "publish", "data": { "channelId": CH_NEWS, "deletePrevious": false } }
    ]);
    let user = json!({ "id": ALICE, "username": "alice", "globalName": "Alice", "avatar": null });
    let spec = json!({
        "discord": discord(), "ops": ops, "tasks": { "access.channels": access(json!([])) },
        "actor": { "userId": ALICE, "user": user, "member": staff },
    });
    let rs = scenario(&t, 10, spec, json!([]), Some("embeds")).await;
    let r = rs["results"].as_array().unwrap();
    assert_eq!(ok(&r[0])["item"]["status"], "Draft");
    assert_eq!(codes_of(&r[1..2]), ["conflict"]);
    assert_eq!(ok(&r[2])["items"].as_array().unwrap().len(), 1);
    assert_eq!(ok(&r[3]), &Value::Null);
    assert_eq!(code(&r[4]), "not_found");
    assert_eq!(r[5]["err"]["path"], "template.buttons.0");
    assert_eq!(r[6]["err"]["path"], "name");
    assert_eq!(ok(&r[8])["content"], format!("Rules <@&{ROLE_QUIET}> <@&{ROLE_PING}>"));
    let posted = ok(&r[9]);
    assert_eq!(posted["item"]["status"], "Posted");
    assert_eq!(posted["item"]["messageId"], "900000000000000001");
    let msg = posted["message"].as_str().unwrap();
    assert!(msg.starts_with("Posted in #general. ") && msg.contains("Not pinged") && msg.contains("@Quiet"), "{msg}");
    let sent = rs["discord"].as_array().unwrap().iter().find(|c| c["method"] == "sendMessage").unwrap();
    assert_eq!(sent["args"]["payload"]["embeds"][0]["title"], "Test Guild rules");
    assert_eq!(sent["args"]["payload"]["allowed_mentions"], json!({ "parse": ["users"], "roles": [ROLE_PING] }));
    let edit = rs["discord"].as_array().unwrap().iter().find(|c| c["method"] == "editMessage").unwrap();
    assert_eq!(edit["args"]["payload"]["content"], "");
    assert_eq!(edit["args"]["payload"]["allowed_mentions"], json!({ "parse": [] }));
    assert!(r[12]["err"]["message"].as_str().unwrap().contains("edit its message in #general"));
    assert_eq!(ok(&r[13])["template"]["embed"]["title"], "New title");
    assert!(ok(&r[14])["message"].as_str().unwrap().contains("previous copy in #general was deleted"));
    assert!(ok(&r[15])["message"].as_str().unwrap().contains("previous copy in #news stays"));
    assert!(r[17]["err"]["message"].as_str().unwrap().contains("can't post in #news"));
    assert!(ok(&r[18])["message"].as_str().unwrap().contains("was updated"));
    assert_eq!(json!([ok(&r[20])["item"]["status"], ok(&r[20])["item"]["channelId"]]), json!(["Draft", CH_TEXT]));
    assert_eq!(code(&r[21]), "invalid");
    let gone = &ok(&r[24])["item"];
    assert_eq!(json!([gone["status"], gone["channelId"], gone["template"]["content"]]), json!(["Draft", null, "saved anyway"]));
    assert_eq!(json!([ok(&r[26])["item"]["status"], ok(&r[26])["item"]["channelId"]]), json!(["Draft", CH_NEWS]));
    assert_eq!(code(&r[27]), "invalid");
    assert_eq!(ok(&r[30])["item"]["status"], "Draft");
    assert_eq!(r[31]["err"]["path"], "channelId");
    assert_eq!(code(&r[32]), "unavailable");
    assert_eq!(r[34]["err"], json!({ "code": "invalid", "message": "You need View Channel, Send Messages in #hidden.", "path": "channelId" }));
    assert_eq!(ok(&r[35])["item"]["channelId"], CH_TEXT);
    assert_eq!(r[36]["err"]["path"], "template");
    assert_eq!(codes_of(&r[38..40]), ["unavailable", "unavailable"]);
    assert_eq!(code(&r[41]), "failed");
    assert_eq!(code(&r[45]), "ok");
    assert!(r[47]["err"]["message"].as_str().unwrap().starts_with("An automation posts this embed there, so editing it needs access to that channel. "));
    assert_eq!(code(&r[48]), "ok");
    assert_eq!(codes_of(&r[50..52]), ["invalid", "ok"]);
    assert_eq!(ok(&r[52]), &json!({ "__none": true }));
    assert_eq!(code(&r[53]), "not_found");
    assert_eq!(code(&r[57]), "ok");
    assert_eq!([r[59]["err"]["code"].clone(), r[60]["err"]["code"].clone()], [json!("invalid"), json!("invalid")], "refresh and unpublish of a copy in a hidden channel");
    assert_eq!(r[62]["err"]["path"], "deletePrevious");
    assert!(ok(&r[64])["message"].as_str().unwrap().contains("stays but is no longer updated"));
    let access_calls: Vec<&Value> = rs["tasks"].as_array().unwrap().iter().filter(|c| c["kind"] == "access.channels").collect();
    assert_eq!(access_calls[access_calls.len() - 2]["payload"]["checks"].as_array().unwrap().len(), 2, "moving checks the old channel too");
    assert_eq!(access_calls[access_calls.len() - 1]["payload"]["checks"].as_array().unwrap().len(), 1, "keeping the old copy checks only the target");
    // TTLs as embedService writes them: the template 300 s, the deletion marker 60 s; JS (guild A) and Rust (guild B) alike.
    let [ga, _, gb, _] = harness::guilds(10);
    for g in [&ga, &gb] {
        let ttl = harness::node("run", &json!({ "guild": g, "ops": [
            { "op": "ttl", "key": "guild:$G:embed:auto-post" }, { "op": "ttl", "key": "guild:$G:embed:rules" }
        ] }));
        let (tpl_ttl, none_ttl) = (ttl["results"][0]["ok"].as_i64().unwrap(), ttl["results"][1]["ok"].as_i64().unwrap());
        assert!((280..=300).contains(&tpl_ttl), "{g}: template TTL {tpl_ttl}");
        assert!((40..=60).contains(&none_ttl), "{g}: deletion marker TTL {none_ttl}");
    }
}

#[test]
fn render_for_discord_builds_the_allowlist_from_typed_roles_only() {
    use dash_records_a::embeds_store::{RenderMode, render_for_discord};
    let thread = "955060000000000005";
    let role = "955060000000000007";
    let resources: dash_collections::DiscordResources = serde_json::from_value(json!({
        "guild": { "id": "955060000000000001", "name": "Guild", "ownerId": "955060000000000002" },
        "channels": [{ "id": thread, "name": format!("<@&{role}>"), "type": 11, "kind": "text" }],
        "roles": [{ "id": role, "name": "secret", "mentionable": false, "everyone": false }],
    }))
    .unwrap();
    let owner = dash_collections::StoreCtx {
        user_id: "955060000000000003".into(),
        member: Some(dash_collections::DashboardMember { owner: true, permissions: "0".into(), highest_role_position: None }),
        ..Default::default()
    };
    let render = |content: &str, ctx: &dash_collections::StoreCtx, mode| render_for_discord(&json!({ "content": content }), &resources, thread, ctx, None, mode).unwrap();
    let (body, refused) = render("New in {channel.name}", &owner, RenderMode::Publish);
    assert!(body["allowed_mentions"].get("roles").is_none(), "no role allowed: {body}");
    assert!(refused.is_empty());
    assert!(body["content"].as_str().unwrap().starts_with("New in <"));
    let (body, _) = render(&format!("Ping <@&{role}> in {{channel.name}}"), &owner, RenderMode::Publish);
    assert_eq!(body["allowed_mentions"]["roles"], json!([role]), "a typed role works for someone who may");
    let staff = dash_collections::StoreCtx { member: Some(dash_collections::DashboardMember::default()), ..owner.clone() };
    let (body, refused) = render(&format!("Ping <@&{role}> in {{channel.name}}"), &staff, RenderMode::Publish);
    assert!(body["allowed_mentions"].get("roles").is_none());
    assert_eq!(refused, [role.to_string()], "refused for a publisher who may not ping it");
    let (body, _) = render("x", &owner, RenderMode::Edit);
    assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embeds_cap_holds() {
    let Some(t) = harness::app().await else { return };
    let g = "940000990000000004";
    t.reset_guilds(&[g]).await;
    let store = dash_records_a::store_for("embeds", &t).unwrap();
    let ctx = dash_collections::StoreCtx { guild_id: g.into(), user_id: BOB.into(), ..Default::default() };
    let data = |name: &str| json!({ "name": name, "template": { "content": "x" } }).as_object().unwrap().clone();
    store.create(g, data("first"), &ctx).await.unwrap();
    sqlx::query(
        r#"INSERT INTO "SavedEmbed" ("id", "guildId", "name", "template", "createdBy", "updatedAt")
           SELECT 'bulk' || i || $1, $1, 'bulk-' || i, '{"content":"x"}'::jsonb, $2, now() FROM generate_series(1, 499) AS i"#,
    )
    .bind(g)
    .bind(BOB)
    .execute(&t.pool)
    .await
    .unwrap();
    let err = store.create(g, data("one-more"), &ctx).await.unwrap_err();
    assert_eq!(err.as_record().map(|e| e.code.as_str()), Some("limit"));
    t.reset_guilds(&[g]).await;
}

// ── automations ───────────────────────────────────────────────────────────────

const A_USER: &str = "955030000000000010";
const A_OTHER: &str = "955030000000000011";
const A_BOT: &str = "955030000000000012";
const A_CH: &str = "955030000000000020";
const A_CH2: &str = "955030000000000021";
const A_FORUM: &str = "955030000000000023";
const A_HIDDEN: &str = "955030000000000024";
const A_ROLE: &str = "955030000000000030";
const A_ROLE2: &str = "955030000000000031";

fn cond(ty: &str, extra: Value) -> Value {
    let mut m = dash_collections::collections::automations::blank_condition(Some(&json!(ty)));
    m.extend(extra.as_object().unwrap().clone());
    Value::Object(m)
}

fn act(ty: &str, extra: Value) -> Value {
    let mut m = dash_collections::collections::automations::blank_action(Some(&json!(ty)));
    m.extend(extra.as_object().unwrap().clone());
    Value::Object(m)
}

fn button(label: &str, index: Value) -> Value {
    json!({ "kind": "automation", "label": label, "url": null, "style": "PRIMARY", "index": index })
}

fn automation_discord() -> Value {
    let ch = |id: &str, name: &str, ty: i64, kind: &str, pos: i64| json!({ "id": id, "name": name, "type": ty, "kind": kind, "parentId": null, "position": pos, "nsfw": false });
    json!({
        "resources": {
            "guild": { "id": "0", "name": "Test", "icon": null, "memberCount": 3, "ownerId": A_USER, "premiumTier": 0, "premiumSubscriptionCount": 0, "verificationLevel": 0 },
            "channels": [ch(A_CH, "general", 0, "text", 0), ch(A_CH2, "news", 5, "announcement", 1), ch("955030000000000022", "Cat", 4, "category", 2),
                         ch(A_FORUM, "forum", 15, "forum", 3), ch(A_HIDDEN, "hidden", 0, "text", 4)],
            "roles": [
                { "id": A_ROLE, "name": "Member", "color": null, "position": 2, "managed": false, "everyone": false, "assignable": true, "dangerous": false, "mentionable": false },
                { "id": A_ROLE2, "name": "Mod", "color": null, "position": 3, "managed": false, "everyone": false, "assignable": true, "dangerous": true, "mentionable": false }
            ],
            "emojis": [],
            "bot": { "userId": "955030000000000099", "topRolePosition": 10, "manageRoles": true, "createInvite": false }
        },
        "users": [user(A_USER, "alice", Some("Alice"), false), user(A_OTHER, "u11", None, false), user(A_BOT, "u12", None, true)],
        "members": {}
    })
}

fn saved_embed_sql(id: &str, name: &str, content: &str) -> String {
    format!(
        "INSERT INTO \"SavedEmbed\" (\"id\", \"guildId\", \"name\", \"template\", \"createdBy\", \"updatedAt\") VALUES ('{id}$G', '$G', '{name}', '{{\"content\":\"{content}\"}}', '{A_USER}', now())"
    )
}

fn aop(op: &str, data: Value) -> Value {
    json!({ "op": op, "store": "automations", "data": data, "validate": true })
}

fn aup(id: &str, data: Value) -> Value {
    json!({ "op": "update", "store": "automations", "id": id, "data": data, "validate": true })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automations_both_ways() {
    let Some(t) = harness::app().await else { return };
    let non_owner = json!({ "userId": A_OTHER, "member": { "owner": false, "permissions": "0" } });
    let verify = json!({
        "name": "verify", "trigger": "BUTTON_CLICKED",
        "conditions": [cond("BUTTON_IS", json!({ "buttons": [{ "automation": "verify", "index": 2 }] }))],
        "actions": [act("SEND_MESSAGE", json!({ "content": "Click", "channelId": A_CH, "button": button("One", Value::Null) })),
                    act("REPLY", json!({ "content": "Thanks", "button": button("Two", Value::Null) })),
                    act("GIVE_ROLES", json!({ "roleIds": [A_ROLE] })), act("SEND_EMBED", json!({ "embedName": "rules" }))],
    });
    let bad_ref = |automation: &str, index: i64| json!({
        "name": if automation == "bad-self" { "bad-self" } else { "bad-ref" }, "trigger": "BUTTON_CLICKED",
        "conditions": [cond("BUTTON_IS", json!({ "buttons": [{ "automation": automation, "index": index }] }))],
        "actions": [act("REPLY", json!({ "content": "x" }))],
    });
    let raw_actions = r#"[{"type":"REPLY","content":"ok","button":null},{"type":"NOPE"}]"#;
    let review_actions = format!(
        r#"[{{"type":"GIVE_ROLES","roleIds":["{A_ROLE2}","955030000000000039"]}},{{"type":"SEND_MESSAGE","content":"x","channelId":"955030000000000029","button":null}}]"#
    );
    let ops = json!([
        { "op": "settings", "data": { "xpEnabled": false, "economyEnabled": true } },
        { "op": "sql", "sql": saved_embed_sql("se1", "rules", "Rules") },
        aop("create", verify),
        { "op": "query", "sql": "SELECT \"conditions\", \"buttonSeq\", \"createdBy\" FROM \"Automation\" WHERE \"id\" = '$ID2'" },
        { "op": "get", "store": "automations", "id": "$ID2" },
        { "op": "get", "store": "automations", "id": "$ID2", "ctx": { "discord": false } },
        aop("create", json!({ "name": "listener", "trigger": "BUTTON_CLICKED", "conditions": [cond("BUTTON_IS", json!({ "buttons": [{ "automation": "verify", "index": 1 }] }))], "actions": [act("REPLY", json!({ "content": "ok" }))] })),
        aop("create", bad_ref("verify", 9)),
        aop("create", bad_ref("nobody", 1)),
        aop("create", bad_ref("bad-self", 1)),
        aop("create", json!({ "name": "e", "actions": [act("SEND_EMBED", json!({ "embedName": "nope" }))] })),
        { "op": "sql", "sql": saved_embed_sql("se2", "pinger", &format!("<@&{A_ROLE}>")) },
        { "op": "create", "store": "automations", "validate": true, "data": { "name": "e", "actions": [act("SEND_EMBED", json!({ "embedName": "pinger" }))] }, "ctx": non_owner },
        aop("create", json!({ "name": "e", "actions": [act("GIVE_XP", json!({ "amount": 5 }))] })),
        aop("create", json!({ "name": "coins", "actions": [act("GIVE_COINS", json!({ "amount": 5 }))] })),
        aop("create", json!({ "name": "e", "trigger": "THREAD_CREATED", "actions": [act("ADD_TO_THREAD", json!({ "users": [{ "userId": A_BOT }] }))] })),
        aop("create", json!({ "name": "e", "conditions": [cond("USER_IS", json!({ "users": [{ "userId": "955030000000000077" }] }))], "actions": [act("PIN", json!({}))] })),
        aop("create", json!({ "name": "verify", "actions": [act("PIN", json!({}))] })),
        aup("$ID2", json!({ "name": "verify-2", "actions": [act("REPLY", json!({ "content": "Thanks", "button": button("Two", json!(2)) })), act("SEND_MESSAGE", json!({ "content": "New", "button": button("Three", Value::Null) }))] })),
        { "op": "query", "sql": "SELECT \"buttonSeq\", \"conditions\" FROM \"Automation\" WHERE \"id\" = '$ID2'" },
        { "op": "get", "store": "automations", "id": "$ID6" },
        aup("$ID6", json!({ "cooldownSeconds": 30, "conditions": [cond("BUTTON_IS", json!({ "buttons": [{ "automation": "verify-2", "index": 1 }] }))] })),
        aup("$ID2", json!({ "actions": [act("REPLY", json!({ "content": "x", "button": button("X", json!(1)) }))] })),
        { "op": "update", "store": "automations", "id": "$ID2", "data": { "actions": [act("REPLY", json!({ "content": "x", "button": button("X", json!(1)) }))] } },
        aup("$ID2", json!({ "actions": [] })),
        { "op": "update", "store": "automations", "id": "$ID2", "data": { "enabled": true } },
        aop("create", json!({ "name": "embed-user", "actions": [act("SEND_EMBED", json!({ "embedName": "rules", "channelId": A_CH }))] })),
        { "op": "sql", "sql": "DELETE FROM \"SavedEmbed\" WHERE \"guildId\" = '$G' AND \"name\" = 'rules'" },
        { "op": "get", "store": "automations", "id": "$ID26" },
        aup("$ID26", json!({ "cooldownSeconds": 5, "actions": [act("SEND_EMBED", json!({ "embedName": "rules", "channelId": A_CH }))] })),
        { "op": "sql", "sql": format!("UPDATE \"Automation\" SET \"actions\" = '{review_actions}' WHERE \"id\" = '$ID26'") },
        { "op": "action", "store": "automations", "id": "$ID26", "action": "review", "data": {} },
        { "op": "action", "store": "automations", "id": "$ID6", "action": "review", "data": {} },
        { "op": "sql", "sql": format!("UPDATE \"Automation\" SET \"actions\" = '{raw_actions}' WHERE \"id\" = '$ID6'") },
        { "op": "get", "store": "automations", "id": "$ID6" },
        aup("$ID6", json!({ "cooldownSeconds": 60 })),
        { "op": "query", "sql": "SELECT \"actions\" FROM \"Automation\" WHERE \"id\" = '$ID6'" },
        { "op": "cacheSet", "key": "guild:$G:automations", "value": { "v": 1, "byTrigger": {} }, "ttl": 60 },
        aup("$ID6", json!({ "cooldownSeconds": 61 })),
        { "op": "cache", "key": "guild:$G:automations" },
        { "op": "list", "store": "automations" },
        { "op": "get", "store": "automations", "id": "$ID2", "guild": "$O" },
        { "op": "update", "store": "automations", "id": "$ID2", "guild": "$O", "data": { "cooldownSeconds": 1 } },
        { "op": "remove", "store": "automations", "id": "$ID2", "guild": "$O" },
        { "op": "remove", "store": "automations", "id": "$ID2" },
        { "op": "get", "store": "automations", "id": "$ID2" },
        { "op": "remove", "store": "automations", "id": "$ID2" },
        { "op": "action", "store": "automations", "id": "$ID2", "action": "review", "data": {} },
        { "op": "action", "store": "automations", "id": "$ID6", "action": "review", "data": {}, "ctx": { "discord": false } }
    ]);
    let spec = json!({
        "discord": automation_discord(), "ops": ops, "tasks": { "access.channels": access(json!([])) },
        "actor": { "userId": A_USER, "user": null, "member": { "owner": true, "permissions": "8" } },
    });
    let rs = scenario(&t, 11, spec, json!([]), Some("automations")).await;
    let r = rs["results"].as_array().unwrap();
    let first = &ok(&r[2])["item"];
    assert_eq!(first["name"], "verify");
    assert_eq!(first["actions"].as_array().unwrap().iter().map(|a| a["button"]["index"].clone()).collect::<Vec<_>>(), [json!(1), json!(2), Value::Null, Value::Null]);
    assert_eq!(first["conditions"][0]["buttons"], json!([{ "automation": "verify", "index": 2 }]));
    assert_eq!(first["createdBy"], "Alice (@alice)");
    assert_eq!(first["problems"], json!([]));
    assert_eq!(first["summary"], "Button clicked · 1 condition · 4 actions");
    let row = &ok(&r[3])[0];
    assert_eq!(row["conditions"][0]["buttons"][0]["automationId"], first["id"]);
    assert_eq!((row["buttonSeq"].as_i64(), row["createdBy"].as_str()), (Some(2), Some(A_USER)));
    assert_eq!(ok(&r[5])["createdBy"], A_USER);
    assert_eq!(ok(&r[6])["item"]["conditions"][0]["buttons"], json!([{ "automation": "verify", "index": 1 }]));
    let paths: Vec<Value> = r[7..13].iter().map(|x| x["err"]["path"].clone()).collect();
    assert_eq!(
        paths[..4],
        [json!("conditions.0.buttons.0.index"), json!("conditions.0.buttons.0.automation"), json!("conditions.0.buttons.0.index"), json!("actions.0.embedName")]
    );
    assert_eq!(r[12]["err"]["path"], "actions.0.embedName");
    assert_eq!(r[13]["err"]["path"], "actions.0.type");
    assert_eq!(ok(&r[14])["item"]["actions"][0]["amount"], 5);
    assert_eq!(r[15]["err"]["path"], "actions.0.users.0.userId");
    assert_eq!(r[16]["err"]["path"], "conditions.0.users.0.userId");
    assert_eq!(json!([code(&r[17]), r[17]["err"]["path"].clone()]), json!(["conflict", "name"]));
    let edited = &ok(&r[18])["item"];
    assert_eq!(edited["name"], "verify-2");
    assert_eq!(edited["actions"].as_array().unwrap().iter().map(|a| a["button"]["index"].clone()).collect::<Vec<_>>(), [json!(2), json!(3)]);
    assert_eq!(edited["conditions"][0]["buttons"], json!([{ "automation": "verify-2", "index": 2 }]));
    assert_eq!(ok(&r[19])[0]["buttonSeq"], 3);
    assert_eq!(ok(&r[20])["problems"], json!(["Condition 1: button 1 no longer exists on \"verify-2\"."]));
    assert_eq!(ok(&r[21])["item"]["cooldownSeconds"], 30);
    assert_eq!(r[22]["err"]["field"], "actions.0.button.index");
    assert_eq!(json!([code(&r[23]), r[23]["err"]["path"].clone()]), json!(["invalid", "actions.0.button.index"]));
    assert_eq!(ok(&r[24])["item"]["enabled"], false);
    assert_eq!(r[25]["err"]["path"], "enabled");
    assert_eq!(ok(&r[28])["problems"], json!(["Action 1: the saved embed \"rules\" was not found."]));
    assert_eq!(ok(&r[29])["item"]["cooldownSeconds"], 5);
    let review = ok(&r[31]);
    assert!(review["message"].as_str().unwrap().starts_with("3 problems: "), "{review}");
    assert_eq!(ok(&r[32])["message"], "1 problem: Condition 1: button 1 no longer exists on \"verify-2\".");
    assert_eq!(ok(&r[34])["problems"][0], "Action 2 is invalid and ignored: Unknown action type.");
    assert_eq!(ok(&r[35])["item"]["cooldownSeconds"], 60);
    assert_eq!(ok(&r[36])[0]["actions"], serde_json::from_str::<Value>(raw_actions).unwrap());
    assert_eq!(ok(&r[39]), &Value::Null, "every write drops the bot's automation cache");
    let names: Vec<&str> = ok(&r[40])["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["verify-2", "listener", "coins", "embed-user"]);
    assert_eq!(ok(&r[41]), &Value::Null);
    assert_eq!(codes_of(&r[42..44]), ["not_found", "not_found"]);
    assert_eq!(ok(&r[45]), &Value::Null);
    assert_eq!(codes_of(&r[46..49]), ["not_found", "not_found", "unavailable"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automations_presets_pings_access_cap() {
    let Some(t) = harness::app().await else { return };
    let non_owner = json!({ "userId": A_OTHER, "member": { "owner": false, "permissions": "0" } });
    let fillers = format!(
        "INSERT INTO \"Automation\" (\"id\", \"guildId\", \"name\", \"trigger\", \"createdBy\", \"updatedAt\", \"createdAt\") SELECT 'f' || i || '$G', '$G', 'filler-' || i, 'MESSAGE_SENT', '{A_USER}', now(), (now() AT TIME ZONE 'UTC') + i * interval '1 second' FROM generate_series(1, 44) AS i"
    );
    let hidden = json!([{ "index": 0, "text": "You need View Channel, Send Messages in #hidden." }]);
    let ops = json!([
        aop("create", json!({ "name": "welcome", "preset": "welcomer", "presetChannelId": A_CH, "presetRoleId": A_ROLE })),
        aop("create", json!({ "name": "w2", "preset": "welcomer", "presetChannelId": A_FORUM })),
        aop("create", json!({ "name": "stars", "preset": "starboard", "presetChannelId": A_CH2, "presetThreshold": 4 })),
        aop("create", json!({ "name": "joiner", "preset": "thread-joiner", "presetUserId": A_OTHER, "presetChannelId": A_FORUM })),
        aop("create", json!({ "name": "j2", "preset": "thread-joiner", "presetUserId": A_BOT })),
        aop("create", json!({ "name": "j2", "preset": "thread-joiner", "presetUserId": "955030000000000088" })),
        { "op": "settings", "data": { "customPrefix": "?", "modRoleIds": [A_ROLE2] } },
        aop("create", json!({ "name": "clean", "preset": "commands-only", "presetChannelId": A_CH, "presetStaffRoleId": A_ROLE })),
        { "op": "query", "sql": "SELECT \"conditions\" FROM \"Automation\" WHERE \"guildId\" = '$G' AND \"name\" = 'clean'" },
        aop("create", json!({ "name": "c2", "preset": "commands-only", "presetChannelId": A_FORUM })),
        { "op": "create", "store": "automations", "validate": true, "data": { "name": "w3", "preset": "welcomer", "presetChannelId": A_CH, "presetMessage": format!("Hi <@&{A_ROLE}>") }, "ctx": non_owner },
        aop("create", json!({ "name": "w3", "preset": "welcomer", "presetChannelId": A_CH, "presetMessage": format!("Hi <@&{A_ROLE}>") })),
        { "op": "create", "store": "automations", "validate": true, "data": { "name": "pinger", "actions": [act("SEND_MESSAGE", json!({ "content": format!("<@&{A_ROLE}> hi"), "channelId": A_CH }))] }, "ctx": non_owner },
        { "op": "answer", "kind": "access.channels", "result": access(hidden.clone()) },
        { "op": "create", "store": "automations", "validate": true, "data": { "name": "poster", "actions": [act("SEND_MESSAGE", json!({ "content": "x", "channelId": A_HIDDEN }))] }, "ctx": non_owner },
        { "op": "create", "store": "automations", "validate": true, "data": { "name": "replier", "actions": [act("REPLY", json!({ "content": "x" }))] }, "ctx": non_owner },
        aop("create", json!({ "name": "poster", "actions": [act("SEND_MESSAGE", json!({ "content": "x", "channelId": A_HIDDEN }))] })),
        { "op": "update", "store": "automations", "id": "$ID16", "validate": true, "data": { "cooldownSeconds": 9, "actions": [act("SEND_MESSAGE", json!({ "content": "x", "channelId": A_HIDDEN }))] }, "ctx": non_owner },
        { "op": "update", "store": "automations", "id": "$ID16", "validate": true, "data": { "actions": [act("SEND_MESSAGE", json!({ "content": "changed", "channelId": A_HIDDEN }))] }, "ctx": non_owner },
        { "op": "answer", "kind": "access.channels", "result": access(json!([])) },
        { "op": "sql", "sql": fillers },
        aop("create", json!({ "name": "one-too-many", "actions": [act("PIN", json!({}))] })),
        { "op": "list", "store": "automations" },
        { "op": "settings", "data": { "customPrefix": "!", "modRoleIds": [] } }
    ]);
    let spec = json!({
        "discord": automation_discord(), "ops": ops, "tasks": { "access.channels": access(json!([])) },
        "actor": { "userId": A_USER, "user": null, "member": { "owner": true, "permissions": "8" } },
    });
    let rs = scenario(&t, 12, spec, json!([]), Some("automations")).await;
    let r = rs["results"].as_array().unwrap();
    let welcome = &ok(&r[0])["item"];
    assert_eq!(welcome["trigger"], "MEMBER_JOINED");
    assert_eq!(welcome["actions"].as_array().unwrap().iter().map(|a| a["type"].clone()).collect::<Vec<_>>(), [json!("SEND_MESSAGE"), json!("GIVE_ROLES")]);
    assert_eq!(welcome["actions"][0]["content"], "Hey {user}, welcome to **{server}**!");
    assert_eq!((welcome["enabled"].as_bool(), welcome["preset"].as_str()), (Some(true), Some("none")));
    assert_eq!(r[1]["err"]["path"], "presetChannelId");
    let stars = &ok(&r[2])["item"];
    assert_eq!(stars["trigger"], "REACTION_ADDED");
    assert_eq!(stars["conditions"][1]["min"], 4);
    let joiner = &ok(&r[3])["item"];
    assert_eq!(joiner["actions"][0]["users"], json!([{ "userId": A_OTHER }]));
    assert_eq!([r[4]["err"]["path"].clone(), r[5]["err"]["path"].clone()], [json!("presetUserId"), json!("presetUserId")]);
    let stored = &ok(&r[8])[0]["conditions"];
    assert_eq!(stored[1]["pattern"], "^(?!\\?)");
    assert_eq!(stored[2]["roleIds"], json!([A_ROLE, A_ROLE2]));
    assert_eq!(r[9]["err"]["path"], "presetChannelId");
    assert_eq!(r[10]["err"]["path"], "presetMessage");
    assert_eq!(code(&r[11]), "ok", "the owner may ping any role");
    assert_eq!(r[12]["err"]["path"], "actions.0.content");
    assert_eq!(r[14]["err"], json!({ "code": "invalid", "message": "You need View Channel, Send Messages in #hidden.", "path": "actions.0.channelId" }));
    assert_eq!(code(&r[15]), "ok", "a reply has no fixed channel");
    assert_eq!(code(&r[17]), "ok", "an edit keeps a send action exactly as it was");
    assert_eq!(r[18]["err"]["path"], "actions.0.channelId", "a changed action is checked again");
    assert_eq!(code(&r[21]), "limit");
    assert_eq!(ok(&r[22])["items"].as_array().unwrap().len(), 50);
}

// ── registry, raw SQL, pure helpers ───────────────────────────────────────────

#[test]
fn raw_sql_matches_the_js_statements() {
    let golden = bot_core::testing::load_golden("dash-records-a/raw-sql.json");
    let rust = dash_records_a::raw_sql();
    assert_eq!(golden.as_array().unwrap().len(), rust.len());
    for g in golden.as_array().unwrap() {
        let (_, text, values) = rust.iter().find(|(n, _, _)| *n == g["name"].as_str().unwrap()).unwrap_or_else(|| panic!("{} is ported", g["name"]));
        assert_eq!(text, g["text"].as_str().unwrap(), "{}", g["name"]);
        assert_eq!(&json!(values), &g["values"], "{}", g["name"]);
    }
    assert!(dash_records_a::polls_store::voters_sql(3).contains("IN ($1,$2,$3)"), "Prisma.join separates with a comma");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_sql_runs_in_a_rolled_back_transaction() {
    let Some(t) = harness::app().await else { return };
    for (name, sql, params) in dash_records_a::raw_sql() {
        let mut tx = t.pool.begin().await.unwrap();
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for p in &params {
            q = q.bind(p);
        }
        q.fetch_all(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register_stores_fills_its_keys() {
    let Some(t) = harness::app().await else { return };
    let mut reg = dash_collections::StoreRegistry::new();
    dash_records_a::register_stores(&mut reg, &t).unwrap();
    for key in dash_records_a::STORE_KEYS {
        assert!(reg.store(key).is_ok(), "{key}");
    }
    let missing = reg.missing();
    assert_eq!(missing.len(), 22 - dash_records_a::STORE_KEYS.len(), "{missing:?}");
    assert!(dash_records_a::STORE_KEYS.iter().all(|k| !missing.contains(&k.to_string())));
    // The actions each store answers are exactly the collection's actions.
    for key in dash_records_a::STORE_KEYS {
        let mut ours: Vec<&str> = reg.store(key).unwrap().action_ids().to_vec();
        let mut theirs: Vec<String> = reg.collection(key).unwrap().actions.iter().map(|a| a.id.clone()).collect();
        ours.sort_unstable();
        theirs.sort();
        assert_eq!(ours, theirs, "{key}");
    }
    // An unknown action of a store answers 'Unknown action.'.
    let ctx = dash_collections::StoreCtx { guild_id: "940000990000000005".into(), ..Default::default() };
    let err = reg.store("levels").unwrap().action("940000990000000005", ALICE, "nope", serde_json::Map::new(), &ctx).await.unwrap_err();
    assert_eq!(err.as_record().unwrap().message, "Unknown action.");
}

#[test]
fn pure_helpers_match_the_js() {
    use dash_records_a::achievements_store::{tier_from_wire, tier_to_wire};
    use dash_records_a::cases_store::case_id_of;
    use dash_records_a::giveaways_store::{is_item_id, read_history_cursor, start_payload};
    // cases.store.js caseIdOf
    assert_eq!(case_id_of("12"), Some(12));
    assert_eq!(case_id_of("0"), None);
    assert_eq!(case_id_of("2147483648"), None);
    assert_eq!(case_id_of("12345678901"), None);
    assert_eq!(case_id_of("#1"), None);
    // giveaways/polls ID_RE and CURSOR_RE
    assert!(is_item_id("cabc_1-2") && !is_item_id("bad id!") && !is_item_id(""));
    assert_eq!(read_history_cursor("1700000000000:abc"), Some((1_700_000_000_000, "abc".into())));
    assert_eq!(read_history_cursor("1:a_b"), None);
    assert_eq!(read_history_cursor("1234567890123456:a"), None);
    // startPayload: undefined required keys are left out, the rest default
    assert_eq!(
        start_payload(json!({ "prize": "Nitro" }).as_object().unwrap()),
        json!({ "prize": "Nitro", "description": null, "duration": null, "endsAt": null, "requiredRoleIds": [], "deniedRoleIds": [],
                "roleWeights": [], "xpPrize": null, "coinPrize": null, "pingRoleId": null, "embedColor": null })
    );
    // achievements.store.js tierFromWire / tierToWire
    let wire = json!({ "goal": 10, "roleAdd": snow(1), "roleRemove": snow(1), "xp": 0, "coins": 5 });
    assert_eq!(tier_from_wire(&wire), json!({ "goal": 10, "rewards": [{ "type": "ROLE_ADD", "roleId": snow(1) }, { "type": "COINS", "amount": 5 }] }));
    let tiers = feat_achievements::achievement_service::normalize_tiers(&json!([tier_from_wire(&wire)]));
    assert_eq!(tier_to_wire(&tiers[0]), json!({ "goal": 10, "roleAdd": snow(1), "roleRemove": null, "xp": null, "coins": 5 }));
}

// ── what a store returns is accepted back as an update (dashboard-collections-more.test.js) ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_store_returns_is_accepted_back_as_an_update() {
    let Some(t) = harness::app().await else { return };
    let g = "940000990000000006";
    t.reset_guilds(&[g]).await;
    let deadline = bot_db::format_js_date(&(chrono::Utc::now() + chrono::Duration::days(1)));
    let deadline = format!("{}.000Z", &deadline[..19]);
    let made = [
        ("levelRoles", json!({ "level": 3, "roleId": snow(1) })),
        ("economyBoosts", json!({ "name": "Boost", "percent": 25, "period": "WEEKLY", "roleId": snow(3), "enabled": false })),
        (
            "achievements",
            json!({ "name": "Talker", "description": "Talk", "emoji": "💬", "trigger": "REACTIONS", "trackPastProgress": false, "deadline": deadline,
                    "enabled": true, "tiers": [tier(5, json!({ "roleAdd": snow(4) })), tier(50, json!({ "roleRemove": snow(5) }))] }),
        ),
    ];
    let ctx = dash_collections::StoreCtx { guild_id: g.into(), user_id: "700000000000000001".into(), ..Default::default() };
    let read = dash_collections::ReadCtx { guild_id: g.into(), discord: None };
    for (key, data) in made {
        let store = dash_records_a::store_for(key, &t).unwrap();
        let created = store.create(g, data.as_object().unwrap().clone(), &ctx).await.unwrap_or_else(|e| panic!("{key}: {e:?}"));
        let item = store.get(g, created.item["id"].as_str().unwrap(), &read).await.unwrap().unwrap();
        let c = dash_collections::Collections::builtin().get(key).unwrap().clone();
        let form: serde_json::Map<String, Value> =
            c.fields.iter().filter(|f| !c.create_only.iter().any(|k| k == f.field())).map(|f| (f.field().to_string(), item[f.field()].clone())).collect();
        let now = chrono::Utc::now().timestamp_millis() as f64;
        let valid = dash_collections::validate_record_input(key, &Value::Object(form.clone()), dash_collections::Mode::Update, None, Some(&item), now)
            .unwrap_or_else(|e| panic!("{key}: {e:?}"));
        assert_eq!(valid.data, form, "{key}: the validated form equals what was sent");
        let saved = store.update(g, item["id"].as_str().unwrap(), valid.data, &ctx).await.unwrap_or_else(|e| panic!("{key}: {e:?}"));
        assert_eq!(saved.item, item, "{key}: saving an unchanged form changes nothing");
        for f in &c.read_only {
            assert!(item.get(f.field()).is_some(), "{key}: item has read-only {}", f.field());
        }
        let listed = store.list(g, &dash_collections::ListOpts::default(), &read).await.unwrap();
        assert!(listed.items.iter().any(|x| x["id"] == item["id"]), "{key}: listed");
    }
    t.reset_guilds(&[g]).await;
}
