//! Port of `test/giveaways.test.js` and `test/polls.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: the pure helpers against JS-produced fixtures (`rust/fixtures/golden/feat-giveaways-polls/golden.json`,
//! from `rust/fixtures/gen/feat-giveaways-polls-golden.mjs`), the command payloads and plugin declarations, the render
//! throttle, the pending forms, and the command, button and modal paths that answer before any database access.
//! With `TEST_DATABASE_URL` / `TEST_REDIS_URL`: every flow against the real tables through a recording Discord seam,
//! and both directions of the JS round trip (`rust/fixtures/gen/feat-giveaways-polls-js.mjs`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::command::option_type;
use bot_core::clock::Clock;
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, load_golden, offline_discord_as, run_node};
use bot_db::{Giveaway, GiveawayStatus, GuildSettings, Poll, PollStatus};
use feat_giveaways_polls::discord_ops::{DiscordError, DiscordOps, SentMessage, set_discord_ops};
use feat_giveaways_polls::giveaway_service as gs;
use feat_giveaways_polls::poll_service as ps;
use serde_json::{Value, json};
use serenity::all::{GuildChannel, Member, Permissions as P};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_giveaways_polls::register(&mut reg);
    reg
}

fn golden() -> Value {
    load_golden("feat-giveaways-polls/golden.json")
}

fn settings_row(fields: &Value) -> GuildSettings {
    let map = fields.as_object().cloned().unwrap_or_default();
    bot_core::settings::MemorySettingsStore::new_row("900000000000000777", map).expect("settings row")
}

fn settings_of(v: &Value) -> Option<GuildSettings> {
    (!v.is_null()).then(|| settings_row(v))
}

/// discord.js `toJSON()` / `resolveBody()` and serenity serialisation agree up to defaults: drop `null` values and
/// the falsy defaults discord.js writes (`tts`, `enforce_nonce`, an emoji's `animated`), and serenity's embed type.
fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                // serenity writes an embed's `type: "rich"` (Discord's default for bot embeds).
                if x.is_null() || ((k == "tts" || k == "enforce_nonce" || k == "animated") && x == &json!(false)) || (k == "type" && x == "rich") {
                    continue;
                }
                // An embed timestamp is the same instant whether written with or without milliseconds.
                if k == "timestamp"
                    && let Some(t) = x.as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                {
                    out.insert(k.clone(), json!(bot_db::format_js_date(&t.with_timezone(&chrono::Utc))));
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

fn without_timestamp(v: &Value) -> Value {
    let mut v = norm(v);
    if let Some(m) = v.as_object_mut() {
        m.remove("timestamp");
    }
    v
}

fn to_json<T: serde::Serialize>(x: &T) -> Value {
    norm(&serde_json::to_value(x).expect("serializable"))
}

fn assert_same(ours: &Value, js: &Value, what: &str) {
    if let Some(d) = bot_core::testing::json_diff(&norm(ours), &norm(js), false) {
        panic!("{what}: differs from the JS: {d}\n ours: {ours}\n   js: {js}");
    }
}

fn giveaway_of(v: &Value) -> Giveaway {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("giveaway row {v}: {e}"))
}

fn poll_of(v: &Value) -> Poll {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("poll row {v}: {e}"))
}

fn dt(s: &str) -> chrono::DateTime<chrono::Utc> {
    bot_db::parse_js_date(s).expect("date")
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["giveaway", "poll"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "giveaways");
    bot_core::testing::assert_plugin_matches_golden(&reg, "polls");
}

#[test]
fn hook_order_and_job_types() {
    for (event, id) in [("messageDelete", "giveaways"), ("messageDeleteBulk", "giveaways"), ("messageDelete", "polls"), ("messageDeleteBulk", "polls"), ("channelDelete", "polls")] {
        assert!(bot_config::hook_index(event, id).is_some(), "{event}:{id} is in HOOK_ORDER");
    }
    let g = golden();
    assert_eq!(gs::END_JOB_TYPE, g["giveaway"]["customIds"]["endJobType"]);
    assert_eq!(JobType::GiveawayEnd.as_str(), gs::END_JOB_TYPE);
    assert_eq!(ps::END_JOB_TYPE, g["polls"]["customIds"]["endJobType"]);
    assert_eq!(JobType::PollEnd.as_str(), ps::END_JOB_TYPE);
    assert_eq!(gs::end_job_key("cabc123"), g["giveaway"]["customIds"]["endJobKey"]);
    assert_eq!(ps::end_job_key("cpoll1"), g["polls"]["customIds"]["endJobKey"]);
}

#[test]
fn custom_ids_match_the_js() {
    let g = golden();
    let join = gs::build_join_row("cabc123", 0, false);
    assert_eq!(to_json(&join)["components"][0]["custom_id"], g["giveaway"]["customIds"]["join"]);
    assert_eq!(ps::vote_custom_id("cpoll1", 3), g["polls"]["customIds"]["vote"]);
    let modal = to_json(&ps::build_create_modal("1234567890"));
    assert_eq!(modal["custom_id"], g["polls"]["customIds"]["modal"]);
    let parsed = bot_config::parse_custom_id(g["giveaway"]["customIds"]["join"].as_str().unwrap_or(""));
    assert_eq!((parsed.prefix, parsed.params), ("giveaway", vec!["join", "cabc123"]));
    let sel = bot_config::parse_custom_id(g["polls"]["customIds"]["select"].as_str().unwrap_or(""));
    assert_eq!((sel.prefix, sel.params), ("poll-select", vec!["cpoll1"]));
}

#[test]
fn constants_match_the_js() {
    let c = &golden()["polls"]["constants"];
    assert_eq!(c["QUESTION_MAX"], ps::poll::QUESTION_MAX);
    assert_eq!(c["OPTION_MAX"], ps::poll::OPTION_MAX);
    assert_eq!(c["OPTIONS_MAX"], ps::poll::OPTIONS_MAX);
    assert_eq!(c["ACTIVE_MAX"], ps::poll::ACTIVE_MAX);
    assert_eq!(c["DURATION_MAX_MS"], ps::poll::DURATION_MAX_MS);
    assert_eq!(c["PENDING_TTL_MS"], ps::poll::PENDING_TTL_MS);
    assert_eq!(c["RENDER_INTERVAL_MS"], ps::poll::RENDER_INTERVAL_MS);
}

// ── Giveaways: pure helpers against the JS ────────────────────────────────────

#[test]
fn join_rows_match_the_js() {
    for c in golden()["giveaway"]["joinRows"].as_array().expect("rows") {
        let row = gs::build_join_row(c["id"].as_str().unwrap_or(""), c["count"].as_i64().unwrap_or(0), c["disabled"].as_bool().unwrap_or(false));
        assert_same(&to_json(&row), &c["row"], &format!("join row {c}"));
    }
}

#[test]
fn giveaway_embeds_match_the_js() {
    for c in golden()["giveaway"]["embeds"].as_array().expect("embeds") {
        let g = giveaway_of(&c["g"]);
        let settings = settings_of(&c["settings"]);
        let roles: Vec<String> = serde_json::from_value(c["roles"].clone()).unwrap_or_default();
        let exists = |id: &str| roles.iter().any(|r| r == id);
        let embed = gs::build_giveaway_embed(&g, settings.as_ref(), c["entryCount"].as_i64().unwrap_or(0), &exists, dt(c["now"].as_str().unwrap_or("")));
        assert_same(&to_json(&embed), &c["embed"], c["name"].as_str().unwrap_or(""));
    }
}

#[test]
fn list_lines_match_the_js() {
    for c in golden()["giveaway"]["listLines"].as_array().expect("lines") {
        let r = &c["row"];
        let row = gs::ListRow {
            id: r["id"].as_str().unwrap_or("").into(),
            prize: r["prize"].as_str().unwrap_or("").into(),
            status: GiveawayStatus::parse(r["status"].as_str().unwrap_or("")).expect("status"),
            channel_id: r["channelId"].as_str().unwrap_or("").into(),
            ends_at: dt(r["endsAt"].as_str().unwrap_or("")),
            ended_at: r["endedAt"].as_str().map(dt),
            entries: r["_count"]["entries"].as_i64().unwrap_or(0),
        };
        assert_eq!(gs::format_list_line(&row), c["line"].as_str().unwrap_or(""));
    }
}

#[test]
fn announcements_match_the_js() {
    for c in golden()["giveaway"]["announce"].as_array().expect("announce") {
        let g = giveaway_of(&c["g"]);
        let winners: Vec<String> = serde_json::from_value(c["winners"].clone()).unwrap_or_default();
        let settings = settings_of(&c["settings"]);
        let ours = gs::announcement_bodies(&g, &winners, settings.as_ref(), c["reroll"].as_bool().unwrap_or(false));
        assert_same(&Value::Array(ours), &c["bodies"], c["name"].as_str().unwrap_or(""));
    }
}

#[test]
fn pure_giveaway_helpers_match_the_js() {
    let p = &golden()["giveaway"]["pure"];
    for c in p["normalize"].as_array().expect("normalize") {
        assert_eq!(serde_json::to_value(gs::normalize_role_weights(&c["input"])).unwrap_or_default(), c["out"], "normalize {}", c["input"]);
    }
    for c in p["pack"].as_array().expect("pack") {
        let ids: Vec<String> = serde_json::from_value(c["ids"].clone()).unwrap_or_default();
        assert_eq!(json!(gs::pack_mentions(&ids, c["max"].as_u64().unwrap_or(0) as usize)), c["out"]);
    }
    for c in p["refs"].as_array().expect("refs") {
        let ours = match gs::parse_giveaway_ref(c["input"].as_str().unwrap_or("")) {
            None => Value::Null,
            Some(gs::GiveawayRef::Id(v)) => json!({ "kind": "id", "value": v }),
            Some(gs::GiveawayRef::Message { message_id, guild_id: None }) => json!({ "kind": "message", "value": message_id }),
            Some(gs::GiveawayRef::Message { message_id, guild_id: Some(g) }) => json!({ "kind": "message", "value": message_id, "guildId": g }),
        };
        assert_eq!(ours, c["out"], "ref {}", c["input"]);
    }
    let now = dt("2026-05-01T12:00:00.000Z");
    for c in p["endsAt"].as_array().expect("endsAt") {
        let ends = c["delta"].as_i64().map(|d| now + chrono::Duration::milliseconds(d));
        let ours = match gs::validate_ends_at(ends, now, "Europe/Berlin") {
            Ok(()) => json!({ "ok": true }),
            Err((t, m)) => json!({ "ok": false, "title": t, "message": m }),
        };
        assert_eq!(ours, c["out"]);
    }
}

#[tokio::test]
async fn the_lazy_draw_matches_the_js_draw() {
    for c in golden()["giveaway"]["draws"].as_array().expect("draws") {
        let ids: Vec<String> = serde_json::from_value(c["userIds"].clone()).unwrap_or_default();
        let us: Vec<f64> = serde_json::from_value(c["us"].clone()).unwrap_or_default();
        let table = c["table"].as_object().cloned().unwrap_or_default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut k = 0usize;
        let calls2 = calls.clone();
        let out = gs::draw_winners(
            &ids,
            c["count"].as_i64().unwrap_or(1),
            c["maxWeight"].as_i64().unwrap_or(1),
            move |batch: Vec<String>| {
                calls2.lock().unwrap_or_else(|e| e.into_inner()).push(batch.len());
                let mut m = gs::Resolved::new();
                for id in batch {
                    match table.get(&id) {
                        Some(Value::String(s)) if s == "unknown" => {}
                        Some(Value::String(_)) => {
                            m.insert(id, None);
                        }
                        Some(w) => {
                            m.insert(id, w.as_i64());
                        }
                        None => {}
                    }
                }
                async move { m }
            },
            || {
                k += 1;
                us[k - 1]
            },
            c["batchSize"].as_u64().unwrap_or(1) as usize,
            f64::INFINITY,
            || 0.0,
        )
        .await;
        assert_eq!(json!({ "winnerIds": out.winner_ids, "complete": out.complete, "unknown": out.unknown }), c["out"], "seed {}", c["seed"]);
        assert_eq!(json!(*calls.lock().unwrap_or_else(|e| e.into_inner())), c["calls"], "lazy batches, seed {}", c["seed"]);
    }
}

// test/giveaways.test.js 2.
#[test]
fn member_weight_requirements_and_denied_roles() {
    let w = vec![gs::RoleWeight { role_id: "a".into(), weight: 5 }, gs::RoleWeight { role_id: "b".into(), weight: 3 }];
    assert_eq!(gs::member_weight(|r| r == "b", &w), 3);
    assert_eq!(gs::member_weight(|r| r == "a" || r == "b", &w), 5);
    assert_eq!(gs::member_weight(|_| false, &w), 1);
    assert!(gs::meets_requirements(|r| r == "x", &["x"]));
    assert!(!gs::meets_requirements(|_| false, &["x"]));
    assert!(gs::meets_requirements::<&str>(|_| false, &[]));
    assert_eq!(gs::held_denied_role(|r| r == "d2", &["d1", "d2"]), Some("d2".into()));
    assert_eq!(gs::held_denied_role(|_| false, &["d1"]), None);
    assert_eq!(gs::evaluate_member(true, &[], &[], &[], &w), None, "bots never win");
    assert_eq!(gs::evaluate_member(false, &["a".into()], &["a".into()], &["z".into()], &w), Some(5));
    assert_eq!(gs::evaluate_member(false, &["a".into(), "z".into()], &[], &["z".into()], &w), None);
}

fn table_resolver(table: HashMap<String, Option<i64>>) -> impl FnMut(Vec<String>) -> std::future::Ready<gs::Resolved> {
    move |ids: Vec<String>| std::future::ready(ids.into_iter().filter_map(|id| table.get(&id).map(|v| (id, *v))).collect())
}

// test/giveaways.test.js 3a-3h.
#[tokio::test]
async fn draw_basics_laziness_distribution_deadline_and_duplicates() {
    let ids: Vec<String> = (0..10).map(|i| format!("u{i}")).collect();
    let table: HashMap<String, Option<i64>> = ids.iter().map(|i| (i.clone(), Some(1))).collect();
    let out = gs::draw_winners(&ids, 3, 1, table_resolver(table.clone()), gs::secure_random, 100, f64::INFINITY, || 0.0).await;
    assert_eq!(out.winner_ids.len(), 3);
    assert_eq!(out.winner_ids.iter().collect::<HashSet<_>>().len(), 3);
    assert!(out.complete);

    // 3b: a count above the eligible entrants gives exactly the eligible ones.
    let mut t2 = table.clone();
    for i in 0..7 {
        t2.insert(format!("u{i}"), None);
    }
    let out = gs::draw_winners(&ids, 9, 1, table_resolver(t2), gs::secure_random, 100, f64::INFINITY, || 0.0).await;
    let mut w = out.winner_ids.clone();
    w.sort();
    assert_eq!(w, vec!["u7", "u8", "u9"]);

    // 3c: unknown entrants are counted and never chosen.
    let mut t3 = table.clone();
    t3.remove("u0");
    t3.remove("u1");
    let out = gs::draw_winners(&ids, 10, 1, table_resolver(t3), gs::secure_random, 100, f64::INFINITY, || 0.0).await;
    assert_eq!(out.unknown, 2);
    assert!(!out.winner_ids.contains(&"u0".to_string()));

    // 3d: 1000 entrants, one winner, one resolver call.
    let many: Vec<String> = (0..1000).map(|i| format!("m{i}")).collect();
    let all: HashMap<String, Option<i64>> = many.iter().map(|i| (i.clone(), Some(1))).collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let mut inner = table_resolver(all);
    let out = gs::draw_winners(&many, 1, 1, move |b| { c2.fetch_add(1, Ordering::SeqCst); inner(b) }, gs::secure_random, 100, f64::INFINITY, || 0.0).await;
    assert_eq!(out.winner_ids.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // 3f: weights 1 and 3 give the heavy entrant 0.75 of the wins.
    let pair = vec!["light".to_string(), "heavy".to_string()];
    let weights: HashMap<String, Option<i64>> = [("light".to_string(), Some(1)), ("heavy".to_string(), Some(3))].into();
    let mut heavy = 0;
    for _ in 0..4000 {
        let out = gs::draw_winners(&pair, 1, 3, table_resolver(weights.clone()), gs::secure_random, 100, f64::INFINITY, || 0.0).await;
        if out.winner_ids == ["heavy"] {
            heavy += 1;
        }
    }
    let share = heavy as f64 / 4000.0;
    assert!((share - 0.75).abs() < 0.04, "heavy share {share}");

    // 3g: a passed deadline gives complete: false without resolving.
    let calls = Arc::new(AtomicUsize::new(0));
    let c3 = calls.clone();
    let mut inner = table_resolver(table.clone());
    let out = gs::draw_winners(&ids, 1, 1, move |b| { c3.fetch_add(1, Ordering::SeqCst); inner(b) }, gs::secure_random, 100, 10.0, || 11.0).await;
    assert!(!out.complete);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // 3h: duplicates drawn at most once; empty input or count 0 gives nothing.
    let dup = vec!["u1".to_string(), "u1".to_string(), "u1".to_string()];
    let out = gs::draw_winners(&dup, 3, 1, table_resolver(table.clone()), gs::secure_random, 100, f64::INFINITY, || 0.0).await;
    assert_eq!(out.winner_ids, vec!["u1"]);
    assert!(gs::draw_winners(&[], 3, 1, table_resolver(table.clone()), gs::secure_random, 100, f64::INFINITY, || 0.0).await.winner_ids.is_empty());
    assert!(gs::draw_winners(&ids, 0, 1, table_resolver(table), gs::secure_random, 100, f64::INFINITY, || 0.0).await.winner_ids.is_empty());
    let r = gs::secure_random();
    assert!(r > 0.0 && r < 1.0);
}

// test/giveaways.test.js 9.
#[test]
fn reward_unavailable_messages() {
    assert_eq!(
        gs::reward_unavailable_text(RewardKind::Xp, false, false).as_deref(),
        Some("XP prizes are not available on this bot (the ⭐ Levels module is not installed).")
            .map(|s| s.replace("⭐ Levels", &format!("{} {}", ModuleKey::Leveling.info().emoji, ModuleKey::Leveling.info().label)))
            .as_deref()
    );
    let m = ModuleKey::Economy.info();
    assert_eq!(
        gs::reward_unavailable_text(RewardKind::Coins, true, false),
        Some(format!("Coin prizes need the **{} {}** module. Enable it with `/module enable economy`.", m.emoji, m.label))
    );
    assert_eq!(gs::reward_unavailable_text(RewardKind::Coins, true, true), None);
}

#[test]
fn pay_warnings_name_the_kind_and_reason() {
    let mut results = std::collections::BTreeMap::new();
    results.insert("xp".to_string(), gs::PayTally { ok: 1, failed: 2, reasons: vec!["module-disabled".into(), "error".into()] });
    results.insert("coins".to_string(), gs::PayTally { ok: 0, failed: 1, reasons: vec!["unavailable".into()] });
    let w = gs::pay_warnings(&results);
    let lv = ModuleKey::Leveling.info();
    let ec = ModuleKey::Economy.info();
    assert_eq!(
        w,
        vec![
            format!("⚠️ The XP prize could not be paid to 2 winners: the {} {} module is turned off. Please pay it manually.", lv.emoji, lv.label),
            format!("⚠️ The coin prize could not be paid to 1 winner: the {} {} module is not installed. Please pay it manually.", ec.emoji, ec.label),
        ]
    );
}

#[test]
fn odds_decisions() {
    let rw = |r: &str, w: i64| gs::RoleWeight { role_id: r.into(), weight: w };
    let ten: Vec<gs::RoleWeight> = (0..10).map(|i| rw(&format!("8100000000000000{i:02}"), 2)).collect();
    assert_eq!(gs::next_odds(&ten, "810000000000000099", gs::OddsOp::Add { weight: 3 }), Err(gs::GiveawayError::Limit));
    let replaced = gs::next_odds(&ten, "810000000000000000", gs::OddsOp::Add { weight: 7 }).expect("replace needs no room");
    assert_eq!(replaced[0], rw("810000000000000000", 7));
    assert_eq!(gs::next_odds(&ten, "810000000000000099", gs::OddsOp::Remove), Err(gs::GiveawayError::NotSet));
    assert_eq!(gs::next_odds(&ten, "810000000000000001", gs::OddsOp::Remove).map(|v| v.len()), Ok(9));
}

// ── Polls: pure helpers against the JS ────────────────────────────────────────

#[test]
fn poll_components_match_the_js() {
    for c in golden()["polls"]["components"].as_array().expect("components") {
        let p = poll_of(&c["p"]);
        let selected: Option<Vec<i32>> = c["opts"].get("selected").and_then(|s| serde_json::from_value(s.clone()).ok());
        let rows = ps::build_poll_components(&p, c["opts"]["disabled"].as_bool().unwrap_or(false), selected.as_deref());
        assert_same(&to_json(&rows), &c["rows"], c["name"].as_str().unwrap_or(""));
    }
}

#[test]
fn poll_embeds_payloads_and_results_match_the_js() {
    let g = golden();
    for c in g["polls"]["embeds"].as_array().expect("embeds") {
        let t: ps::Tally = serde_json::from_value(c["tally"].clone()).expect("tally");
        let s = settings_of(&c["settings"]);
        let e = ps::build_poll_embed(&poll_of(&c["p"]), &t, s.as_ref(), c["ended"].as_bool().unwrap_or(false));
        assert_same(&to_json(&e), &c["embed"], c["name"].as_str().unwrap_or(""));
    }
    for c in g["polls"]["payloads"].as_array().expect("payloads") {
        let t: ps::Tally = serde_json::from_value(c["tally"].clone()).expect("tally");
        let body = ps::build_poll_payload_default(&poll_of(&c["p"]), &t, None);
        let mut js = c["body"].clone();
        js["embeds"][0] = without_timestamp(&js["embeds"][0]);
        let mut ours = body.clone();
        ours["embeds"][0] = without_timestamp(&ours["embeds"][0]);
        // The poll embed's timestamp is the poll's creation time on both sides.
        assert_eq!(norm(&body["embeds"][0])["timestamp"], norm(&c["body"]["embeds"][0])["timestamp"]);
        assert_same(&ours, &js, c["name"].as_str().unwrap_or(""));
    }
    for c in g["polls"]["results"].as_array().expect("results") {
        let t: ps::Tally = serde_json::from_value(c["tally"].clone()).expect("tally");
        let e = ps::build_results_embed(&poll_of(&c["p"]), &t, None, c["live"].as_bool().unwrap_or(false), c["messageExists"].as_bool().unwrap_or(true));
        assert_same(&without_timestamp(&to_json(&e)), &without_timestamp(&c["embed"]), c["name"].as_str().unwrap_or(""));
    }
    assert_same(&to_json(&ps::build_create_modal("1234567890")), &g["polls"]["modal"], "create modal");
    let polls: Vec<Poll> = g["polls"]["listInput"].as_array().expect("list").iter().map(poll_of).collect();
    let votes: HashMap<String, i64> = [(polls[0].id.clone(), 1), ("c3".to_string(), 2500)].into();
    assert_eq!(json!(ps::poll_list_lines(&polls, &votes, dt("2026-05-01T12:00:00.000Z").timestamp_millis())), g["polls"]["listLines"]);
}

#[test]
fn poll_parsers_and_rules_match_the_js() {
    let g = &golden()["polls"];
    for c in g["optionsParse"].as_array().expect("options") {
        let (options, errors) = ps::parse_poll_options(c["raw"].as_str().unwrap_or(""));
        assert_eq!(json!({ "options": options, "errors": errors }), c["out"], "options {:?}", c["raw"]);
    }
    for c in g["questions"].as_array().expect("questions") {
        let (question, errors) = ps::validate_question(c["raw"].as_str().unwrap_or(""));
        assert_eq!(json!({ "question": question, "errors": errors }), c["out"]);
    }
    for c in g["durations"].as_array().expect("durations") {
        let ours = match ps::parse_duration_option(c["raw"].as_str()) {
            Ok(ms) => json!({ "ms": ms }),
            Err(e) => json!({ "error": e.key() }),
        };
        assert_eq!(ours, c["out"], "duration {}", c["raw"]);
    }
    for c in g["canVote"].as_array().expect("canVote") {
        let held: Vec<String> = serde_json::from_value(c["held"].clone()).unwrap_or_default();
        let allowed: Vec<String> = serde_json::from_value(c["poll"]["allowedRoleIds"].clone()).unwrap_or_default();
        let denied: Vec<String> = serde_json::from_value(c["poll"]["deniedRoleIds"].clone()).unwrap_or_default();
        let existing: Option<HashSet<String>> = serde_json::from_value(c["existing"].clone()).ok();
        let ours = match ps::can_vote(&held, &allowed, &denied, existing.as_ref()) {
            Ok(()) => json!({ "ok": true }),
            Err(ps::VoteRefusal::Denied { role_id }) => json!({ "ok": false, "reason": "denied", "roleId": role_id }),
            Err(ps::VoteRefusal::NotAllowed { role_id }) => json!({ "ok": false, "reason": "not-allowed", "roleId": role_id }),
            Err(ps::VoteRefusal::AllowedRoleGone) => json!({ "ok": false, "reason": "allowed-role-gone" }),
        };
        assert_eq!(ours, c["out"], "canVote {c}");
    }
    for c in g["toggles"].as_array().expect("toggles") {
        let cur: Vec<i32> = serde_json::from_value(c["cur"].clone()).unwrap_or_default();
        let (next, r) = ps::apply_vote_toggle(&cur, c["idx"].as_i64().unwrap_or(0) as i32, c["max"].as_i64().unwrap_or(1) as i32);
        assert_eq!(json!({ "next": next, "result": r }), c["out"]);
    }
    for c in g["selections"].as_array().expect("selections") {
        let values: Vec<String> = serde_json::from_value(c["values"].clone()).unwrap_or_default();
        let ours = ps::normalize_selection(&values, c["n"].as_u64().unwrap_or(0) as usize, c["max"].as_i64().unwrap_or(0) as i32);
        assert_eq!(json!(ours), c["out"], "selection {c}");
    }
    let opts = vec!["Chess".to_string(), "Go".to_string(), "Poker".to_string()];
    for c in g["optionLines"].as_array().expect("lines") {
        let t: ps::Tally = serde_json::from_value(g["tallies"][c["tally"].as_str().unwrap_or("")].clone()).expect("tally");
        assert_eq!(ps::format_option_lines(&opts, &t, c["hidden"].as_bool().unwrap_or(false), c["ended"].as_bool().unwrap_or(false)), c["out"]);
    }
    for c in g["tallyFromGroups"].as_array().expect("groups") {
        let groups: Vec<(i64, i64)> = c["groups"].as_array().into_iter().flatten().map(|x| (x["optionIndex"].as_i64().unwrap_or(-1), x["_count"]["_all"].as_i64().unwrap_or(0))).collect();
        let t = ps::tally_from_groups(c["n"].as_u64().unwrap_or(0) as usize, &groups, c["totals"]["votes"].as_i64().unwrap_or(0), c["totals"]["voters"].as_i64().unwrap_or(0));
        assert_eq!(serde_json::to_value(&t).unwrap_or_default(), c["out"]);
    }
    for c in g["sendFailure"].as_array().expect("sendFailure") {
        let e = c["code"].as_u64().map(DiscordError::code);
        assert_eq!(ps::send_failure_reason(Some(&e.unwrap_or_else(|| DiscordError::new(None, "x")))), c["out"].as_str().unwrap_or(""));
    }
    assert_eq!(ps::winners(&[0, 0]), Vec::<usize>::new());
    assert_eq!(ps::winners(&[2, 3, 3]), vec![1, 2]);
    assert_eq!((ps::percent(1, 3), ps::percent(2, 3), ps::percent(0, 0)), (33, 67, 0));
}

// test/polls.test.js 10.
#[test]
fn pending_store_expires_and_evicts() {
    let s = ps::PendingStore::new(1000, 2);
    s.put("a", 1, 0);
    assert_eq!(s.take("a", 500), Some(1));
    assert_eq!(s.take("a", 500), None, "taken once");
    s.put("b", 2, 0);
    assert_eq!(s.take("b", 1001), None, "expired");
    s.put("c", 3, 0);
    s.put("d", 4, 10);
    s.put("e", 5, 20);
    assert_eq!(s.size(), 2, "the oldest is evicted beyond max");
    assert_eq!(s.take("c", 30), None);
    s.put("f", 6, 2000);
    assert_eq!(s.size(), 1, "expired entries are swept on put");
}

// test/polls.test.js 11.
#[tokio::test(start_paused = true)]
async fn request_render_renders_once_then_once_per_window() {
    let t = TestApp::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    ps::set_render_impl(
        &t,
        Some(Arc::new(move |_app, _id| {
            c2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(ps::RenderOutcome::Ok) })
        })),
    );
    ps::request_render(&t, "p1");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the first request renders at once");
    for _ in 0..5 {
        ps::request_render(&t, "p1");
    }
    assert_eq!(t.ext::<ps::RenderState>().queued(), 1, "requests inside the window collapse");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    t.clock.advance(5_000);
    tokio::time::sleep(std::time::Duration::from_millis(5_000)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one trailing render");
    ps::request_render(&t, "p2");
    ps::cancel_render(&t, "p2");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "a cancelled render never runs");
}

// ── Commands, buttons and modals that answer before the database ──────────────

const OG: &str = "900000000000000111";
const OCH: &str = "800000000000000111";
const MOD: &str = "600000000000000111";
const MOD_ROLE: &str = "820000000000000111";

fn offline_guild() -> FakeGuild {
    let bot = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::READ_MESSAGE_HISTORY;
    FakeGuild::new(OG, "600000000000000099")
        .role("820000000000000110", 1, bot)
        .member(TEST_BOT_ID, &["820000000000000110"])
        .role(MOD_ROLE, 2, P::MODERATE_MEMBERS | P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .member(MOD, &[MOD_ROLE])
        .channel(OCH, 0, "general", &[])
        .channel("800000000000000112", 0, "locked", &[(OG, false, P::empty(), P::SEND_MESSAGES)])
}

fn mod_slash(name: &str, sub: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(OG).user(MOD).roles(&[MOD_ROLE]).permissions(P::MODERATE_MEMBERS).channel(OCH).subcommand(sub)
}

// test/polls.test.js 15.
#[tokio::test]
async fn poll_create_validates_stores_the_form_and_shows_the_modal() {
    let t = TestApp::new().with_guilds([OG]).with_fake_guilds(&[offline_guild()]);
    t.install(registry());
    let route = |i| bot_core::router::route_interaction(&t, i);

    route(mod_slash("poll", "create").option("duration", option_type::STRING, json!("nonsense")).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].method, "reply");
    assert!(calls[0].ephemeral());
    assert_eq!(calls[0].embed_title(), Some("❌  Invalid Duration"));
    assert_eq!(calls[0].embed_description(), Some("Use a duration like `30m`, `2h`, `1d` or `1w`."));

    t.responses.clear();
    route(mod_slash("poll", "create").option("denied_role", option_type::ROLE, json!(OG)).build()).await;
    assert_eq!(t.responses.calls()[0].embed_description(), Some("You can't deny @everyone, or nobody could vote."));

    t.responses.clear();
    route(mod_slash("poll", "create").option("allowed_role", option_type::ROLE, json!(MOD_ROLE)).option("denied_role", option_type::ROLE, json!(MOD_ROLE)).build()).await;
    assert_eq!(t.responses.calls()[0].embed_description(), Some("The allowed role and the denied role must be different."));

    t.responses.clear();
    route(mod_slash("poll", "create").option("channel", option_type::CHANNEL, json!("800000000000000112")).build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Cannot Post There"));
    assert_eq!(t.responses.calls()[0].embed_description(), Some("I need **Send Messages** in <#800000000000000112> to post a poll."));

    t.responses.clear();
    route(mod_slash("poll", "create").option("duration", option_type::STRING, json!("2h")).option("max_choices", option_type::INTEGER, json!(3)).option("hide_results", option_type::BOOLEAN, json!(true)).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].method, "showModal");
    let nonce = calls[0].body["custom_id"].as_str().unwrap_or("").trim_start_matches("poll-modal:").to_string();
    let pending = t.ext::<ps::PendingPolls>().0.take(&nonce, t.clock.now_ms()).expect("the form is stored");
    assert_eq!(
        pending,
        ps::PendingPoll {
            guild_id: OG.into(),
            channel_id: OCH.into(),
            user_id: MOD.into(),
            max_choices: 3,
            allowed_role_ids: vec![],
            denied_role_ids: vec![],
            hide_results: true,
            duration_ms: Some(7_200_000),
        }
    );
}

// test/polls.test.js 16.
#[tokio::test]
async fn poll_modal_expired_foreign_and_invalid_forms() {
    let t = TestApp::new().with_guilds([OG]).with_fake_guilds(&[offline_guild()]);
    t.install(registry());
    let modal = |nonce: &str, q: &str, o: &str| FakeInteraction::modal(&format!("poll-modal:{nonce}"), &[("poll-question", q), ("poll-options", o)]).guild(OG).user(MOD).roles(&[MOD_ROLE]).build();
    bot_core::router::route_interaction(&t, modal("1", "Q?", "a\nb")).await;
    let c = &t.responses.calls()[0];
    assert_eq!(c.embed_title(), Some("❌  Form Expired"));
    assert!(c.ephemeral());
    assert!(c.embed_description().unwrap_or("").ends_with("Your input (copy it to try again):\n```\nQ?\n---\na\nb\n```"), "{:?}", c.embed_description());

    let pending = |user: &str| ps::PendingPoll {
        guild_id: OG.into(),
        channel_id: OCH.into(),
        user_id: user.into(),
        max_choices: 1,
        allowed_role_ids: vec![],
        denied_role_ids: vec![],
        hide_results: false,
        duration_ms: None,
    };
    t.ext::<ps::PendingPolls>().0.put("2", pending("600000000000000222"), t.clock.now_ms());
    t.responses.clear();
    bot_core::router::route_interaction(&t, modal("2", "Q?", "a\nb")).await;
    assert_eq!(t.responses.calls()[0].embed_description(), Some("This form is not yours. Run `/poll create` to start a poll."));

    t.ext::<ps::PendingPolls>().0.put("3", pending(MOD), t.clock.now_ms());
    t.responses.clear();
    bot_core::router::route_interaction(&t, modal("3", "", "a\nA\n```x")).await;
    let c = &t.responses.calls()[0];
    assert_eq!(c.embed_title(), Some("❌  Invalid Poll"));
    assert!(
        c.embed_description().unwrap_or("").starts_with("• The question is empty.\n• Option 2 repeats option 1."),
        "{:?}",
        c.embed_description()
    );
    assert!(c.embed_description().unwrap_or("").contains("`\u{200b}``x"), "triple backticks are broken up");
}

// test/polls.test.js 17 and the giveaway twin.
#[tokio::test]
async fn malformed_buttons_answer_without_the_database() {
    let t = TestApp::new().with_guilds([OG]).with_fake_guilds(&[offline_guild()]);
    t.install(registry());
    for id in ["poll:vote:cp1:abc", "poll:other:cp1:1", "poll:vote::1", "poll:vote:cp1:123", "giveaway:join", "giveaway:leave:cabc"] {
        t.responses.clear();
        // The module gate reads the settings: on in the memory store by default.
        bot_core::router::route_interaction(&t, FakeInteraction::button(id).guild(OG).user(MOD).build()).await;
        let calls = t.responses.calls();
        assert_eq!(calls.len(), 1, "{id}: {calls:?}");
        assert_eq!(calls[0].content(), Some("This button is no longer active."), "{id}");
        assert!(calls[0].ephemeral());
    }
}

#[test]
fn autocomplete_names() {
    let now = dt("2026-05-01T12:00:00.000Z").timestamp_millis();
    assert_eq!(gs::status_text(GiveawayStatus::Draft), "a draft");
    let row = gs::AutocompleteRow {
        id: "c1".into(),
        prize: "Nitro".into(),
        status: GiveawayStatus::Active,
        channel_id: "8".into(),
        ends_at: dt("2026-05-01T14:00:00.000Z"),
    };
    assert_eq!(feat_giveaways_polls::commands::giveaway::choice_name(&row, Some("general"), now), "🟢 Nitro · #general · ends in 2h");
    let draft = gs::AutocompleteRow { status: GiveawayStatus::Draft, ..row };
    assert_eq!(feat_giveaways_polls::commands::giveaway::choice_name(&draft, None, now), "📝 Nitro · #deleted-channel · draft");
    assert_eq!(
        feat_giveaways_polls::commands::poll::choice_name("end", "Why?", now - 3 * 60_000, now, None, None),
        "Why? · 3m ago"
    );
    assert_eq!(feat_giveaways_polls::commands::poll::choice_name("results", "Why?", now, now, Some(PollStatus::Ended), Some("general")), "🏁 Why? · #general");
}

// ── Database: the flows against the real tables ───────────────────────────────

/// The recording Discord seam: sends get fresh ids, edits and fetches of a deleted message fail with 10008.
#[derive(Default)]
struct FakeOps {
    log: Mutex<Vec<(&'static str, String, Value)>>,
    seq: AtomicU64,
    fail_send: Mutex<Option<u64>>,
    gone: Mutex<HashSet<String>>,
    /// REST member answers by user id: a member JSON, `None` (not in the guild) or an error code.
    rest: Mutex<HashMap<String, Result<Option<Value>, u64>>>,
    /// The answer for every other member lookup: an error code, else "not in the guild".
    member_error: Mutex<Option<u64>>,
    /// Member lookups take this long (ms).
    member_delay_ms: AtomicU64,
    /// REST channel answers by channel id (a GuildChannel JSON); every other channel is Unknown Channel.
    channels: Mutex<HashMap<String, Value>>,
}

impl FakeOps {
    fn calls(&self, op: &str) -> Vec<(String, Value)> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(o, _, _)| *o == op).map(|(_, k, v)| (k.clone(), v.clone())).collect()
    }
    fn sends(&self) -> Vec<Value> {
        self.calls("send").into_iter().map(|(_, v)| v).collect()
    }
    fn clear(&self) {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    fn delete_message(&self, id: &str) {
        self.gone.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string());
    }
    fn is_gone(&self, id: &str) -> bool {
        self.gone.lock().unwrap_or_else(|e| e.into_inner()).contains(id)
    }
    fn record(&self, op: &'static str, key: String, v: Value) {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push((op, key, v));
    }
}

#[async_trait]
impl DiscordOps for FakeOps {
    async fn send_message(&self, guild_id: &str, channel_id: &str, body: Value) -> Result<SentMessage, DiscordError> {
        if let Some(code) = *self.fail_send.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(DiscordError::code(code));
        }
        let id = format!("71{:017}", self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        self.record("send", channel_id.to_string(), body);
        Ok(SentMessage { id, channel_id: channel_id.into(), guild_id: Some(guild_id.into()) })
    }
    async fn edit_message(&self, _channel_id: &str, message_id: &str, body: Value) -> Result<(), DiscordError> {
        if self.is_gone(message_id) {
            return Err(DiscordError::code(10008));
        }
        self.record("edit", message_id.to_string(), body);
        Ok(())
    }
    async fn delete_message(&self, _channel_id: &str, message_id: &str) -> Result<(), DiscordError> {
        self.record("delete", message_id.to_string(), Value::Null);
        FakeOps::delete_message(self, message_id);
        Ok(())
    }
    async fn fetch_message(&self, _channel_id: &str, message_id: &str) -> Result<(), DiscordError> {
        if self.is_gone(message_id) { Err(DiscordError::code(10008)) } else { Ok(()) }
    }
    async fn fetch_channel(&self, channel_id: &str) -> Result<Option<GuildChannel>, DiscordError> {
        self.record("channel", channel_id.to_string(), Value::Null);
        match self.channels.lock().unwrap_or_else(|e| e.into_inner()).get(channel_id) {
            Some(v) => Ok(Some(serde_json::from_value(v.clone()).expect("channel json"))),
            None => Err(DiscordError::code(10003)),
        }
    }
    async fn fetch_member(&self, guild_id: &str, user_id: &str) -> Result<Option<Member>, DiscordError> {
        let delay = self.member_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        self.record("member", user_id.to_string(), Value::Null);
        let answer = self.rest.lock().unwrap_or_else(|e| e.into_inner()).get(user_id).cloned();
        match answer {
            Some(Ok(Some(mut v))) => {
                v["guild_id"] = json!(guild_id);
                Ok(Some(serde_json::from_value(v).map_err(|e| DiscordError::new(None, e.to_string()))?))
            }
            Some(Ok(None)) => Ok(None),
            Some(Err(code)) => Err(DiscordError::code(code)),
            None => match *self.member_error.lock().unwrap_or_else(|e| e.into_inner()) {
                Some(code) => Err(DiscordError::code(code)),
                None => Ok(None),
            },
        }
    }
}

const BOT_ROLE: &str = "830000000000000001";
const ROLE_A: &str = "830000000000000002";
const ROLE_B: &str = "830000000000000003";
const ROLE_MOD: &str = "830000000000000004";
const MODERATOR: &str = "610000000000000001";

fn user(n: u32) -> String {
    format!("62000000000000{n:04}")
}

fn member_roles(n: u32) -> Vec<String> {
    match n {
        1 => vec![ROLE_A.into()],
        2 => vec![ROLE_B.into()],
        _ => vec![],
    }
}

struct Env {
    t: DbTestApp,
    ops: Arc<FakeOps>,
    g: &'static str,
    ch: String,
}

impl Env {
    /// A guild with the bot (allowed to post), a moderator, members `1..=members` (member 1 holds ROLE_A, member 2
    /// ROLE_B) and one text channel.
    async fn new(g: &'static str, members: u32) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([g]);
        t.reset_guilds(&[g]).await;
        let ch = format!("84{}", &g[2..]);
        let bot = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::READ_MESSAGE_HISTORY;
        let mut guild = FakeGuild::new(g, "600000000000000099")
            .role(BOT_ROLE, 1, bot)
            .member(TEST_BOT_ID, &[BOT_ROLE])
            .role(ROLE_A, 2, P::empty())
            .role(ROLE_B, 3, P::empty())
            .role(ROLE_MOD, 4, P::MODERATE_MEMBERS | P::VIEW_CHANNEL | P::SEND_MESSAGES)
            .member(MODERATOR, &[ROLE_MOD])
            .channel(&ch, 0, "giveaways", &[])
            .channel(&format!("85{}", &g[2..]), 0, "locked", &[(g, false, P::empty(), P::SEND_MESSAGES)])
            .channel(&format!("86{}", &g[2..]), 2, "voice", &[]);
        for n in 1..=members {
            let roles = member_roles(n);
            let refs: Vec<&str> = roles.iter().map(String::as_str).collect();
            guild = guild.member(&user(n), &refs);
        }
        t.set_discord(offline_discord_as(&[guild.json()]));
        let ops = Arc::new(FakeOps::default());
        set_discord_ops(&t, ops.clone());
        t.install(registry());
        t.settings().ensure_guild_settings(g).await.expect("settings row");
        Some(Self { t, ops, g, ch })
    }

    fn channel(&self) -> GuildChannel {
        self.channel_by(&self.ch)
    }

    /// The channel where @everyone (so the bot) may not send messages.
    fn locked(&self) -> String {
        format!("85{}", &self.g[2..])
    }

    fn voice(&self) -> String {
        format!("86{}", &self.g[2..])
    }

    fn channel_by(&self, id: &str) -> GuildChannel {
        let gid = serenity::all::GuildId::new(self.g.parse().unwrap_or(1));
        let cid = serenity::all::ChannelId::new(id.parse().unwrap_or(1));
        bot_core::interaction_helpers::cached_channel(&self.t, gid, cid).expect("cached channel")
    }

    /// Makes the guild unavailable (`None`) or available again.
    fn set_available(&self, available: bool) {
        let ids: Vec<String> = if available { vec![self.g.to_string()] } else { Vec::new() };
        self.t.set_guild_directory(Arc::new(bot_core::discord::StaticGuilds::available(ids)));
    }

    fn start_opts(&self, prize: &str, ends_in_ms: i64) -> gs::StartGiveaway {
        gs::StartGiveaway {
            guild_id: self.g.into(),
            channel: self.channel(),
            host_id: MODERATOR.into(),
            prize: prize.into(),
            description: None,
            winner_count: 1,
            ends_at: chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis() + ends_in_ms).expect("time"),
            required_role_ids: vec![],
            denied_role_ids: vec![],
            role_weights: vec![],
            xp_prize: None,
            coin_prize: None,
            ping_role_id: None,
            embed_color: None,
            settings: None,
        }
    }

    async fn start(&self, prize: &str, ends_in_ms: i64) -> Giveaway {
        gs::start_giveaway(&self.t, self.start_opts(prize, ends_in_ms)).await.expect("started").giveaway
    }

    async fn join(&self, id: &str, n: u32) -> gs::JoinOutcome {
        let all = [BOT_ROLE, ROLE_A, ROLE_B, ROLE_MOD];
        gs::toggle_entry(&self.t, self.g, id, &user(n), &member_roles(n), &|r: &str| all.contains(&r)).await.expect("toggled")
    }

    async fn row(&self, id: &str) -> Giveaway {
        gs::find_giveaway(self.t.db(), id).await.expect("query").expect("row")
    }

    fn gid(&self) -> serenity::all::GuildId {
        serenity::all::GuildId::new(self.g.parse().unwrap_or(1))
    }

    fn cid(&self) -> serenity::all::ChannelId {
        serenity::all::ChannelId::new(self.ch.parse().unwrap_or(1))
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn mid_of(s: &str) -> serenity::all::MessageId {
    serenity::all::MessageId::new(s.parse().unwrap_or(1))
}

fn bind_sample<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    sig: &str,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    for t in sig.split(',') {
        q = match t {
            "ts" => q.bind(chrono::Utc::now().naive_utc()),
            "arr" => q.bind(vec!["0".to_string()]),
            "json" => q.bind(json!([])),
            _ => q.bind("0"),
        };
    }
    q
}

#[tokio::test]
async fn every_raw_statement_executes_against_the_schema() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let sigs: HashMap<&str, &str> = [
        ("giveaways.byId", "t"),
        ("giveaways.countOpen", "t"),
        ("giveaways.setMessage", "t,t,ts"),
        ("giveaways.delete", "t"),
        ("giveaways.entriesUntil", "t,ts"),
        ("giveaways.entriesAll", "t"),
        ("giveaways.stillIn", "t,arr"),
        ("giveaways.entryCount", "t"),
        ("giveaways.draftActive", "t,ts"),
        ("giveaways.draftIfMessage", "t,t,ts"),
        ("giveaways.clearMessage", "t,t,ts"),
        ("giveaways.claimEnd", "t,ts,arr,ts"),
        ("giveaways.claimReroll", "t,arr,ts,ts"),
        ("giveaways.cancel", "t,ts"),
        ("giveaways.restart", "t,t,t,ts,ts"),
        ("giveaways.setWeights", "t,json,ts"),
        ("giveaways.leave", "t,t"),
        ("giveaways.findByMessage", "t,t"),
        ("giveaways.activeByMessages", "t,arr"),
    ]
    .into();
    for (name, sql) in gs::RAW_SQL {
        assert!(!sql.contains("now()"), "{name}: no bare now()");
        let mut tx = pool.begin().await.expect("tx");
        let sig = sigs.get(name).unwrap_or_else(|| panic!("no sample parameters for {name}"));
        bind_sample(sqlx::query(*sql), sig).execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
    }
    for (name, sql, param) in ps::RAW_SQL {
        assert!(!sql.contains("now()"), "{name}: no bare now()");
        let mut tx = pool.begin().await.expect("tx");
        sqlx::query(*sql).bind(*param).execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
    }
}

#[tokio::test]
async fn giveaway_start_posts_schedules_and_respects_the_cap() {
    let Some(e) = Env::new("920000000000000001", 0).await else { return };
    let g = e.start("Nitro", 3_600_000).await;
    assert_eq!(g.status, GiveawayStatus::Active);
    let row = e.row(&g.id).await;
    assert_eq!(row.message_id, g.message_id);
    assert_eq!(row.channel_id, e.ch);
    let sends = e.ops.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["components"][0]["components"][0]["custom_id"], json!(format!("giveaway:join:{}", g.id)));
    assert_eq!(sends[0]["allowed_mentions"], json!({ "parse": [], "roles": [] }));
    assert!(sends[0].get("content").is_none(), "no ping role, no content");
    let job = e.t.scheduler().get_job(&gs::end_job_key(&g.id)).await.expect("end job");
    assert_eq!((job.r#type.as_str(), job.max_attempts), ("giveaway.end", 5));
    assert_eq!(job.payload, json!({ "giveawayId": g.id }));
    assert_eq!(job.run_at.timestamp_millis(), row.ends_at.timestamp_millis());

    // A send failure leaves nothing behind.
    *e.ops.fail_send.lock().unwrap_or_else(|x| x.into_inner()) = Some(50013);
    let err = gs::start_giveaway(&e.t, e.start_opts("Fails", 3_600_000)).await.expect_err("send fails");
    assert_eq!(err.code(), "SEND_FAILED");
    *e.ops.fail_send.lock().unwrap_or_else(|x| x.into_inner()) = None;
    let n: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Giveaway" WHERE "guildId" = $1"#).bind(e.g).fetch_one(e.t.db()).await.expect("count");
    assert_eq!(n, 1);

    // The cap counts ACTIVE and DRAFT only.
    sqlx::query(
        r#"INSERT INTO "Giveaway" ("id","guildId","channelId","hostId","prize","winnerCount","endsAt","status","updatedAt") SELECT 'cfill' || g, $1, $2, $3, 'x', 1, (now() AT TIME ZONE 'UTC'), (CASE WHEN g % 2 = 0 THEN 'ACTIVE' ELSE 'DRAFT' END)::"GiveawayStatus", (now() AT TIME ZONE 'UTC') FROM generate_series(1, 99) g"#,
    )
    .bind(e.g)
    .bind(&e.ch)
    .bind(MODERATOR)
    .execute(e.t.db())
    .await
    .expect("fill");
    assert_eq!(gs::start_giveaway(&e.t, e.start_opts("Over", 3_600_000)).await.expect_err("cap").code(), "LIMIT");
    sqlx::query(r#"UPDATE "Giveaway" SET "status" = 'ENDED' WHERE "id" = 'cfill2'"#).execute(e.t.db()).await.expect("end one");
    assert!(gs::start_giveaway(&e.t, e.start_opts("Fits", 3_600_000)).await.is_ok());
    e.done().await;
}

trait Desc {
    fn build_description(&self) -> String;
}

impl Desc for serenity::builder::CreateEmbed {
    fn build_description(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v["description"].as_str().map(str::to_string)).unwrap_or_default()
    }
}

#[tokio::test]
async fn giveaway_button_enters_leaves_and_checks_roles() {
    let Some(e) = Env::new("920000000000000002", 3).await else { return };
    let g = e.start("Keyboard", 3_600_000).await;
    let click = |n: u32| FakeInteraction::button(&format!("giveaway:join:{}", g.id)).guild(e.g).user(&user(n)).build();
    bot_core::router::route_interaction(&e.t, click(3)).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"]);
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Entered"));
    assert_eq!(calls[1].embed_description(), Some("You have entered the giveaway for **Keyboard**. Good luck! Click **Participate** again to leave."));
    assert_eq!(gs::entry_count(e.t.db(), &g.id).await.expect("count"), 1);
    e.t.responses.clear();
    bot_core::router::route_interaction(&e.t, click(3)).await;
    assert_eq!(e.t.responses.calls()[1].embed_title(), Some("ℹ️  Left Giveaway"));
    assert_eq!(gs::entry_count(e.t.db(), &g.id).await.expect("count"), 0);

    // Requirements: denied and required roles; a deleted role no longer restricts anyone.
    sqlx::query(r#"UPDATE "Giveaway" SET "requiredRoleIds" = $2, "deniedRoleIds" = $3 WHERE "id" = $1"#)
        .bind(&g.id)
        .bind(vec![ROLE_A.to_string(), "830000000000000099".to_string()])
        .bind(vec![ROLE_B.to_string()])
        .execute(e.t.db())
        .await
        .expect("roles");
    assert_eq!(e.join(&g.id, 2).await, gs::JoinOutcome::Denied { role_id: ROLE_B.into() });
    assert_eq!(e.join(&g.id, 3).await, gs::JoinOutcome::Missing { role_ids: vec![ROLE_A.into()] });
    assert_eq!(e.join(&g.id, 1).await, gs::JoinOutcome::Entered { prize: "Keyboard".into() });
    assert_eq!(
        gs::join_reply(&gs::JoinOutcome::Missing { role_ids: vec![ROLE_A.into()] }).build_description(),
        format!("You need the <@&{ROLE_A}> role to enter this giveaway.")
    );
    // Leaving is always allowed, even after losing a required role.
    sqlx::query(r#"UPDATE "Giveaway" SET "requiredRoleIds" = '{}', "deniedRoleIds" = '{}' WHERE "id" = $1"#).bind(&g.id).execute(e.t.db()).await.expect("roles");

    // Concurrent clicks never create a second entry and never fail.
    let outs = futures::future::join_all((0..6).map(|_| e.join(&g.id, 3))).await;
    assert!(outs.iter().all(|o| matches!(o, gs::JoinOutcome::Entered { .. } | gs::JoinOutcome::Left { .. })));
    let n: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "GiveawayEntry" WHERE "giveawayId" = $1 AND "userId" = $2"#)
        .bind(&g.id)
        .bind(user(3))
        .fetch_one(e.t.db())
        .await
        .expect("count");
    assert!(n <= 1, "{n}");

    // Closed and paused giveaways.
    sqlx::query(r#"UPDATE "Giveaway" SET "status" = 'DRAFT' WHERE "id" = $1"#).bind(&g.id).execute(e.t.db()).await.expect("draft");
    assert_eq!(e.join(&g.id, 3).await, gs::JoinOutcome::Paused);
    sqlx::query(r#"UPDATE "Giveaway" SET "status" = 'ENDED' WHERE "id" = $1"#).bind(&g.id).execute(e.t.db()).await.expect("ended");
    assert_eq!(e.join(&g.id, 3).await, gs::JoinOutcome::Closed);
    assert_eq!(gs::toggle_entry(&e.t, "920000000000000099", &g.id, &user(3), &[], &|_: &str| true).await.expect("ok"), gs::JoinOutcome::NotFound);
    e.done().await;
}

/// A counting XP provider.
#[derive(Default)]
struct XpBank(Mutex<Vec<(String, i64, String)>>);

#[async_trait]
impl RewardProvider for XpBank {
    async fn grant(&self, _app: &App, args: RewardArgs) -> BotResult<RewardResult> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push((args.user_id, args.amount, args.reason));
        Ok(RewardResult::ok(Some(args.amount)))
    }
    async fn take(&self, _app: &App, _args: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(None))
    }
}

#[tokio::test]
async fn giveaway_end_draws_eligible_entrants_claims_once_and_pays_once() {
    let Some(e) = Env::new("920000000000000003", 6).await else { return };
    let bank = Arc::new(XpBank::default());
    e.t.rewards().register_provider(RewardKind::Xp, bank.clone(), Some("test")).expect("provider");
    e.t.settings().update_settings(e.g, SettingsPatch::new().set("levelingEnabled", true)).await.expect("levels on");
    let mut o = e.start_opts("Mouse", 3_600_000);
    o.winner_count = 3;
    o.xp_prize = Some(500);
    o.denied_role_ids = vec![ROLE_B.into()];
    let g = gs::start_giveaway(&e.t, o).await.expect("started").giveaway;
    for n in [1, 3, 4, 5] {
        assert!(matches!(e.join(&g.id, n).await, gs::JoinOutcome::Entered { .. }));
    }
    // Member 2 holds the denied role; 629999999999999999 left the server; the bot is a bot.
    for u in [user(2), "629999999999999999".to_string(), TEST_BOT_ID.to_string()] {
        sqlx::query(r#"INSERT INTO "GiveawayEntry" ("id","giveawayId","userId") VALUES ($1,$2,$3)"#).bind(bot_db::new_cuid()).bind(&g.id).bind(u).execute(e.t.db()).await.expect("entry");
    }
    e.ops.clear();
    let (a, b) = tokio::join!(
        gs::end_giveaway(&e.t, e.g, &g.id, gs::EndSource::Command, 1),
        gs::end_giveaway(&e.t, e.g, &g.id, gs::EndSource::Command, 1)
    );
    let (a, b) = (a.expect("no db error"), b.expect("no db error"));
    let ended = match (&a, &b) {
        (Ok(x), Err(gs::GiveawayError::NotActive)) | (Err(gs::GiveawayError::NotActive), Ok(x)) => x.clone(),
        other => panic!("exactly one end claims: {other:?}"),
    };
    assert_eq!(ended.entry_count, 7);
    assert_eq!(ended.winner_ids.len(), 3);
    let eligible: HashSet<String> = [1, 3, 4, 5].into_iter().map(user).collect();
    assert!(ended.winner_ids.iter().all(|w| eligible.contains(w)), "{:?}", ended.winner_ids);
    assert!(ended.announced);
    assert!(ended.pay_warnings.is_empty(), "{:?}", ended.pay_warnings);
    let paid = bank.0.lock().unwrap_or_else(|x| x.into_inner()).clone();
    assert_eq!(paid.len(), 3, "each winner paid exactly once");
    assert!(paid.iter().all(|(_, amount, reason)| *amount == 500 && reason == "Giveaway prize: Mouse"));

    let row = e.row(&g.id).await;
    assert_eq!(row.status, GiveawayStatus::Ended);
    assert_eq!(row.winner_ids, ended.winner_ids);
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&g.id)).await.is_none(), "the end job is gone");
    let edits = e.ops.calls("edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].1["components"][0]["components"][0]["disabled"], json!(true));
    assert_eq!(edits[0].1["embeds"][0]["footer"]["text"], json!(format!("ID: {} · Ended at", g.id)));
    let sends = e.ops.sends();
    assert_eq!(sends.len(), 1, "one announcement, no warnings");
    let mut users: Vec<String> = serde_json::from_value(sends[0]["allowed_mentions"]["users"].clone()).unwrap_or_default();
    users.sort();
    let mut w = ended.winner_ids.clone();
    w.sort();
    assert_eq!(users, w, "pings the winners only");
    assert_eq!(sends[0]["message_reference"], json!({ "message_id": g.message_id, "fail_if_not_exists": false }));
    assert!(sends[0]["content"].as_str().unwrap_or("").ends_with("Each winner also gets **500 XP**."));

    // Reroll: excludes earlier winners, appends, pays the new winner only, then runs out.
    let r = gs::reroll_giveaway(&e.t, e.g, &g.id, 1).await.expect("db").expect("rerolled");
    assert_eq!(r.new_winner_ids.len(), 1);
    assert!(!ended.winner_ids.contains(&r.new_winner_ids[0]));
    assert_eq!(e.row(&g.id).await.winner_ids.len(), 4);
    assert_eq!(bank.0.lock().unwrap_or_else(|x| x.into_inner()).len(), 4);
    assert_eq!(gs::reroll_giveaway(&e.t, e.g, &g.id, 1).await.expect("db"), Err(gs::GiveawayError::NoEntrants));
    assert_eq!(gs::reroll_giveaway(&e.t, "920000000000000099", &g.id, 1).await.expect("db"), Err(gs::GiveawayError::NotFound));
    e.done().await;
}

#[tokio::test]
async fn giveaway_end_job_drafts_restarts_reschedules_and_ends() {
    let Some(e) = Env::new("920000000000000004", 2).await else { return };
    // Not due: rescheduled at endsAt.
    let g = e.start("Later", 3_600_000).await;
    let job = bot_core::scheduler::Job {
        id: "j".into(),
        guild_id: e.g.into(),
        job_type: "giveaway.end".into(),
        payload: json!({ "giveawayId": g.id }),
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 5,
        dedupe_key: Some(gs::end_job_key(&g.id)),
        version: 1,
    };
    let ctx = JobCtx { app: (*e.t).clone(), guild_id: e.g.into(), late_by_ms: 0, attempt: 1 };
    match gs::handle_end_job(job.clone(), ctx.clone()).await.expect("handled") {
        JobOutcome::Reschedule { next_run_at, payload: None } => {
            assert_eq!(next_run_at.timestamp_millis(), e.row(&g.id).await.ends_at.timestamp_millis())
        }
        other => panic!("{other:?}"),
    }
    let bad = bot_core::scheduler::Job { payload: json!({}), ..job.clone() };
    let err = gs::handle_end_job(bad, ctx.clone()).await.expect_err("permanent");
    assert!(format!("{err:#}").contains("giveaway.end: payload.giveawayId missing"), "{err:#}");

    // A deleted message drafts the giveaway and cancels its job.
    let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).naive_utc();
    sqlx::query(r#"UPDATE "Giveaway" SET "endsAt" = $2 WHERE "id" = $1"#).bind(&g.id).bind(past).execute(e.t.db()).await.expect("due");
    e.ops.delete_message(g.message_id.as_deref().unwrap_or(""));
    assert_eq!(gs::end_giveaway(&e.t, e.g, &g.id, gs::EndSource::Job, 1).await.expect("db"), Err(gs::GiveawayError::Drafted));
    let row = e.row(&g.id).await;
    assert_eq!((row.status, row.message_id), (GiveawayStatus::Draft, None));
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&g.id)).await.is_none());

    // Restart: a draft becomes ACTIVE again with a new message and job; entries are kept.
    sqlx::query(r#"INSERT INTO "GiveawayEntry" ("id","giveawayId","userId") VALUES ($1,$2,$3)"#).bind(bot_db::new_cuid()).bind(&g.id).bind(user(1)).execute(e.t.db()).await.expect("entry");
    let ends = chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis() + 600_000).expect("time");
    let msg = gs::restart_giveaway(&e.t, e.g, &g.id, e.channel(), ends, None).await.expect("restarted");
    let row = e.row(&g.id).await;
    assert_eq!((row.status, row.message_id.clone()), (GiveawayStatus::Active, Some(msg.id.clone())));
    assert_eq!(row.ends_at, ends);
    assert_eq!(e.ops.sends().last().map(|s| s["components"][0]["components"][0]["label"].clone()), Some(json!("Participate (1)")));
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&g.id)).await.is_some());
    assert_eq!(gs::restart_giveaway(&e.t, e.g, &g.id, e.channel(), ends, None).await.expect_err("not a draft").code(), "NOT_DRAFT");

    // The job ends it through the scheduler.
    sqlx::query(r#"UPDATE "Giveaway" SET "endsAt" = $2 WHERE "id" = $1"#).bind(&g.id).bind(past).execute(e.t.db()).await.expect("due");
    gs::schedule_end(&e.t, e.g, &g.id, chrono::Utc::now() - chrono::Duration::seconds(1)).await.expect("scheduled");
    let ran = e.t.scheduler().tick(&e.t, false).await;
    assert!(ran >= 1);
    let row = e.row(&g.id).await;
    assert_eq!(row.status, GiveawayStatus::Ended);
    assert_eq!(row.winner_ids, vec![user(1)]);
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&g.id)).await.is_none());
    e.done().await;
}

#[tokio::test]
async fn giveaway_cancel_odds_hooks_and_refs() {
    let Some(e) = Env::new("920000000000000005", 1).await else { return };
    let a = e.start("Cancel me", 3_600_000).await;
    assert_eq!(gs::cancel_giveaway(&e.t, e.g, &a.id).await.expect("db"), Ok(()));
    let row = e.row(&a.id).await;
    assert_eq!(row.status, GiveawayStatus::Cancelled);
    assert!(row.ended_at.is_some());
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&a.id)).await.is_none());
    let edit = e.ops.calls("edit").pop().expect("the message was edited");
    assert_eq!(edit.1["embeds"][0]["title"], json!("🚫 Cancel me"));
    assert_eq!(gs::cancel_giveaway(&e.t, e.g, &a.id).await.expect("db"), Err(gs::GiveawayError::NotOpen));

    // Odds: add, replace, remove, refresh the embed, only open giveaways.
    let b = e.start("Odds", 3_600_000).await;
    e.ops.clear();
    let list = gs::update_odds(&e.t, e.g, &b.id, gs::OddsOp::Add { weight: 3 }, ROLE_A, None).await.expect("db").expect("added");
    assert_eq!(list, vec![gs::RoleWeight { role_id: ROLE_A.into(), weight: 3 }]);
    let list = gs::update_odds(&e.t, e.g, &b.id, gs::OddsOp::Add { weight: 5 }, ROLE_A, None).await.expect("db").expect("replaced");
    assert_eq!(list[0].weight, 5);
    let edit = e.ops.calls("edit").pop().expect("embed refreshed");
    assert!(edit.1.get("components").is_none(), "only the embed is edited");
    assert!(edit.1["embeds"][0]["description"].as_str().unwrap_or("").contains(&format!("**Bonus odds:** <@&{ROLE_A}> ×5")));
    assert_eq!(gs::update_odds(&e.t, e.g, &b.id, gs::OddsOp::Remove, ROLE_B, None).await.expect("db"), Err(gs::GiveawayError::NotSet));
    assert_eq!(gs::update_odds(&e.t, e.g, &b.id, gs::OddsOp::Remove, ROLE_A, None).await.expect("db"), Ok(vec![]));
    assert_eq!(gs::update_odds(&e.t, e.g, &a.id, gs::OddsOp::Remove, ROLE_A, None).await.expect("db"), Err(gs::GiveawayError::NotOpen));

    // References: by id, message id and link, scoped to the guild.
    let mid = b.message_id.clone().unwrap_or_default();
    for input in [b.id.clone(), mid.clone(), format!("https://discord.com/channels/{}/{}/{mid}", e.g, e.ch)] {
        assert_eq!(gs::resolve_giveaway_ref(e.t.db(), e.g, &input).await.expect("db").map(|g| g.id), Some(b.id.clone()), "{input}");
    }
    assert!(gs::resolve_giveaway_ref(e.t.db(), "920000000000000099", &b.id).await.expect("db").is_none());
    let foreign = format!("https://discord.com/channels/920000000000000099/{}/{mid}", e.ch);
    assert!(gs::resolve_giveaway_ref(e.t.db(), e.g, &foreign).await.expect("db").is_none());

    // Hooks: a deleted message drafts an ACTIVE giveaway (single and bulk) and cancels its job; on a cancelled one
    // only the dangling messageId is cleared.
    e.t.run_hooks(MessageDelete { guild_id: e.gid(), message_id: mid_of(&mid), channel_id: e.cid(), message: None, partial: true, channel: None }).await;
    let row = e.row(&b.id).await;
    assert_eq!((row.status, row.message_id), (GiveawayStatus::Draft, None));
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&b.id)).await.is_none());
    let c = e.start("Bulk", 3_600_000).await;
    let cmid = c.message_id.clone().unwrap_or_default();
    e.t.run_hooks(MessageDeleteBulk { guild_id: e.gid(), channel_id: e.cid(), message_ids: vec![mid_of(&cmid), mid_of("5")], messages: vec![], channel: None })
        .await;
    assert_eq!(e.row(&c.id).await.status, GiveawayStatus::Draft);
    assert!(e.t.scheduler().get_job(&gs::end_job_key(&c.id)).await.is_none());
    let amid = a.message_id.clone().unwrap_or_default();
    e.t.run_hooks(MessageDelete { guild_id: e.gid(), message_id: mid_of(&amid), channel_id: e.cid(), message: None, partial: true, channel: None }).await;
    let row = e.row(&a.id).await;
    assert_eq!((row.status, row.message_id), (GiveawayStatus::Cancelled, None));
    // The filter: someone else's cached message is skipped.
    let other = bot_core::testing::fake_message("719999999999999999", e.g, &e.ch, &user(1), "hi");
    assert!(!feat_giveaways_polls::plugins::giveaways::is_own_or_unknown(&e.t, Some(&other)));
    assert!(feat_giveaways_polls::plugins::giveaways::is_own_or_unknown(&e.t, None));
    e.done().await;
}

fn moderator(name: &str, g: &str, ch: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user(MODERATOR).roles(&[ROLE_MOD]).permissions(P::MODERATE_MEMBERS).channel(ch)
}

fn moderator_autocomplete(name: &str, g: &str, ch: &str) -> FakeInteraction {
    FakeInteraction::autocomplete(name).guild(g).user(MODERATOR).roles(&[ROLE_MOD]).permissions(P::MODERATE_MEMBERS).channel(ch)
}

#[tokio::test]
async fn giveaway_command_start_end_list_entrants_and_autocomplete() {
    let Some(e) = Env::new("920000000000000006", 2).await else { return };
    let route = |i| bot_core::router::route_interaction(&e.t, i);
    let start = || moderator("giveaway", e.g, &e.ch).subcommand("start");
    route(start().option("prize", option_type::STRING, json!("Gift")).option("duration", option_type::STRING, json!("2h")).option("bonus_weight", option_type::INTEGER, json!(3)).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls.get(1).and_then(|c| c.embed_title()), Some("❌  Missing Role"), "{calls:?}");
    assert!(calls[0].ephemeral());

    e.t.responses.clear();
    route(start().option("prize", option_type::STRING, json!("Gift")).option("duration", option_type::STRING, json!("10s")).build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_title(), Some("❌  Too Short"));

    e.t.responses.clear();
    route(start().option("prize", option_type::STRING, json!("Gift")).option("duration", option_type::STRING, json!("1h")).option("xp", option_type::INTEGER, json!(10)).build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_title(), Some("❌  Prize Unavailable"));

    e.t.responses.clear();
    route(start().option("prize", option_type::STRING, json!("  Gift card  ")).option("duration", option_type::STRING, json!("2h")).option("color", option_type::STRING, json!("blurple")).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Giveaway Started"), "{calls:?}");
    let rows = gs::list_giveaways(e.t.db(), e.g, gs::ListFilter::Open).await.expect("list");
    assert_eq!(rows.len(), 1);
    let id = rows[0].id.clone();
    assert!(calls[1].embed_description().unwrap_or("").contains(&format!("ID `{id}`")));
    let row = e.row(&id).await;
    assert_eq!(row.prize, "Gift card");
    assert_eq!(row.embed_color, Some(0x5865F2));

    // Autocomplete: by prize; statuses per subcommand.
    e.t.responses.clear();
    route(moderator_autocomplete("giveaway", e.g, &e.ch).subcommand("end").focused("giveaway", "gift").build()).await;
    let choices = e.t.responses.calls()[0].choices();
    assert_eq!(choices.len(), 1, "{choices:?}");
    assert!(format!("{choices:?}").contains("🟢 Gift card · #giveaways · ends in "), "{choices:?}");
    e.t.responses.clear();
    route(moderator_autocomplete("giveaway", e.g, &e.ch).subcommand("reroll").focused("giveaway", "").build()).await;
    assert!(e.t.responses.calls()[0].choices().is_empty(), "nothing has ended yet");

    // Entrants, then end.
    for n in [1, 2] {
        e.join(&id, n).await;
    }
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("entrants").option("giveaway", option_type::STRING, json!(id)).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("🎉 Entrants: Gift card"));
    assert!(calls[1].embed_description().unwrap_or("").starts_with("**Status:** running\n\n1. <@"));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("end").option("giveaway", option_type::STRING, json!(id)).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Giveaway Ended"), "{calls:?}");
    assert!(calls[1].embed_description().unwrap_or("").starts_with("Winners of **Gift card**: <@"));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("end").option("giveaway", option_type::STRING, json!(id)).build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_description(), Some("That giveaway is already ended."));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("end").option("giveaway", option_type::STRING, json!("nope")).build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_description(), Some(feat_giveaways_polls::commands::giveaway::NOT_FOUND_TEXT));

    // List.
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("list").build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_title(), Some("ℹ️  No Giveaways"));
    assert!(e.t.responses.calls()[1].embed_description().unwrap_or("").starts_with("There are no open (active or draft) giveaways."));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("list").option("status", option_type::STRING, json!("ended")).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("🎉 Giveaways"));
    assert!(calls[1].embed_description().unwrap_or("").starts_with("🏁 **Gift card** · <#"), "{:?}", calls[1].embed_description());
    e.done().await;
}

// ── Database: polls ───────────────────────────────────────────────────────────

fn pending_for(e: &Env, max_choices: i64, duration_ms: Option<i64>) -> ps::PendingPoll {
    ps::PendingPoll {
        guild_id: e.g.into(),
        channel_id: e.ch.clone(),
        user_id: MODERATOR.into(),
        max_choices,
        allowed_role_ids: vec![],
        denied_role_ids: vec![],
        hide_results: false,
        duration_ms,
    }
}

fn input(options: &[&str], max_choices: i64) -> ps::PollInput {
    ps::PollInput { question: "Pick one?".into(), options: options.iter().map(|s| s.to_string()).collect(), max_choices }
}

async fn post(e: &Env, pending: &ps::PendingPoll, inp: &ps::PollInput) -> Poll {
    ps::post_poll(&e.t, e.g, &e.channel(), pending, inp).await.expect("db").expect("posted").poll
}

async fn poll_row(e: &Env, id: &str) -> Poll {
    ps::find_poll(e.t.db(), id).await.expect("db").expect("row")
}

fn vote_click(e: &Env, poll: &Poll, idx: usize, n: u32) -> FakeInteraction {
    let roles = member_roles(n);
    let refs: Vec<&str> = roles.iter().map(String::as_str).collect();
    FakeInteraction::button(&format!("poll:vote:{}:{idx}", poll.id)).guild(e.g).user(&user(n)).roles(&refs).channel(&e.ch)
}

async fn reply_of(e: &Env, i: FakeInteraction) -> String {
    e.t.responses.clear();
    bot_core::router::route_interaction(&e.t, i.build()).await;
    let calls = e.t.responses.calls();
    calls.last().and_then(|c| c.content().map(str::to_string)).unwrap_or_else(|| format!("{calls:?}"))
}

#[tokio::test]
async fn poll_cap_is_exact_under_parallel_creates() {
    let Some(e) = Env::new("920000000000000011", 0).await else { return };
    sqlx::query(
        r#"INSERT INTO "Poll" ("id","guildId","channelId","authorId","question","options","maxChoices","updatedAt") SELECT 'cpfill' || g, $1, $2, $3, 'q', ARRAY['a','b'], 1, (now() AT TIME ZONE 'UTC') FROM generate_series(1, 48) g"#,
    )
    .bind(e.g)
    // A channel that is not cached and that REST reports as Unknown Channel (deleted).
    .bind("849999999999999999")
    .bind(MODERATOR)
    .execute(e.t.db())
    .await
    .expect("fill");
    let pending = pending_for(&e, 1, None);
    let inp = input(&["a", "b"], 1);
    let outs = futures::future::join_all((0..5).map(|_| ps::create_poll_row(&e.t, e.g, &pending, &inp))).await;
    let created = outs.into_iter().map(|r| r.expect("db")).filter(Option::is_some).count();
    assert_eq!(created, 2, "the cap of 50 is exact");
    // On LIMIT, the polls whose channel REST reports deleted are ended, then the create is retried; polls in a
    // cached channel are never judged.
    let p = ps::create_poll(&e.t, e.g, &pending_for(&e, 1, None), &inp).await.expect("db");
    assert!(p.is_some(), "reconciled the polls whose channel is gone, then created");
    let n: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ACTIVE'"#).bind(e.g).fetch_one(e.t.db()).await.expect("count");
    assert_eq!(n, 3, "the 48 polls of the deleted channel were ended");
    e.done().await;
}

#[tokio::test]
async fn poll_votes_single_choice_multi_choice_and_roles() {
    let Some(e) = Env::new("920000000000000012", 3).await else { return };
    let p = post(&e, &pending_for(&e, 1, None), &input(&["A", "B", "C"], 1)).await;
    assert_eq!(reply_of(&e, vote_click(&e, &p, 0, 1)).await, "✅ You voted for **1. A**.");
    assert_eq!(e.t.responses.calls()[0].method, "deferReply");
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(reply_of(&e, vote_click(&e, &p, 1, 1)).await, "🔁 Your vote changed to **2. B**.");
    assert_eq!(reply_of(&e, vote_click(&e, &p, 1, 1)).await, "🗑️ Your vote for **2. B** was removed.");
    assert_eq!(reply_of(&e, vote_click(&e, &p, 7, 1)).await, "That option no longer exists.");

    // Concurrent clicks never leave a member with two votes in a single-choice poll.
    let u2 = user(2);
    let outs = futures::future::join_all((0..3).map(|i| ps::apply_vote(&e.t, &p.id, &u2, 1, ps::VoteInput::Toggle(i)))).await;
    assert!(outs.iter().all(Result::is_ok));
    let n: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "PollVote" WHERE "pollId" = $1 AND "userId" = $2"#).bind(&p.id).bind(user(2)).fetch_one(e.t.db()).await.expect("count");
    assert_eq!(n, 1);

    // Multi choice: the button limit, the select replacing the set, the personal ballot edited in place.
    let m = post(&e, &pending_for(&e, 2, None), &input(&["A", "B", "C"], 2)).await;
    assert_eq!(reply_of(&e, vote_click(&e, &m, 0, 1)).await, "✅ Vote added. Your votes (1/2): **1. A**");
    assert_eq!(reply_of(&e, vote_click(&e, &m, 1, 1)).await, "✅ Vote added. Your votes (2/2): **1. A**, **2. B**");
    assert_eq!(reply_of(&e, vote_click(&e, &m, 2, 1)).await, "You can pick at most **2** options. Remove a vote first.");
    e.t.responses.clear();
    bot_core::router::route_interaction(&e.t, FakeInteraction::select(&format!("poll-select:{}", m.id), &["2", "0"]).guild(e.g).user(&user(1)).roles(&[ROLE_A]).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"]);
    assert_eq!(calls[1].content(), Some("✅ Your votes (2/2): **1. A**, **3. C**"));
    let opts = &calls[1].body["components"][0]["components"][0]["options"];
    assert_eq!((opts[0]["default"].clone(), opts[1]["default"].clone(), opts[2]["default"].clone()), (json!(true), json!(false), json!(true)));
    let mut sel = FakeInteraction::select(&format!("poll-select:{}", m.id), &[]).guild(e.g).user(&user(1)).roles(&[ROLE_A]);
    sel.json_mut()["message"]["flags"] = json!(64);
    e.t.responses.clear();
    bot_core::router::route_interaction(&e.t, sel.build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferUpdate", "editReply"], "the ballot is edited in place");
    assert_eq!(calls[1].content(), Some("🗑️ Your votes were cleared."));
    let mut bad = FakeInteraction::select(&format!("poll-select:{}", m.id), &["0", "1", "2"]).guild(e.g).user(&user(1)).roles(&[ROLE_A]);
    bad.json_mut()["message"]["flags"] = json!(64);
    e.t.responses.clear();
    bot_core::router::route_interaction(&e.t, bad.build()).await;
    assert_eq!(e.t.responses.calls()[1].content(), Some("That selection is not valid for this poll."));

    // Allowed and denied roles are checked at vote time.
    let mut pend = pending_for(&e, 1, None);
    pend.allowed_role_ids = vec![ROLE_A.into()];
    pend.denied_role_ids = vec![ROLE_B.into()];
    let r = post(&e, &pend, &input(&["A", "B"], 1)).await;
    assert_eq!(reply_of(&e, vote_click(&e, &r, 0, 3)).await, format!("Only members with <@&{ROLE_A}> can vote in this poll."));
    assert_eq!(reply_of(&e, vote_click(&e, &r, 0, 2)).await, format!("Members with <@&{ROLE_B}> can't vote in this poll."));
    assert_eq!(reply_of(&e, vote_click(&e, &r, 0, 1)).await, "✅ You voted for **1. A**.");
    e.done().await;
}

#[tokio::test]
async fn poll_end_has_one_winner_edits_posts_and_refuses_later_votes() {
    let Some(e) = Env::new("920000000000000013", 3).await else { return };
    let p = post(&e, &pending_for(&e, 1, Some(3_600_000)), &input(&["A", "B", "C"], 1)).await;
    let job = e.t.scheduler().get_job(&ps::end_job_key(&p.id)).await.expect("end job");
    assert_eq!((job.r#type.as_str(), job.payload.clone()), ("poll.end", json!({ "pollId": p.id })));
    assert_eq!(job.run_at.timestamp_millis(), p.ends_at.map(|x| x.timestamp_millis()).unwrap_or(0));
    for (n, i) in [(1, 0), (2, 0), (3, 1)] {
        ps::apply_vote(&e.t, &p.id, &user(n), 1, ps::VoteInput::Toggle(i)).await.expect("vote");
    }
    e.ops.clear();
    let (a, b) = tokio::join!(
        ps::end_poll(&e.t, &p.id, ps::EndReason::Manual, Some(e.g)),
        ps::end_poll(&e.t, &p.id, ps::EndReason::Manual, Some(e.g))
    );
    let (a, b) = (a.expect("db"), b.expect("db"));
    let ended = match (a, b) {
        (Ok(x), Err(ps::EndPollError::AlreadyEnded)) | (Err(ps::EndPollError::AlreadyEnded), Ok(x)) => x,
        other => panic!("one winner: {other:?}"),
    };
    assert_eq!(ended.tally.as_ref().map(|t| t.counts.clone()), Some(vec![2, 1, 0]));
    assert!(ended.posted && !ended.message_gone);
    let edits = e.ops.calls("edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].1["components"][0]["components"][0]["disabled"], json!(true));
    let sends = e.ops.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["embeds"][0]["title"], json!("📊 Poll results"));
    assert_eq!(sends[0]["message_reference"], json!({ "message_id": p.message_id, "fail_if_not_exists": false }));
    assert!(sends[0]["embeds"][0]["description"].as_str().unwrap_or("").contains("🏆 **A** won with **2** votes (67%)."));
    assert!(e.t.scheduler().get_job(&ps::end_job_key(&p.id)).await.is_none(), "a manual end cancels the job");
    assert_eq!(ps::apply_vote(&e.t, &p.id, &user(1), 1, ps::VoteInput::Toggle(2)).await.expect("vote"), ps::VoteOutcome::Ended);
    assert_eq!(reply_of(&e, vote_click(&e, &p, 2, 1)).await, "This poll has ended.");

    // An ended poll whose message still has live buttons is healed by the next click.
    e.ops.clear();
    let mut click = vote_click(&e, &p, 0, 1).message_id(p.message_id.as_deref().unwrap_or(""));
    click.json_mut()["message"]["components"] = json!([{ "type": 1, "components": [{ "type": 2, "custom_id": format!("poll:vote:{}:0", p.id), "style": 2, "label": "1. A", "disabled": false }] }]);
    assert_eq!(reply_of(&e, click).await, "This poll has ended.");
    assert_eq!(e.ops.calls("edit").len(), 1, "the message was finalised");

    // An overdue poll is ended by the vote that finds it.
    let q = post(&e, &pending_for(&e, 1, Some(3_600_000)), &input(&["A", "B"], 1)).await;
    sqlx::query(r#"UPDATE "Poll" SET "endsAt" = $2 WHERE "id" = $1"#).bind(&q.id).bind((chrono::Utc::now() - chrono::Duration::seconds(5)).naive_utc()).execute(e.t.db()).await.expect("overdue");
    assert_eq!(reply_of(&e, vote_click(&e, &q, 0, 1)).await, "This poll has ended.");
    assert_eq!(poll_row(&e, &q.id).await.status, PollStatus::Ended);

    // Module off: the poll closes but no results are posted.
    let r = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    e.t.settings().update_settings(e.g, SettingsPatch::new().set("pollsEnabled", false)).await.expect("off");
    e.ops.clear();
    let done = ps::end_poll(&e.t, &r.id, ps::EndReason::Manual, Some(e.g)).await.expect("db").expect("ended");
    assert!(!done.posted);
    assert!(e.ops.sends().is_empty());
    assert_eq!(e.ops.calls("edit").len(), 1, "the message is still finalised");
    assert_eq!(ps::end_poll(&e.t, "cnope", ps::EndReason::Manual, Some(e.g)).await.expect("db"), Err(ps::EndPollError::NotFound));
    e.done().await;
}

#[tokio::test]
async fn poll_hooks_job_and_live_render() {
    let Some(e) = Env::new("920000000000000014", 2).await else { return };
    // Live render: fresh counts onto the message.
    let p = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    ps::apply_vote(&e.t, &p.id, &user(1), 1, ps::VoteInput::Toggle(1)).await.expect("vote");
    e.ops.clear();
    assert_eq!(ps::render_poll_message(&e.t, &p.id).await.expect("render"), ps::RenderOutcome::Ok);
    let edit = e.ops.calls("edit").pop().expect("edited");
    assert!(edit.1["embeds"][0]["description"].as_str().unwrap_or("").contains("**100%** · 1 vote"));
    e.ops.delete_message(p.message_id.as_deref().unwrap_or(""));
    assert_eq!(ps::render_poll_message(&e.t, &p.id).await.expect("render"), ps::RenderOutcome::Gone);

    // messageDelete ends the poll without a results message.
    e.ops.clear();
    e.t.run_hooks(MessageDelete { guild_id: e.gid(), message_id: mid_of(p.message_id.as_deref().unwrap_or("1")), channel_id: e.cid(), message: None, partial: true, channel: None }).await;
    assert_eq!(poll_row(&e, &p.id).await.status, PollStatus::Ended);
    assert!(e.ops.sends().is_empty());
    // Bulk delete and channel delete.
    let b = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    e.t.run_hooks(MessageDeleteBulk { guild_id: e.gid(), channel_id: e.cid(), message_ids: vec![mid_of(b.message_id.as_deref().unwrap_or("1"))], messages: vec![], channel: None }).await;
    assert_eq!(poll_row(&e, &b.id).await.status, PollStatus::Ended);
    let c = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: e.channel() }).await;
    assert_eq!(poll_row(&e, &c.id).await.status, PollStatus::Ended);
    assert!(e.ops.sends().iter().all(|s| s["embeds"][0]["title"] != json!("📊 Poll results")), "deleted polls post nothing");

    // The job ends the poll (and keeps its own row until the scheduler deletes it).
    let d = post(&e, &pending_for(&e, 1, Some(3_600_000)), &input(&["A", "B"], 1)).await;
    sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = $2 WHERE "dedupeKey" = $1"#).bind(ps::end_job_key(&d.id)).bind((chrono::Utc::now() - chrono::Duration::seconds(1)).naive_utc()).execute(e.t.db()).await.expect("due");
    e.ops.clear();
    assert!(e.t.scheduler().tick(&e.t, false).await >= 1);
    assert_eq!(poll_row(&e, &d.id).await.status, PollStatus::Ended);
    assert!(e.t.scheduler().get_job(&ps::end_job_key(&d.id)).await.is_none());
    assert_eq!(e.ops.sends().len(), 1, "the results were posted");
    e.done().await;
}

#[tokio::test]
async fn poll_modal_and_commands() {
    let Some(e) = Env::new("920000000000000015", 1).await else { return };
    let route = |i| bot_core::router::route_interaction(&e.t, i);
    e.t.ext::<ps::PendingPolls>().0.put("42", pending_for(&e, 5, Some(7_200_000)), e.t.clock().now_ms());
    route(FakeInteraction::modal("poll-modal:42", &[("poll-question", "  Lunch?  "), ("poll-options", "1. Pizza\n2. Sushi\n3. Salad")]).guild(e.g).user(MODERATOR).roles(&[ROLE_MOD]).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"], "{calls:?}");
    assert_eq!(calls[1].embed_title(), Some("✅  Poll Posted"));
    let desc = calls[1].embed_description().unwrap_or("").to_string();
    assert!(desc.starts_with(&format!("Your poll is live in <#{}>. [Jump to poll](https://discord.com/channels/{}/{}/", e.ch, e.g, e.ch)), "{desc}");
    assert!(desc.contains("\nIt ends <t:"));
    assert!(desc.ends_with("Max choices was lowered to **3** (the number of options)."));
    let (polls, _) = ps::list_active_polls(e.t.db(), e.g).await.expect("list");
    assert_eq!(polls.len(), 1);
    let p = polls[0].clone();
    assert_eq!((p.question.as_str(), p.options.clone(), p.max_choices), ("Lunch?", vec!["Pizza".to_string(), "Sushi".into(), "Salad".into()], 3));
    assert!(p.message_id.is_some());
    assert!(e.t.scheduler().get_job(&ps::end_job_key(&p.id)).await.is_some());

    // /poll results (private peek), list, autocomplete, end.
    ps::apply_vote(&e.t, &p.id, &user(1), 3, ps::VoteInput::Set(vec![0, 2])).await.expect("vote");
    let slash = |sub: &str| moderator("poll", e.g, &e.ch).subcommand(sub);
    e.t.responses.clear();
    route(slash("results").option("poll", option_type::STRING, json!(p.id)).build()).await;
    let c = &e.t.responses.calls()[1];
    assert_eq!(c.embed_title(), Some("📊 Live results"));
    assert!(c.embed_description().unwrap_or("").contains("🗳️ **2** votes from **1** member · up to 3 choices per member"));
    e.t.responses.clear();
    route(slash("list").build()).await;
    let c = &e.t.responses.calls()[1];
    assert_eq!(c.embed_title(), Some("📊 Active polls (1/50)"));
    assert!(c.embed_description().unwrap_or("").starts_with("**1.** [Lunch?](https://discord.com/channels/"));
    e.t.responses.clear();
    route(moderator_autocomplete("poll", e.g, &e.ch).subcommand("end").focused("poll", "lun").build()).await;
    let choices = e.t.responses.calls()[0].choices();
    assert_eq!(choices.len(), 1, "{choices:?}");
    assert_eq!(choices[0]["value"], json!(p.id));
    e.t.responses.clear();
    route(moderator_autocomplete("poll", e.g, &e.ch).subcommand("results").focused("poll", "nothing like this").build()).await;
    assert!(e.t.responses.calls()[0].choices().is_empty());
    e.t.responses.clear();
    route(slash("end").option("poll", option_type::STRING, json!(p.id)).build()).await;
    let c = &e.t.responses.calls()[1];
    assert_eq!(c.embed_title(), Some("✅  Poll Ended"));
    assert_eq!(c.embed_description(), Some(format!("The results were posted in <#{}>.", e.ch).as_str()));
    e.t.responses.clear();
    route(slash("end").option("poll", option_type::STRING, json!(p.id)).build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_title(), Some("⚠️  Already Ended"));
    e.t.responses.clear();
    route(slash("list").build()).await;
    assert_eq!(e.t.responses.calls()[1].embed_title(), Some("ℹ️  No Active Polls"));
    e.done().await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

const JS: &str = "rust/fixtures/gen/feat-giveaways-polls-js.mjs";

fn js(args: &[&str]) -> Value {
    let v = run_node(JS, args);
    assert!(v.get("error").is_none(), "JS {args:?}: {v}");
    v
}

#[tokio::test]
async fn a_giveaway_started_by_the_js_bot_ends_under_rust() {
    let Some(e) = Env::new("920000000000000021", 3).await else { return };
    let due = (chrono::Utc::now().timestamp_millis() + 3_600_000).to_string();
    let opts = format!(r#"{{"prize":"JS prize","winnerCount":2,"endsAtMs":{due},"roleWeights":[{{"roleId":"{ROLE_A}","weight":3}}],"roles":["{ROLE_A}"]}}"#);
    let started = js(&["gstart", e.g, &e.ch, &opts]);
    let id = started["giveaway"]["id"].as_str().expect("id").to_string();
    assert_eq!(started["job"]["type"], json!("giveaway.end"));
    assert_eq!(started["job"]["payload"], json!({ "giveawayId": id }));
    assert_eq!(started["job"]["maxAttempts"], json!(5));

    // The message body the JS posted is the one Rust builds for the same row.
    let row = e.row(&id).await;
    let exists = |r: &str| r == ROLE_A;
    let ours = gs::giveaway_message_body(&row, None, 0, &[], &exists, chrono::Utc::now());
    assert_same(&ours, &started["sent"][0], "the giveaway message");

    // Entries through the JS button handler.
    for n in [1, 3] {
        let roles = serde_json::to_string(&member_roles(n)).unwrap_or_default();
        let r = js(&["gjoin", e.g, &id, &user(n), &roles]);
        assert_eq!(r["title"], json!("✅  Entered"), "{r}");
    }
    // A click on the JS-posted Participate button is handled by Rust.
    bot_core::router::route_interaction(&e.t, FakeInteraction::button(&format!("giveaway:join:{id}")).guild(e.g).user(&user(2)).roles(&[ROLE_B]).build()).await;
    assert_eq!(e.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).as_deref(), Some("✅  Entered"));
    // Time passes: the giveaway and its JS-written job are due; the job runs under Rust.
    let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).naive_utc();
    sqlx::query(r#"UPDATE "Giveaway" SET "endsAt" = $2 WHERE "id" = $1"#).bind(&id).bind(past).execute(e.t.db()).await.expect("due");
    sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = $2 WHERE "dedupeKey" = $1"#).bind(gs::end_job_key(&id)).bind(past).execute(e.t.db()).await.expect("due");
    assert!(e.t.scheduler().tick(&e.t, false).await >= 1);
    let read = js(&["gread", &id]);
    assert_eq!(read["giveaway"]["status"], json!("ENDED"));
    let winners: Vec<String> = serde_json::from_value(read["giveaway"]["winnerIds"].clone()).unwrap_or_default();
    let entrants: HashSet<String> = [user(1), user(2), user(3)].into();
    assert_eq!(winners.len(), 2);
    assert!(winners.iter().all(|w| entrants.contains(w)), "drawn from the JS and Rust entrants: {winners:?}");
    assert_eq!(read["job"], Value::Null, "the end job is gone");
    assert_eq!(read["entries"].as_array().map(Vec::len), Some(3));
    let rust = e.row(&id).await;
    assert_eq!(serde_json::to_value(&rust).unwrap_or_default(), read["giveaway"], "the row reads the same through Prisma and Rust");
    let sends = e.ops.sends();
    assert!(sends.iter().any(|s| s["content"].as_str().unwrap_or("").starts_with("🎉 Congratulations ")), "{sends:?}");
    e.done().await;
}

#[tokio::test]
async fn a_giveaway_run_by_rust_reads_and_rerolls_in_the_js_bot() {
    let Some(e) = Env::new("920000000000000022", 3).await else { return };
    let mut o = e.start_opts("Rust prize", 3_600_000);
    o.role_weights = vec![gs::RoleWeight { role_id: ROLE_B.into(), weight: 4 }];
    o.description = Some("From Rust".into());
    let g = gs::start_giveaway(&e.t, o).await.expect("started").giveaway;
    for n in [1, 2, 3] {
        e.join(&g.id, n).await;
    }
    let read = js(&["gread", &g.id]);
    assert_eq!(read["giveaway"], serde_json::to_value(e.row(&g.id).await).unwrap_or_default(), "Prisma reads the Rust row");
    assert_eq!(read["giveaway"]["roleWeights"], json!([{ "roleId": ROLE_B, "weight": 4 }]));
    assert_eq!(read["job"]["type"], json!("giveaway.end"));
    assert_eq!(read["job"]["payload"], json!({ "giveawayId": g.id }));
    assert_eq!(read["job"]["maxAttempts"], json!(5));
    assert_eq!(read["entries"].as_array().map(Vec::len), Some(3));

    let ended = gs::end_giveaway(&e.t, e.g, &g.id, gs::EndSource::Command, 1).await.expect("db").expect("ended");
    assert_eq!(ended.winner_ids.len(), 1);
    // The JS bot rerolls what Rust ended: it sees the entries made up to endedAt and excludes the Rust winner.
    let members = json!([[user(1), member_roles(1)], [user(2), member_roles(2)], [user(3), member_roles(3)]]).to_string();
    let r = js(&["greroll", e.g, &g.id, &members]);
    assert_eq!(r["res"]["ok"], json!(true), "{r}");
    let new: Vec<String> = serde_json::from_value(r["res"]["newWinnerIds"].clone()).unwrap_or_default();
    assert_eq!(new.len(), 1);
    assert!(!ended.winner_ids.contains(&new[0]));
    assert_eq!(e.row(&g.id).await.winner_ids.len(), 2, "Rust reads the JS reroll");
    e.done().await;
}

#[tokio::test]
async fn a_poll_posted_by_the_js_bot_ends_under_rust() {
    let Some(e) = Env::new("920000000000000023", 0).await else { return };
    let pending = json!({ "guildId": e.g, "channelId": e.ch, "userId": MODERATOR, "maxChoices": 2, "allowedRoleIds": [], "deniedRoleIds": [], "hideResults": true, "durationMs": 3_600_000 }).to_string();
    let inp = json!({ "question": "JS poll?", "options": ["Red", "Green", "Blue"], "maxChoices": 2 }).to_string();
    let started = js(&["pstart", e.g, &e.ch, &pending, &inp]);
    let id = started["out"]["poll"]["id"].as_str().expect("id").to_string();
    assert_eq!(started["job"]["type"], json!("poll.end"));
    assert_eq!(started["job"]["payload"], json!({ "pollId": id }));
    let p = poll_row(&e, &id).await;
    assert_same(&ps::build_poll_payload_default(&p, &ps::empty_tally(3), None), &started["sent"][0], "the poll message");
    for (u, v) in [("1", r#"{"set":[0,2]}"#), ("2", r#"{"idx":2}"#), ("3", r#"{"idx":1}"#)] {
        let r = js(&["pvote", &id, &user(u.parse().unwrap_or(0)), "2", v]);
        assert_eq!(r["changed"], json!(true), "{r}");
    }
    // Rust reads the JS votes and toggles on top of them.
    let t = ps::get_tally(e.t.db(), &p).await.expect("tally");
    assert_eq!(t, ps::Tally { counts: vec![1, 1, 2], votes: 4, voters: 3 });
    assert_eq!(
        ps::apply_vote(&e.t, &id, &user(1), 2, ps::VoteInput::Toggle(2)).await.expect("vote"),
        ps::VoteOutcome::Applied { result: "removed", next: vec![0], changed: true }
    );
    // A pick in the JS-posted select menu is handled by Rust.
    bot_core::router::route_interaction(&e.t, FakeInteraction::select(&format!("poll-select:{id}"), &["1"]).guild(e.g).user(&user(9)).build()).await;
    assert_eq!(e.t.responses.calls().last().and_then(|c| c.content().map(str::to_string)).as_deref(), Some("✅ Your votes (1/2): **2. Green**"));
    // The JS job runs under Rust.
    sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = $2 WHERE "dedupeKey" = $1"#).bind(ps::end_job_key(&id)).bind((chrono::Utc::now() - chrono::Duration::seconds(1)).naive_utc()).execute(e.t.db()).await.expect("due");
    e.ops.clear();
    assert!(e.t.scheduler().tick(&e.t, false).await >= 1);
    let read = js(&["pread", &id]);
    assert_eq!(read["poll"]["status"], json!("ENDED"));
    assert_eq!(read["tally"], json!({ "counts": [1, 2, 1], "votes": 4, "voters": 4 }));
    assert_eq!(read["job"], Value::Null);
    let results = e.ops.sends();
    assert_eq!(results.len(), 1);
    let desc = results[0]["embeds"][0]["description"].as_str().unwrap_or("").to_string();
    assert!(desc.contains("🏆 **Green** won with **2** votes (50%)."), "{desc}");
    assert!(desc.contains("**2.** 🏆 Green"), "hidden results are shown once the poll ended: {desc}");
    e.done().await;
}

#[tokio::test]
async fn a_poll_run_by_rust_reads_and_ends_in_the_js_bot() {
    let Some(e) = Env::new("920000000000000024", 3).await else { return };
    let p = post(&e, &pending_for(&e, 1, Some(3_600_000)), &input(&["Yes", "No"], 1)).await;
    for (n, i) in [(1, 0), (2, 1), (3, 1)] {
        ps::apply_vote(&e.t, &p.id, &user(n), 1, ps::VoteInput::Toggle(i)).await.expect("vote");
    }
    let read = js(&["pread", &p.id]);
    let ours = ps::get_tally(e.t.db(), &p).await.expect("tally");
    assert_eq!(read["tally"], serde_json::to_value(&ours).unwrap_or_default());
    assert_eq!(read["poll"], serde_json::to_value(poll_row(&e, &p.id).await).unwrap_or_default(), "Prisma reads the Rust row");
    assert_eq!(read["job"]["type"], json!("poll.end"));
    assert_eq!(read["job"]["payload"], json!({ "pollId": p.id }));
    assert_eq!(read["votes"], json!([[user(1), 0], [user(2), 1], [user(3), 1]]));
    // A JS vote on a Rust poll, then the JS end.
    let r = js(&["pvote", &p.id, &user(1), "1", r#"{"idx":1}"#]);
    assert_eq!(r["result"], json!("switched"));
    let end = js(&["pend", e.g, &p.id]);
    assert_eq!(end["ok"], json!(true));
    assert_eq!(end["tally"]["counts"], json!([0, 3]));
    assert_eq!(poll_row(&e, &p.id).await.status, PollStatus::Ended);
    assert!(e.t.scheduler().get_job(&ps::end_job_key(&p.id)).await.is_none(), "the JS end cancelled the Rust-scheduled job");
    assert_eq!(ps::end_poll(&e.t, &p.id, ps::EndReason::Manual, Some(e.g)).await.expect("db"), Err(ps::EndPollError::AlreadyEnded));
    e.done().await;
}

// ── More of test/giveaways.test.js and test/polls.test.js ────────────────────

fn member_json(id: &str, roles: &[&str]) -> Value {
    json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": false },
        "roles": roles, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    })
}

async fn add_entries(e: &Env, giveaway_id: &str, users: &[String]) {
    for u in users {
        sqlx::query(r#"INSERT INTO "GiveawayEntry" ("id","giveawayId","userId") VALUES ($1,$2,$3)"#).bind(bot_db::new_cuid()).bind(giveaway_id).bind(u).execute(e.t.db()).await.expect("entry");
    }
}

async fn make_due(e: &Env, giveaway_id: &str) {
    let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).naive_utc();
    sqlx::query(r#"UPDATE "Giveaway" SET "endsAt" = $2 WHERE "id" = $1"#).bind(giveaway_id).bind(past).execute(e.t.db()).await.expect("due");
}

// makeResolver: required, denied and bonus roles, bots, members who left, deleted roles, failed lookups.
#[tokio::test]
async fn the_member_resolver_reads_the_cache_then_rest() {
    let Some(e) = Env::new("920000000000000031", 2).await else { return };
    let mut g = e.start("Resolver", 3_600_000).await;
    g.required_role_ids = vec![ROLE_A.into(), "830000000000000099".into()]; // the second role was deleted
    g.denied_role_ids = vec![ROLE_B.into()];
    g.role_weights = Some(json!([{ "roleId": ROLE_A, "weight": 4 }, { "roleId": "830000000000000098", "weight": 9 }]));
    {
        let mut rest = e.ops.rest.lock().unwrap_or_else(|x| x.into_inner());
        rest.insert("629000000000000001".into(), Ok(Some(member_json("629000000000000001", &[ROLE_A]))));
        rest.insert("629000000000000002".into(), Ok(None));
        rest.insert("629000000000000003".into(), Err(500));
        rest.insert("629000000000000004".into(), Ok(Some(member_json("629000000000000004", &[]))));
    }
    let mut resolve = gs::make_resolver(&e.t, e.g, &g);
    let ids: Vec<String> = [user(1), user(2), TEST_BOT_ID.to_string(), "629000000000000001".into(), "629000000000000002".into(), "629000000000000003".into(), "629000000000000004".into()].into();
    let out = resolve(ids).await;
    assert_eq!(out.get(&user(1)), Some(&Some(4)), "cached, holds the required role, bonus weight");
    assert_eq!(out.get(&user(2)), Some(&None), "holds the denied role (and misses the required one)");
    assert_eq!(out.get(TEST_BOT_ID), Some(&None), "bots never win");
    assert_eq!(out.get("629000000000000001"), Some(&Some(4)), "found over REST");
    assert_eq!(out.get("629000000000000002"), Some(&None), "left the server");
    assert_eq!(out.get("629000000000000003"), None, "a failed lookup is unknown");
    assert_eq!(out.get("629000000000000004"), Some(&None), "misses the required role");
    let exists = guild_view_role_exists(&e);
    assert_eq!(gs::max_weight_for(&g, &exists), 4, "a deleted bonus role is ignored");
    e.done().await;
}

fn guild_view_role_exists(e: &Env) -> impl Fn(&str) -> bool + use<> {
    feat_giveaways_polls::guild_view::role_exists_fn(&e.t, e.g)
}

// DB end: no entrants, a deleted channel, no access, unresolved entrants, an unavailable guild, across the attempts.
#[tokio::test]
async fn giveaway_end_follows_the_job_attempts() {
    let Some(e) = Env::new("920000000000000032", 3).await else { return };
    let bank = Arc::new(XpBank::default());
    e.t.rewards().register_provider(RewardKind::Xp, bank.clone(), Some("test")).expect("provider");
    e.t.settings().update_settings(e.g, SettingsPatch::new().set("levelingEnabled", true)).await.expect("levels on");

    // No entrants: ends with no winners and says so.
    let empty = e.start("Empty-1", 3_600_000).await;
    e.ops.clear();
    let r = gs::end_giveaway(&e.t, e.g, &empty.id, gs::EndSource::Command, 1).await.expect("db").expect("ended");
    assert!(r.winner_ids.is_empty());
    assert!(e.ops.sends()[0]["content"].as_str().unwrap_or("").starts_with("No valid entrants for **Empty-1**"));
    assert!(e.ops.calls("edit")[0].1["embeds"][0]["description"].as_str().unwrap_or("").contains("**Winners:** No valid entrants."));

    // A deleted channel drafts the giveaway.
    let lost = e.start("LostChannel-1", 3_600_000).await;
    sqlx::query(r#"UPDATE "Giveaway" SET "channelId" = '879999999999999999' WHERE "id" = $1"#).bind(&lost.id).execute(e.t.db()).await.expect("channel");
    assert_eq!(gs::end_giveaway(&e.t, e.g, &lost.id, gs::EndSource::Command, 1).await.expect("db"), Err(gs::GiveawayError::Drafted));

    // No access: a command gets NO_ACCESS; a job retries until its final attempt, then ends without posting but pays.
    let mut o = e.start_opts("Locked-1", 3_600_000);
    o.channel = e.channel_by(&e.locked());
    o.winner_count = 2;
    o.xp_prize = Some(11);
    let locked = gs::start_giveaway(&e.t, o).await.expect("started").giveaway;
    add_entries(&e, &locked.id, &[user(2), user(3)]).await;
    make_due(&e, &locked.id).await;
    match gs::end_giveaway(&e.t, e.g, &locked.id, gs::EndSource::Command, 1).await.expect("db") {
        Err(gs::GiveawayError::NoAccess { missing, .. }) => assert!(missing.contains(&"SendMessages".to_string()), "{missing:?}"),
        other => panic!("{other:?}"),
    }
    let err = gs::end_giveaway(&e.t, e.g, &locked.id, gs::EndSource::Job, 2).await.expect_err("retries");
    assert!(err.to_string().contains("channel unusable"), "{err}");
    assert_eq!(e.row(&locked.id).await.status, GiveawayStatus::Active, "a retryable failure changes nothing");
    e.ops.clear();
    let last = gs::end_giveaway(&e.t, e.g, &locked.id, gs::EndSource::Job, gs::END_MAX_ATTEMPTS).await.expect("db").expect("ended");
    assert!(!last.announced);
    assert!(e.ops.sends().is_empty() && e.ops.calls("edit").is_empty(), "nothing was posted");
    assert_eq!(bank.0.lock().unwrap_or_else(|x| x.into_inner()).iter().filter(|g| g.2 == "Giveaway prize: Locked-1").count(), 2, "prizes are still paid");

    // Unresolved entrants: a command refuses, a job retries, the final attempt ends without them.
    let mut o = e.start_opts("Flaky-1", 3_600_000);
    o.winner_count = 2;
    let flaky = gs::start_giveaway(&e.t, o).await.expect("started").giveaway;
    add_entries(&e, &flaky.id, &["629000000000000041".to_string(), "629000000000000042".to_string()]).await;
    make_due(&e, &flaky.id).await;
    *e.ops.member_error.lock().unwrap_or_else(|x| x.into_inner()) = Some(500);
    assert_eq!(gs::end_giveaway(&e.t, e.g, &flaky.id, gs::EndSource::Command, 1).await.expect("db"), Err(gs::GiveawayError::ResolveFailed));
    let err = gs::end_giveaway(&e.t, e.g, &flaky.id, gs::EndSource::Job, 1).await.expect_err("retries");
    assert!(err.to_string().contains("could not check every entrant"), "{err}");
    assert_eq!(e.row(&flaky.id).await.status, GiveawayStatus::Active);
    let fin = gs::end_giveaway(&e.t, e.g, &flaky.id, gs::EndSource::Job, gs::END_MAX_ATTEMPTS).await.expect("db").expect("ended");
    assert!(fin.winner_ids.is_empty());
    *e.ops.member_error.lock().unwrap_or_else(|x| x.into_inner()) = None;

    // An unavailable guild is never judged.
    let unavailable = e.start("Unavail-1", 3_600_000).await;
    make_due(&e, &unavailable.id).await;
    e.set_available(false);
    let err = gs::end_giveaway(&e.t, e.g, &unavailable.id, gs::EndSource::Job, 1).await.expect_err("retries");
    assert!(err.to_string().contains("unavailable"), "{err}");
    assert_eq!(gs::end_giveaway(&e.t, e.g, &unavailable.id, gs::EndSource::Command, 1).await.expect("db"), Err(gs::GiveawayError::ResolveFailed));
    e.set_available(true);
    assert_eq!(e.row(&unavailable.id).await.status, GiveawayStatus::Active);
    e.done().await;
}

// DB reroll: CONFLICT, RESOLVE_FAILED, a deleted message still announces.
#[tokio::test]
async fn giveaway_reroll_conflict_resolve_failed_and_a_deleted_message() {
    let Some(e) = Env::new("920000000000000033", 3).await else { return };
    let g = e.start("Reroll-1", 3_600_000).await;
    // Every entrant is uncached, so every draw waits on the REST member lookups.
    let entrants: Vec<String> = (51..55).map(|n| format!("6290000000000000{n}")).collect();
    add_entries(&e, &g.id, &entrants).await;
    for id in &entrants {
        e.ops.rest.lock().unwrap_or_else(|x| x.into_inner()).insert(id.clone(), Ok(Some(member_json(id, &[]))));
    }
    let first = gs::end_giveaway(&e.t, e.g, &g.id, gs::EndSource::Command, 1).await.expect("db").expect("ended").winner_ids;
    let others: Vec<String> = entrants.iter().filter(|id| !first.contains(id)).cloned().collect();
    assert_eq!(others.len(), 3);

    // The row changes while the draw waits on a member lookup: CONFLICT.
    e.ops.member_delay_ms.store(400, Ordering::SeqCst);
    let (pool, id) = (e.t.db().clone(), g.id.clone());
    let bump = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        sqlx::query(r#"UPDATE "Giveaway" SET "updatedAt" = "updatedAt" + interval '1 second' WHERE "id" = $1"#).bind(&id).execute(&pool).await.expect("bump");
    });
    let r = gs::reroll_giveaway(&e.t, e.g, &g.id, 5).await.expect("db");
    bump.await.expect("bump task");
    assert_eq!(r, Err(gs::GiveawayError::Conflict));
    e.ops.member_delay_ms.store(0, Ordering::SeqCst);

    // A failed lookup: RESOLVE_FAILED.
    e.ops.rest.lock().unwrap_or_else(|x| x.into_inner()).insert(others[0].clone(), Err(500));
    assert_eq!(gs::reroll_giveaway(&e.t, e.g, &g.id, 5).await.expect("db"), Err(gs::GiveawayError::ResolveFailed));
    e.ops.rest.lock().unwrap_or_else(|x| x.into_inner()).insert(others[0].clone(), Ok(None));

    // The message was deleted: the reroll still announces, and edits nothing.
    e.ops.delete_message(g.message_id.as_deref().unwrap_or(""));
    e.ops.clear();
    let r = gs::reroll_giveaway(&e.t, e.g, &g.id, 1).await.expect("db").expect("rerolled");
    assert!(r.announced);
    assert!(e.ops.calls("edit").is_empty());
    let sends = e.ops.sends();
    assert!(sends[0]["content"].as_str().unwrap_or("").starts_with("🎉 New winner: <@"), "{sends:?}");
    e.done().await;
}

// The Participate (N) count: one components-only edit per window.
#[tokio::test]
async fn the_participate_count_is_refreshed_once_per_window() {
    let Some(e) = Env::new("920000000000000034", 3).await else { return };
    let g = e.start("Count-1", 3_600_000).await;
    e.ops.clear();
    let click = |n: u32| FakeInteraction::button(&format!("giveaway:join:{}", g.id)).guild(e.g).user(&user(n)).build();
    bot_core::router::route_interaction(&e.t, click(3)).await;
    for _ in 0..50 {
        if !e.ops.calls("edit").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let edits = e.ops.calls("edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].1, json!({ "components": [to_json(&gs::build_join_row(&g.id, 1, false))] }), "only the button row");
    bot_core::router::route_interaction(&e.t, FakeInteraction::button(&format!("giveaway:join:{}", g.id)).guild(e.g).user(&user(2)).roles(&[ROLE_B]).build()).await;
    bot_core::router::route_interaction(&e.t, FakeInteraction::button(&format!("giveaway:join:{}", g.id)).guild(e.g).user(&user(1)).roles(&[ROLE_A]).build()).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(e.ops.calls("edit").len(), 1, "later clicks wait for the window");
    assert_eq!(e.t.ext::<gs::CountRefresh>().queued(), 1, "and collapse into one trailing edit");
    gs::reset_count_refresh(&e.t);
    e.done().await;
}

// DB: endPoll with a deleted message, a deleted channel, send failures and an unavailable guild.
#[tokio::test]
async fn poll_end_with_a_deleted_message_or_channel_a_send_failure_or_an_unavailable_guild() {
    let Some(e) = Env::new("920000000000000035", 1).await else { return };
    let a = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    e.ops.delete_message(a.message_id.as_deref().unwrap_or(""));
    e.ops.clear();
    let r = ps::end_poll(&e.t, &a.id, ps::EndReason::Manual, Some(e.g)).await.expect("db").expect("ended");
    assert!(r.message_gone && r.posted);
    let sends = e.ops.sends();
    assert!(sends[0].get("message_reference").is_none(), "no reply to a deleted message");
    assert!(!sends[0]["embeds"][0]["description"].as_str().unwrap_or("").contains("[Jump to the poll]"));

    let b = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    sqlx::query(r#"UPDATE "Poll" SET "channelId" = '879999999999999999' WHERE "id" = $1"#).bind(&b.id).execute(e.t.db()).await.expect("channel");
    e.ops.clear();
    let r = ps::end_poll(&e.t, &b.id, ps::EndReason::Manual, Some(e.g)).await.expect("db").expect("ended");
    assert!(r.message_gone && !r.posted);
    assert!(e.ops.sends().is_empty());

    let c = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    *e.ops.fail_send.lock().unwrap_or_else(|x| x.into_inner()) = Some(50013);
    let r = ps::end_poll(&e.t, &c.id, ps::EndReason::Manual, Some(e.g)).await.expect("db").expect("ended");
    assert!(!r.posted);
    *e.ops.fail_send.lock().unwrap_or_else(|x| x.into_inner()) = None;

    let d = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
    e.set_available(false);
    e.ops.clear();
    let r = ps::end_poll(&e.t, &d.id, ps::EndReason::Manual, Some(e.g)).await.expect("db").expect("ended");
    assert!(!r.posted && !r.message_gone);
    assert!(e.ops.log.lock().unwrap_or_else(|x| x.into_inner()).is_empty(), "no Discord call for an unavailable guild");
    e.set_available(true);
    assert_eq!(poll_row(&e, &d.id).await.status, PollStatus::Ended);

    // Prisma's `contains` does not escape wildcards; neither does the autocomplete query.
    for q in ["a%b", "axb", "a_b"] {
        let mut pend = pending_for(&e, 1, None);
        pend.channel_id = e.ch.clone();
        let p = ps::create_poll_row(&e.t, e.g, &pend, &ps::PollInput { question: q.into(), options: vec!["x".into(), "y".into()], max_choices: 1 }).await.expect("db").expect("row");
        assert_eq!(p.question, q);
    }
    let found = |q: &'static str| {
        let (db, g) = (e.t.db().clone(), e.g);
        async move {
            let mut v: Vec<String> = ps::find_polls_for_autocomplete(&db, g, "", "results", q).await.expect("db").into_iter().map(|r| r.question).filter(|q| q.starts_with('a')).collect();
            v.sort();
            v
        }
    };
    assert_eq!(found("%").await, vec!["a%b", "a_b", "axb"]);
    assert_eq!(found("A_B").await, vec!["a%b", "a_b", "axb"]);
    assert_eq!(found("x").await, vec!["axb"]);
    e.done().await;
}

// DB: a vote racing the end is either counted in the final results or refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vote_racing_the_end_is_counted_or_refused() {
    let Some(e) = Env::new("920000000000000036", 0).await else { return };
    for round in 0..5 {
        let p = post(&e, &pending_for(&e, 1, None), &input(&["A", "B"], 1)).await;
        let voter = user(100 + round);
        let (vote, end) = tokio::join!(
            ps::apply_vote(&e.t, &p.id, &voter, 1, ps::VoteInput::Toggle(0)),
            ps::end_poll(&e.t, &p.id, ps::EndReason::Manual, Some(e.g))
        );
        let ended = end.expect("db").expect("ended");
        let counted = ended.tally.as_ref().map(|t| t.counts[0]).unwrap_or(0);
        match vote.expect("db") {
            ps::VoteOutcome::Ended => assert_eq!(counted, 0, "refused votes are not counted"),
            ps::VoteOutcome::Applied { .. } => assert_eq!(counted, 1, "a vote that committed first is in the final tally"),
            other => panic!("{other:?}"),
        }
    }
    e.done().await;
}

// The /giveaway subcommands through the router.
#[tokio::test]
async fn giveaway_command_refusals_reroll_odds_cancel_and_restart() {
    let Some(e) = Env::new("920000000000000037", 3).await else { return };
    let route = |i| bot_core::router::route_interaction(&e.t, i);
    let title_of = |calls: &[bot_core::interaction::RecordedCall]| calls.last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default();
    let desc_of = |calls: &[bot_core::interaction::RecordedCall]| calls.last().and_then(|c| c.embed_description().map(str::to_string)).unwrap_or_default();
    let start = |ch: &str| moderator("giveaway", e.g, ch).subcommand("start").option("prize", option_type::STRING, json!("Prize")).option("duration", option_type::STRING, json!("1h"));

    let cases: Vec<(FakeInteraction, &str, String)> = vec![
        (start(&e.voice()), "❌  Unsupported Channel", "Giveaways can be posted in text or announcement channels only. Run this in one, or pick one with `channel`.".into()),
        (start(&e.locked()), "❌  No Access", format!("You can't send messages in <#{}>.", e.locked())),
        (start(&e.ch).option("required_role", option_type::ROLE, json!(ROLE_A)).option("denied_role", option_type::ROLE, json!(ROLE_A)), "❌  Invalid Role", "The same role cannot be both required and denied.".into()),
        (start(&e.ch).option("required_role", option_type::ROLE, json!(e.g)), "❌  Invalid Role", "Choose a specific role for `required_role`, not @everyone.".into()),
        (start(&e.ch).option("bonus_role", option_type::ROLE, json!(e.g)), "❌  Invalid Role", "Bonus odds for @everyone would change nothing. Choose a specific role.".into()),
        (start(&e.ch).option("ping_role", option_type::ROLE, json!(e.g)), "❌  Invalid Role", "I never ping @everyone. Choose a role for `ping_role`.".into()),
        (
            start(&e.ch).option("ping_role", option_type::ROLE, json!(ROLE_A)),
            "❌  Cannot Ping Role",
            format!("I can't ping <@&{ROLE_A}> in <#{}>. Make the role mentionable, or give me **Mention @everyone, @here and All Roles** there.", e.ch),
        ),
        (start(&e.ch).option("color", option_type::STRING, json!("not a colour")), "❌  Invalid Colour", "Use a hex colour like `#5865F2` or a name like `blurple`.".into()),
        (
            moderator("giveaway", e.g, &e.ch).subcommand("start").option("prize", option_type::STRING, json!("   ")).option("duration", option_type::STRING, json!("1h")),
            "❌  Invalid Prize",
            "The prize cannot be empty.".into(),
        ),
    ];
    for (i, title, desc) in cases {
        e.t.responses.clear();
        route(i.build()).await;
        let calls = e.t.responses.calls();
        assert_eq!((title_of(&calls), desc_of(&calls)), (title.to_string(), desc.clone()), "{calls:?}");
    }

    // Odds through the command.
    let g = e.start("Command-1", 3_600_000).await;
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).group("odds", "add").option("giveaway", option_type::STRING, json!(g.id)).option("role", option_type::ROLE, json!(ROLE_A)).option("weight", option_type::INTEGER, json!(4)).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(title_of(&calls), "✅  Odds Updated", "{calls:?}");
    assert_eq!(desc_of(&calls), format!("Members with <@&{ROLE_A}> now count as **4** entries in **Command-1**.\n\n**Bonus odds:** <@&{ROLE_A}> ×4"));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).group("odds", "remove").option("giveaway", option_type::STRING, json!(g.id)).option("role", option_type::ROLE, json!(ROLE_B)).build()).await;
    assert_eq!(desc_of(&e.t.responses.calls()), format!("<@&{ROLE_B}> has no bonus odds in this giveaway."));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).group("odds", "remove").option("giveaway", option_type::STRING, json!(g.id)).option("role", option_type::ROLE, json!(ROLE_A)).build()).await;
    assert_eq!(desc_of(&e.t.responses.calls()), format!("<@&{ROLE_A}> no longer has bonus odds in **Command-1**."));

    // Reroll before and after the end.
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("reroll").option("giveaway", option_type::STRING, json!(g.id)).build()).await;
    assert_eq!(title_of(&e.t.responses.calls()), "❌  Still Running");
    add_entries(&e, &g.id, &[user(1), user(2)]).await;
    gs::end_giveaway(&e.t, e.g, &g.id, gs::EndSource::Command, 1).await.expect("db").expect("ended");
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("reroll").option("giveaway", option_type::STRING, json!(g.id)).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(title_of(&calls), "✅  Giveaway Rerolled", "{calls:?}");
    assert!(desc_of(&calls).starts_with("New winner of **Command-1**: <@"));
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("reroll").option("giveaway", option_type::STRING, json!(g.id)).build()).await;
    assert_eq!(title_of(&e.t.responses.calls()), "⚠️  No One Left");

    // Cancel: the confirm prompt, then the click.
    let c = e.start("Cancel-2", 3_600_000).await;
    e.t.responses.clear();
    let app = (*e.t).clone();
    let i = moderator("giveaway", e.g, &e.ch).subcommand("cancel").option("giveaway", option_type::STRING, json!(c.id)).build();
    let run = tokio::spawn(async move { bot_core::router::route_interaction(&app, i).await });
    let mut yes = None;
    for _ in 0..200 {
        if !e.t.collectors().is_empty() {
            yes = e.t.responses.calls().iter().rev().find_map(|c| {
                c.body["components"][0]["components"][0]["custom_id"].as_str().filter(|id| id.starts_with("confirm:") && id.ends_with(":yes")).map(str::to_string)
            });
            if yes.is_some() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let yes = yes.expect("the confirm prompt was shown");
    let prompt = e.t.responses.calls();
    assert_eq!(title_of(&prompt), "⚠️  Cancel this giveaway?");
    bot_core::router::route_interaction(&e.t, FakeInteraction::button(&yes).guild(e.g).user(MODERATOR).roles(&[ROLE_MOD]).message_id("1000000000000000100").build()).await;
    run.await.expect("route");
    let calls = e.t.responses.calls();
    assert_eq!(title_of(&calls), "✅  Giveaway Cancelled", "{calls:?}");
    assert_eq!(desc_of(&calls), "**Cancel-2** was cancelled.");
    assert_eq!(e.row(&c.id).await.status, GiveawayStatus::Cancelled);

    // Restart a draft.
    let d = e.start("Restart-1", 3_600_000).await;
    sqlx::query(r#"UPDATE "Giveaway" SET "status" = 'DRAFT', "messageId" = NULL WHERE "id" = $1"#).bind(&d.id).execute(e.t.db()).await.expect("draft");
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("restart").option("giveaway", option_type::STRING, json!(d.id)).option("duration", option_type::STRING, json!("2h")).build()).await;
    let calls = e.t.responses.calls();
    assert_eq!(title_of(&calls), "✅  Giveaway Restarted", "{calls:?}");
    assert!(desc_of(&calls).contains("Existing entries were kept."));
    assert_eq!(e.row(&d.id).await.status, GiveawayStatus::Active);
    e.t.responses.clear();
    route(moderator("giveaway", e.g, &e.ch).subcommand("restart").option("giveaway", option_type::STRING, json!(d.id)).build()).await;
    assert_eq!(title_of(&e.t.responses.calls()), "❌  Not a Draft");
    e.done().await;
}

// ── makeResolver over the gateway ───────────────────────────────────────────────

fn chunk_of(guild: &str, nonce: &str, members: Vec<Value>) -> serenity::all::GuildMembersChunkEvent {
    serde_json::from_value(json!({ "guild_id": guild, "members": members, "chunk_index": 0, "chunk_count": 1, "nonce": nonce, "not_found": [] }))
        .expect("chunk")
}

/// giveawayService.js makeResolver: the uncached entrants of a batch are asked for in ONE gateway member request
/// (`guild.members.fetch({ user, time })`), not one REST call each. A member missing from the answer has left the
/// server (ineligible); a request that fails or times out leaves the whole batch unknown.
#[tokio::test]
async fn the_member_resolver_asks_the_gateway_once_per_batch() {
    use bot_core::guild_members::{MemberChunks, deliver_members_chunk, set_chunk_sender};
    const RG: &str = "900000000000000171";
    const RA: &str = "830000000000000171";
    const RB: &str = "830000000000000172";
    let guild = FakeGuild::new(RG, "600000000000000099")
        .role(RA, 1, P::empty())
        .role(RB, 2, P::empty())
        .member(TEST_BOT_ID, &[])
        .member("629000000000000170", &[RA])
        .channel("800000000000000171", 0, "general", &[]);
    let t = TestApp::new().with_guilds([RG]).with_fake_guilds(&[guild]);
    let mut gv = golden()["giveaway"]["embeds"][0]["g"].clone();
    gv["guildId"] = json!(RG);
    gv["requiredRoleIds"] = json!([RA]);
    gv["deniedRoleIds"] = json!([RB]);
    gv["roleWeights"] = json!([{ "roleId": RA, "weight": 3 }]);
    let g = giveaway_of(&gv);

    let requests = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let (app, seen) = (t.app.clone(), requests.clone());
    set_chunk_sender(
        &t,
        Some(Arc::new(move |_gid, ids: Vec<serenity::all::UserId>, nonce: String| {
            seen.lock().unwrap_or_else(|e| e.into_inner()).push(ids.iter().map(|u| u.to_string()).collect());
            // 171 holds the required role, 172 the denied one, 173 is not in the server any more.
            let members = vec![member_json("629000000000000171", &[RA]), member_json("629000000000000172", &[RA, RB])];
            assert!(deliver_members_chunk(&app.ext::<MemberChunks>(), &chunk_of(RG, &nonce, members)));
            Box::pin(async { true })
        })),
    );
    let mut resolve = gs::make_resolver(&t, RG, &g);
    let ids: Vec<String> = ["629000000000000170", "629000000000000171", "629000000000000172", "629000000000000173"].map(String::from).into();
    let out = resolve(ids).await;
    assert_eq!(out.get("629000000000000170"), Some(&Some(3)), "cached: no lookup");
    assert_eq!(out.get("629000000000000171"), Some(&Some(3)), "from the gateway answer");
    assert_eq!(out.get("629000000000000172"), Some(&None), "holds the denied role");
    assert_eq!(out.get("629000000000000173"), Some(&None), "missing from the answer: left the server");
    assert_eq!(
        *requests.lock().unwrap_or_else(|e| e.into_inner()),
        vec![vec!["629000000000000171".to_string(), "629000000000000172".into(), "629000000000000173".into()]],
        "one request for every uncached entrant of the batch"
    );

    // The request cannot reach the shard: every uncached id of the batch stays unknown (the draw is not complete).
    set_chunk_sender(&t, Some(Arc::new(|_, _, _| Box::pin(async { false }))));
    let mut resolve = gs::make_resolver(&t, RG, &g);
    let out = resolve(vec!["629000000000000170".into(), "629000000000000174".into(), "629000000000000175".into()]).await;
    assert_eq!(out.get("629000000000000170"), Some(&Some(3)));
    assert_eq!(out.get("629000000000000174"), None, "unknown");
    assert_eq!(out.get("629000000000000175"), None, "unknown");
}

/// commands/community/poll.js create: a picked channel the cache lacks (an archived thread, say) is not refused as
/// "Channel Not Found". discord.js builds it from the interaction's resolved data; here Discord is asked for it (with
/// its overwrites), within half the answer window, and the form opens for it.
#[tokio::test]
async fn poll_create_accepts_a_picked_channel_missing_from_the_cache() {
    use bot_core::testing::FakeRest;
    const TH: &str = "800000000000000119";
    let rest = FakeRest::new();
    let threads = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::SEND_MESSAGES_IN_THREADS | P::EMBED_LINKS | P::READ_MESSAGE_HISTORY;
    let guild = FakeGuild::new(OG, "600000000000000099")
        .role("820000000000000110", 1, threads)
        .member(TEST_BOT_ID, &["820000000000000110"])
        .role(MOD_ROLE, 2, P::MODERATE_MEMBERS | threads)
        .member(MOD, &[MOD_ROLE])
        .channel(OCH, 0, "general", &[]);
    let t = TestApp::new().with_guilds([OG]).with_fake_guilds_rest(&[guild], &rest);
    t.install(registry());
    rest.on(
        "GET",
        &format!("/channels/{TH}"),
        200,
        json!({ "id": TH, "type": 11, "name": "old-thread", "parent_id": OCH, "owner_id": MOD, "guild_id": OG,
            "message_count": 0, "member_count": 1, "rate_limit_per_user": 0, "nsfw": false, "permission_overwrites": [],
            "thread_metadata": { "archived": true, "auto_archive_duration": 1440, "archive_timestamp": "2025-01-01T00:00:00.000000+00:00", "locked": false } }),
    );
    bot_core::router::route_interaction(&t, mod_slash("poll", "create").option("channel", option_type::CHANNEL, json!(TH)).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].method, "showModal", "{calls:?}");
    let nonce = calls[0].body["custom_id"].as_str().unwrap_or("").trim_start_matches("poll-modal:").to_string();
    let pending = t.ext::<ps::PendingPolls>().0.take(&nonce, t.clock.now_ms()).expect("the form is stored");
    assert_eq!(pending.channel_id, TH);
    assert_eq!(rest.calls_to("GET", &format!("/channels/{TH}")).len(), 1);
}
