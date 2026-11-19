//! Port of `test/economy.test.js` (and the economy parts of `test/review-economy.test.js`), plus the golden, database
//! and cross-language checks every feature crate runs.
//!
//! Offline: the pure helpers against `rust/fixtures/golden/feat-economy/pure.json` (written by
//! `node rust/fixtures/gen/feat-economy-js.mjs pure --write`), the raw SQL text against Prisma's, the command payloads,
//! the plugin, the validation that runs before any database access and the private-error state machine.
//! With `TEST_DATABASE_URL`: every statement under EXPLAIN, the lazy global copy, escrow, atomic claims, boosts,
//! transfers, shop transactions, reset, the rewards provider, the commands through the router, and both directions
//! of the JS round trip.

use std::sync::Arc;

use bot_core::command::option_type;
use bot_core::interaction::{RecordingBackend, Responder};
use bot_core::prelude::*;
use bot_core::settings::MemorySettingsStore;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, load_golden, offline_discord_as, run_node};
use bot_db::{BoostPeriod, EconomyAccount, EconomyBoost, GuildSettings, SQL_NOW, ShopItemType};
use chrono::{DateTime, Utc};
use feat_economy::economy_service::{self as svc, EconomyCode, NewBoost, ResetWhat};
use serde_json::{Map, Value, json};
use serenity::all::Permissions;

const COIN_MAX: i64 = 1_000_000_000;

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_economy::register(&mut reg);
    reg
}

/// A settings row like the JS test double `S(over)`: economy on, daily 100, work 200, plus `over`.
fn s_with(over: Value) -> GuildSettings {
    let mut m: Map<String, Value> =
        json!({ "economyEnabled": true, "dailyMin": 100, "dailyMax": 100, "workMin": 200, "workMax": 200 }).as_object().cloned().unwrap_or_default();
    for (k, v) in over.as_object().into_iter().flatten() {
        m.insert(k.clone(), v.clone());
    }
    MemorySettingsStore::new_row("0", m).expect("settings row")
}

fn s() -> GuildSettings {
    s_with(json!({}))
}

fn settings_from(v: &Value) -> Option<GuildSettings> {
    v.as_object().map(|o| MemorySettingsStore::new_row("0", o.clone()).expect("settings row"))
}

fn date(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).expect("date").with_timezone(&Utc)
}

fn validation_json(r: &Result<String, String>) -> Value {
    match r {
        Ok(v) => json!({ "ok": true, "value": v }),
        Err(reason) => json!({ "ok": false, "reason": reason }),
    }
}

fn pure() -> Value {
    load_golden("feat-economy/pure.json")
}

// ═════════════════════════════════ pure helpers ═════════════════════════════════

#[test]
fn daily_amounts_follow_the_streak_table_and_round_half_up() {
    let got: Vec<i64> = (1..=8).map(|s| svc::daily_amount(100, s)).collect();
    assert_eq!(got, vec![100, 110, 120, 140, 180, 250, 400, 400]);
    assert_eq!(svc::daily_amount(55, 2), 61);
    assert_eq!(svc::daily_amount(50, 2), 55);
    assert_eq!(svc::daily_amount(51, 2), 56);
    for c in pure()["daily"].as_array().expect("daily") {
        let (base, streak) = (c["base"].as_i64().unwrap_or(0), c["streak"].as_i64().unwrap_or(0));
        assert_eq!(svc::daily_amount(base, streak), c["amount"].as_i64().unwrap_or(-1), "{c}");
        assert_eq!(svc::streak_multiplier_label(streak), c["label"].as_str().unwrap_or(""), "{c}");
    }
}

#[test]
fn streak_multiplier_labels_and_windows() {
    assert_eq!(svc::streak_multiplier_label(1), "×1");
    assert_eq!(svc::streak_multiplier_label(2), "×1.1");
    assert_eq!(svc::streak_multiplier_label(6), "×2.5");
    assert_eq!(svc::streak_multiplier_label(30), "×4");
    let now = date("2026-10-10T12:00:00Z");
    let ago = |h: f64| now - chrono::Duration::milliseconds((h * 3_600_000.0) as i64);
    assert_eq!(svc::next_daily_streak(None, 0, now), 1);
    assert_eq!(svc::next_daily_streak(Some(ago(30.0)), 3, now), 4);
    assert_eq!(svc::next_daily_streak(Some(ago(49.0)), 3, now), 1);
    assert_eq!(svc::active_streak(Some(ago(49.0)), 5, now), 0);
    assert_eq!(svc::active_streak(Some(ago(47.0)), 5, now), 5);
    assert_eq!(svc::active_streak(None, 5, now), 0);
    for c in pure()["streakWindows"].as_array().expect("windows") {
        let last = ago(c["hours"].as_f64().unwrap_or(0.0));
        assert_eq!(svc::next_daily_streak(Some(last), 3, now), c["next"].as_i64().unwrap_or(-1), "{c}");
        assert_eq!(svc::active_streak(Some(last), 5, now), c["active"].as_i64().unwrap_or(-1), "{c}");
    }
}

#[test]
fn economy_config_matches_the_js_defaults_and_clamps() {
    let golden = pure();
    for (name, settings) in golden["settings"].as_object().expect("settings") {
        let row = settings_from(settings);
        let got = serde_json::to_value(svc::economy_config(row.as_ref())).unwrap_or(Value::Null);
        assert_eq!(got, golden["economyConfig"][name], "economyConfig({name})");
    }
    let c = svc::economy_config(Some(&s_with(json!({ "economyMaxBalance": 2_000_000_000, "economyStartingBalance": 2_000_000_000, "dailyMin": 300 }))));
    assert_eq!((c.max_balance, c.starting_balance, c.daily_max), (COIN_MAX, COIN_MAX, 300));
}

#[test]
fn format_coins_and_format_net_match_the_js() {
    let golden = pure();
    for c in golden["formatCoins"].as_array().expect("cases") {
        let row = settings_from(&golden["settings"][c["settings"].as_str().unwrap_or("none")]);
        let n = c["n"].as_i64().unwrap_or(0);
        assert_eq!(svc::format_coins(n, row.as_ref(), true), c["bold"].as_str().unwrap_or(""), "{c}");
        assert_eq!(svc::format_coins(n, row.as_ref(), false), c["plain"].as_str().unwrap_or(""), "{c}");
        assert_eq!(svc::format_net(n, row.as_ref()), c["net"].as_str().unwrap_or(""), "{c}");
    }
    assert_eq!(svc::format_coins(1234, None, true), "🪙 **1,234** coins");
    assert!(svc::format_net(-100, None).starts_with('−'));
}

#[test]
fn currency_emoji_and_name_validation_match_the_js() {
    let golden = pure();
    for c in golden["emoji"].as_array().expect("emoji") {
        let got = match svc::validate_currency_emoji(c["input"].as_str().unwrap_or("")) {
            Ok((value, custom)) => json!({ "ok": true, "value": value, "customId": custom }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(got, c["result"], "emoji {:?}", c["input"]);
    }
    for c in golden["names"].as_array().expect("names") {
        let input = c["input"].as_str().unwrap_or("");
        assert_eq!(validation_json(&svc::validate_currency_name(input)), c["currency"], "currency {input:?}");
        assert_eq!(validation_json(&svc::validate_item_name(input)), c["item"], "item {input:?}");
        assert_eq!(validation_json(&svc::validate_boost_name(input)), c["boost"], "boost {input:?}");
    }
    assert_eq!(svc::validate_currency_name("  gold   coins "), Ok("gold coins".to_string()));
    assert_eq!(svc::validate_item_name("  VIP   Pass "), Ok("VIP Pass".to_string()));
}

#[test]
fn ownership_helpers_match_the_js() {
    for c in pure()["ownership"].as_array().expect("ownership") {
        let t = ShopItemType::parse(c["type"].as_str().unwrap_or("")).expect("type");
        let (use_on_buy, destroy) = (c["useOnBuy"].as_bool().unwrap_or(false), c["destroyOnUse"].as_bool().unwrap_or(false));
        assert_eq!(svc::max_owned_of(c["canBuyMultiple"].as_bool().unwrap_or(false), c["maxPerUser"].as_i64().unwrap_or(0)), c["max"].as_i64().unwrap_or(-1), "{c}");
        assert_eq!(svc::goes_to_inventory_of(use_on_buy, destroy, t), c["toInventory"].as_bool().unwrap_or(true), "{c}");
    }
}

#[test]
fn game_channel_check_allow_deny_threads_and_fail_open() {
    for c in pure()["gameChannels"].as_array().expect("cases") {
        let row = s_with(json!({ "economyGameChannelIds": c["ids"], "economyGameChannelMode": c["mode"] }));
        let exists = |id: &str| id == "1" || id == "2";
        let got = svc::game_channel_check(Some(&row), c["channel"].as_str().unwrap_or(""), c["parent"].as_str(), exists);
        assert_eq!(got.to_json(), c["result"], "{c}");
    }
}

#[tokio::test]
async fn casino_lock_is_per_member_and_idempotent() {
    let t = TestApp::new();
    assert!(svc::try_lock_casino(&t, "g", "u"));
    assert!(!svc::try_lock_casino(&t, "g", "u"));
    assert!(svc::is_casino_locked(&t, "g", "u"));
    assert!(svc::try_lock_casino(&t, "g", "other"));
    assert!(svc::try_lock_casino(&t, "g2", "u"));
    svc::unlock_casino(&t, "g", "u");
    svc::unlock_casino(&t, "g", "u");
    assert!(!svc::is_casino_locked(&t, "g", "u"));
    assert!(svc::try_lock_casino(&t, "g", "u"));
    assert!(!svc::is_casino_locked(&TestApp::new(), "g", "u"), "one lock set per app");
}

#[test]
fn item_variables_labels_and_the_default_messages() {
    let golden = pure();
    for c in golden["itemVars"].as_array().expect("cases") {
        let row = settings_from(&golden["settings"][c["settings"].as_str().unwrap_or("none")]);
        let vars = svc::item_vars(
            c["name"].as_str().unwrap_or(""),
            c["price"].as_i64().unwrap_or(0),
            c["quantity"].as_i64().unwrap_or(0),
            c["total"].as_i64().unwrap_or(0),
            row.as_ref(),
        );
        let obj: Map<String, Value> = vars.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        assert_eq!(Value::Object(obj), c["vars"], "{c}");
        let ctx = bot_util::placeholder_parser::RenderContext {
            vars: Some(vars.into_iter().map(|(k, v)| (k, bot_util::placeholder_parser::VarValue::Text(v))).collect()),
            ..Default::default()
        };
        assert_eq!(bot_util::placeholder_parser::render_placeholders(svc::DEFAULT_BUY_MESSAGE, &ctx, None), c["buy"].as_str().unwrap_or(""));
        assert_eq!(bot_util::placeholder_parser::render_placeholders(svc::DEFAULT_USE_MESSAGE, &ctx, None), c["use"].as_str().unwrap_or(""));
    }
    for c in golden["itemLabel"].as_array().expect("labels") {
        assert_eq!(svc::item_label(c["name"].as_str().unwrap_or("")), c["label"].as_str().unwrap_or(""), "{c}");
    }
}

#[test]
fn payout_clamp_boosted_amounts_and_period_keys() {
    let golden = pure();
    for c in golden["clampPayout"].as_array().expect("clamp") {
        let r = svc::clamp_payout(c["payout"].as_i64().unwrap_or(0), c["stake"].as_i64().unwrap_or(0), c["max"].as_i64().unwrap_or(0));
        assert_eq!(serde_json::to_value(r).unwrap_or(Value::Null), c["result"], "{c}");
    }
    for c in golden["boosted"].as_array().expect("boosted") {
        assert_eq!(svc::boosted_amount(c["base"].as_i64().unwrap_or(0), c["pct"].as_i64().unwrap_or(0)), c["amount"].as_i64().unwrap_or(-1));
    }
    let keys = golden["periodKeys"].as_array().expect("keys");
    assert!(keys.len() >= 200);
    for c in keys {
        let period = BoostPeriod::parse(c["period"].as_str().unwrap_or("")).expect("period");
        let got = svc::boost_period_key(period, c["tz"].as_str().unwrap_or(""), date(c["date"].as_str().unwrap_or("")));
        assert_eq!(got, c["key"].as_str().unwrap_or(""), "{c}");
    }
    let now = date("2026-06-30T23:30:00Z");
    assert_eq!(svc::boost_period_key(BoostPeriod::Daily, "Europe/London", now), "D:2026-07-01");
    assert_eq!(svc::boost_period_key(BoostPeriod::Weekly, "UTC", date("2021-01-03T12:00:00Z")), "W:2020-W53");
}

#[test]
fn order_boosts_role_filter_disabled_percent_then_age() {
    let golden = pure();
    let boosts: Vec<EconomyBoost> = golden["orderBoosts"]["boosts"]
        .as_array()
        .expect("boosts")
        .iter()
        .map(|b| {
            let at = date(b["createdAt"].as_str().unwrap_or(""));
            EconomyBoost {
                id: b["id"].as_str().unwrap_or("").into(),
                guild_id: "g".into(),
                name: b["id"].as_str().unwrap_or("").into(),
                role_id: b["roleId"].as_str().map(str::to_string),
                percent: b["percent"].as_i64().unwrap_or(0) as i32,
                period: BoostPeriod::Daily,
                enabled: b["enabled"].as_bool().unwrap_or(true),
                created_by: "a".into(),
                created_at: at,
                updated_at: at,
            }
        })
        .collect();
    for c in golden["orderBoosts"]["cases"].as_array().expect("cases") {
        let roles: Vec<String> = c["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
        let ids: Vec<String> = svc::order_boosts(&boosts, &roles).into_iter().map(|b| b.id).collect();
        assert_eq!(json!(ids), c["ids"], "{c}");
    }
    assert!(svc::order_boosts(&[], &[]).is_empty());
}

#[test]
fn constants_match_the_js() {
    let c = &pure()["constants"];
    assert_eq!(c["PITY_THRESHOLD"], json!(svc::PITY_THRESHOLD));
    assert_eq!(c["PITY_AMOUNT"], json!(svc::PITY_AMOUNT));
    assert_eq!(c["PITY_COOLDOWN_MS"], json!(svc::PITY_COOLDOWN_MS));
    assert_eq!(c["DAILY_COOLDOWN_MS"], json!(svc::DAILY_COOLDOWN_MS));
    assert_eq!(c["DAILY_STREAK_WINDOW_MS"], json!(svc::DAILY_STREAK_WINDOW_MS));
    assert_eq!(c["DAILY_MULTIPLIER_TENTHS"], json!(svc::DAILY_MULTIPLIER_TENTHS));
    assert_eq!(c["ITEM_NAME_MAX"], json!(svc::ITEM_NAME_MAX));
    assert_eq!(c["ITEM_DESCRIPTION_MAX"], json!(svc::ITEM_DESCRIPTION_MAX));
    assert_eq!(c["ITEM_MESSAGE_MAX"], json!(svc::ITEM_MESSAGE_MAX));
    assert_eq!(c["ITEM_IMAGE_MAX"], json!(svc::ITEM_IMAGE_MAX));
    assert_eq!(c["MAX_STOCK"], json!(svc::MAX_STOCK));
    assert_eq!(c["RANGE_MAX"], json!(svc::RANGE_MAX));
    assert_eq!(c["WORK_COOLDOWN_MAX_MINUTES"], json!(svc::WORK_COOLDOWN_MAX_MINUTES));
    assert_eq!(c["RICHEST_LIMIT"], json!(svc::RICHEST_LIMIT));
    assert_eq!(c["GAME_CHANNELS_MAX"], json!(svc::GAME_CHANNELS_MAX));
    assert_eq!(c["BOOST_NAME_MAX"], json!(svc::BOOST_NAME_MAX));
    assert_eq!(c["BOOST_PERCENT_MAX"], json!(svc::BOOST_PERCENT_MAX));
    assert_eq!(c["DEFAULT_BUY_MESSAGE"], json!(svc::DEFAULT_BUY_MESSAGE));
    assert_eq!(c["DEFAULT_USE_MESSAGE"], json!(svc::DEFAULT_USE_MESSAGE));
    assert_eq!(c["WORK_LINES"], json!(svc::WORK_LINES));
    for t in ShopItemType::ALL {
        assert_eq!(c["ITEM_TYPE_LABELS"][t.as_str()], json!(svc::item_type_label(*t)));
    }
    for p in BoostPeriod::ALL {
        assert_eq!(c["BOOST_PERIOD_LABELS"][p.as_str()], json!(svc::boost_period_label(*p)));
    }
}

/// Every raw statement is the JS statement: the same text once `$n` becomes Prisma's `?`, and the same parameters
/// (random ids aside).
#[test]
fn raw_sql_is_the_js_sql_statement_for_statement() {
    let golden = pure();
    let js = golden["rawSql"].as_array().expect("rawSql");
    let ours = svc::raw_sql();
    assert_eq!(js.len(), ours.len());
    let placeholder = regex::Regex::new(r"\$\d+").expect("regex");
    for ((name, stmt), j) in ours.iter().zip(js) {
        assert_eq!(*name, j["name"].as_str().unwrap_or(""));
        assert_eq!(placeholder.replace_all(&stmt.sql, "?"), j["sql"].as_str().unwrap_or(""), "{name}");
        let values: Vec<Value> = stmt
            .args
            .iter()
            .map(|a| match a {
                svc::Arg::Text(s) => json!(s),
                svc::Arg::Int(n) => json!(n),
                svc::Arg::Big(n) => json!(n),
                svc::Arg::Bool(b) => json!(b),
            })
            .collect();
        let jv = j["values"].as_array().expect("values");
        assert_eq!(values.len(), jv.len(), "{name}");
        for (a, b) in values.iter().zip(jv) {
            let random_id = b.as_str().is_some_and(|s| s.len() == 36 && s.matches('-').count() == 4);
            if !random_id {
                assert_eq!(a, b, "{name}");
            }
        }
    }
    for (name, stmt) in svc::raw_sql().into_iter().chain(svc::client_sql()) {
        let lower = stmt.sql.to_lowercase();
        let bare = ["now", "()"].concat();
        assert!(!lower.replace(&SQL_NOW.to_lowercase(), "").contains(&bare), "{name}: bare now");
    }
}

#[test]
fn failed_before_send_accepts_only_errors_that_prove_nothing_reached_the_database() {
    assert!(svc::failed_before_send(&anyhow::Error::new(sqlx::Error::PoolTimedOut)));
    assert!(svc::failed_before_send(&anyhow::Error::new(bot_db::DbError::Sqlx(sqlx::Error::PoolClosed))));
    assert!(!svc::failed_before_send(&anyhow::Error::new(sqlx::Error::RowNotFound)));
    assert!(!svc::failed_before_send(&anyhow::Error::new(sqlx::Error::Protocol("connection reset".into()))));
    assert!(!svc::failed_before_send(&anyhow::anyhow!("boom")));
    assert!(!svc::failed_before_send(&anyhow::Error::new(bot_db::DbError::Timeout("statement".into()))));
}

// ═════════════════════════════ validation before the database ═════════════════════════════

#[tokio::test]
async fn reserve_bet_settle_bet_transfer_credit_debit_validate_first() {
    let t = TestApp::new();
    for bet in [0, -5, COIN_MAX + 1] {
        assert_eq!(svc::reserve_bet(&t, "g", "u", bet, None).await.expect("no db needed"), svc::Reserved::Invalid, "{bet}");
    }
    let capped = s_with(json!({ "economyMaxBet": 500 }));
    assert_eq!(svc::reserve_bet(&t, "g", "u", 1000, Some(&capped)).await.expect("no db"), svc::Reserved::MaxBet { max_bet: 500 });
    assert_eq!(svc::Reserved::MaxBet { max_bet: 500 }.to_json(), json!({ "ok": false, "code": "MAX_BET", "maxBet": 500 }));
    let invalid = |r: anyhow::Result<()>| r.err().is_some_and(|e| svc::is_code(&e, EconomyCode::Invalid));
    assert!(invalid(svc::settle_bet(&t, "g", "u", 0, None, -1).await.map(|_| ())));
    assert!(invalid(svc::settle_bet(&t, "g", "u", -1, None, 100).await.map(|_| ())));
    assert!(invalid(svc::transfer(&t, "g", "a", "a", 5, None).await.map(|_| ())));
    assert!(invalid(svc::transfer(&t, "g", "a", "b", 0, None).await.map(|_| ())));
    assert!(invalid(svc::transfer(&t, "g", "a", "b", COIN_MAX + 1, None).await.map(|_| ())));
    assert!(invalid(svc::credit(&t, "g", "a", -1, None).await.map(|_| ())));
    assert!(invalid(svc::debit(&t, "g", "a", 0, None).await.map(|_| ())));
    let err = svc::set_balance(&t, "g", "a", 5, Some(&s_with(json!({ "economyMaxBalance": 4 })))).await.expect_err("refused");
    assert_eq!(svc::economy_error(&err).and_then(|e| e.data_i64("max")), Some(4));
    assert!(invalid(svc::purchase_item(&t, "g", "a", "i", 0, None).await.map(|_| ())));
    assert!(invalid(svc::give_item(&t, "g", "a", "i", 201).await.map(|_| ())));
    assert!(invalid(svc::take_item(&t, "g", "a", "i", 0).await.map(|_| ())));
    let pct = svc::create_boost(&t, "g", NewBoost { name: "x".into(), percent: 0, period: BoostPeriod::Daily, role_id: None, created_by: "a".into() }).await;
    assert_eq!(pct.err().as_ref().and_then(svc::economy_error).and_then(|e| e.reason().map(str::to_string)).as_deref(), Some("percent"));
}

// ═════════════════════════════ commands, plugin, payloads ═════════════════════════════

const COMMANDS: [&str; 10] = ["balance", "daily", "inventory", "pay", "pity", "richest", "shop", "use", "work", "economy-admin"];

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &COMMANDS);
    bot_core::testing::assert_plugin_matches_golden(&reg, "economy");
}

#[test]
fn command_modules_permission_levels_and_payload_budgets() {
    use feat_economy::commands::*;
    let budgets: [(CommandDef, usize); 10] = [
        (balance::command(), 110),
        (daily::command(), 90),
        (pity::command(), 90),
        (work::command(), 70),
        (pay::command(), 150),
        (richest::command(), 70),
        (shop::command(), 400),
        (inventory::command(), 110),
        (use_::command(), 120),
        (economy_admin::command(), 3800),
    ];
    for (def, budget) in budgets {
        let name = def.name().to_string();
        let payload = def.payload();
        let meta = def.metadata();
        assert_eq!(meta["module"], "economy", "{name}");
        assert_eq!(payload["contexts"], json!([0]), "{name}");
        let chars = SlashCommand::from_payload(&payload).expect("payload").char_count();
        assert!(chars <= budget, "{name}: {chars} > {budget}");
        if name == "economy-admin" {
            assert_eq!(meta["gated"], false);
            assert_eq!(meta["permissionLevel"], 2);
            assert_eq!(payload["default_member_permissions"], "32");
        } else {
            assert_eq!(meta["permissionLevel"], 0, "{name}");
            assert_eq!(meta["gated"], true, "{name}");
        }
        assert_eq!(payload.to_string().contains("\"autocomplete\":true"), meta["hasAutocomplete"] == true, "{name}");
    }
    let admin = economy_admin::command().payload();
    let groups: Vec<&str> = admin["options"].as_array().into_iter().flatten().filter(|o| o["type"] == 2).filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(groups, vec!["coins", "item", "config", "boost"]);
    let boost = admin["options"].as_array().into_iter().flatten().find(|o| o["name"] == "boost").cloned().unwrap_or(Value::Null);
    let names = |v: &Value| v["options"].as_array().into_iter().flatten().filter_map(|o| o["name"].as_str().map(str::to_string)).collect::<Vec<_>>();
    assert_eq!(names(&boost), vec!["add", "remove", "toggle", "list"]);
    assert_eq!(names(&boost["options"][0]), vec!["name", "percent", "period", "role"]);
    assert_eq!(economy_admin::HANDLER_KEYS.len(), 20);
}

#[test]
fn admin_text_helpers() {
    use feat_economy::commands::economy_admin::{channel_list_text, limits_summary, reset_prompt, streak_table};
    assert_eq!(streak_table(), "Day 1 ×1 · Day 2 ×1.1 · Day 3 ×1.2 · Day 4 ×1.4 · Day 5 ×1.8 · Day 6 ×2.5 · Day 7+ ×4");
    let none = |_: &str| false;
    assert_eq!(channel_list_text(None, none, 3500), "Casino games can be played in every channel.");
    let deny = s_with(json!({ "economyGameChannelIds": ["601", "602"], "economyGameChannelMode": "DENY" }));
    assert_eq!(
        channel_list_text(Some(&deny), |id| id == "601", 3500),
        "Casino games are blocked in: <#601>, ~~602~~ (deleted)"
    );
    assert!(channel_list_text(Some(&deny), none, 3500).ends_with("\nAll of these channels are gone, so games are currently allowed everywhere."));
    assert_eq!(
        limits_summary(None),
        "Starting balance: 🪙 **1,000** coins\nMaximum balance: 🪙 **1,000,000,000** coins\nMaximum bet: No limit\nMaximum payout: No limit"
    );
    assert!(reset_prompt(ResetWhat::Coins, None, None).contains("Members who never used the economy here will start at 🪙 **1,000** coins"));
    assert_eq!(reset_prompt(ResetWhat::Items, None, None), "Every item in every inventory is removed. Shop items stay.");
}

// Offline command flows (no database: the checks that run before the deferral).

const OG: &str = "955026100000000001";
const ME: &str = "955026900000000001";

async fn offline_app() -> TestApp {
    let t = TestApp::new().with_guilds([OG]);
    t.install(registry());
    t.settings().update_settings(OG, SettingsPatch::new().set("economyEnabled", true)).await.expect("memory store");
    t
}

/// Adds a resolved user (and member) for a user option, as Discord sends them.
fn resolved(mut fi: FakeInteraction, id: &str, bot: bool, member: bool) -> FakeInteraction {
    let j = fi.json_mut();
    j["data"]["resolved"]["users"][id] =
        json!({ "id": id, "username": format!("user{}", &id[id.len() - 2..]), "discriminator": "0", "avatar": null, "bot": bot, "global_name": null });
    if member {
        j["data"]["resolved"]["members"][id] =
            json!({ "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "permissions": "0" });
    }
    fi
}

#[tokio::test]
async fn pity_is_refused_privately_while_a_casino_game_runs_and_leaves_that_lock_alone() {
    let t = offline_app().await;
    assert!(svc::try_lock_casino(&t, OG, ME));
    bot_core::router::route_interaction(&t, FakeInteraction::slash("pity").guild(OG).user(ME).build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    let call = &t.responses.calls()[0];
    assert!(call.ephemeral());
    assert!(call.embed_title().unwrap_or("").contains("Game In Progress"));
    assert!(svc::is_casino_locked(&t, OG, ME), "the game's lock is untouched");
}

#[tokio::test]
async fn pity_releases_the_casino_lock_when_it_ends_even_on_a_failure() {
    let t = offline_app().await;
    t.responses.set_failing(true);
    bot_core::router::route_interaction(&t, FakeInteraction::slash("pity").guild(OG).user(ME).build()).await;
    assert!(!svc::is_casino_locked(&t, OG, ME));
    t.responses.set_failing(false);
    // Offline the database is unreachable: the command fails after the deferral and still releases the lock.
    bot_core::router::route_interaction(&t, FakeInteraction::slash("pity").guild(OG).user(ME).build()).await;
    assert_eq!(t.responses.names().first(), Some(&"deferReply"));
    assert!(!svc::is_casino_locked(&t, OG, ME));
}

#[tokio::test]
async fn pay_refuses_yourself_bots_and_non_members_privately_before_deferring() {
    let t = offline_app().await;
    let cases: [(&str, bool, bool, &str); 3] = [
        (ME, false, true, "You can't pay yourself."),
        ("955026900000000002", true, true, "Bots can't hold coins or items."),
        ("955026900000000003", false, false, "That user is not a member of this server."),
    ];
    for (target, bot, member, text) in cases {
        t.responses.clear();
        let fi = FakeInteraction::slash("pay")
            .guild(OG)
            .user(ME)
            .option("member", option_type::USER, json!(target))
            .option("amount", option_type::INTEGER, json!(5));
        bot_core::router::route_interaction(&t, resolved(fi, target, bot, member).build()).await;
        assert_eq!(t.responses.names(), vec!["reply"], "{text}");
        let call = &t.responses.calls()[0];
        assert!(call.ephemeral());
        assert_eq!(call.embed_description(), Some(text));
    }
}

#[tokio::test]
async fn balance_and_inventory_refuse_bot_targets_privately() {
    let t = offline_app().await;
    for name in ["balance", "inventory"] {
        t.responses.clear();
        let bot = "955026900000000002";
        let fi = FakeInteraction::slash(name).guild(OG).user(ME).option("user", option_type::USER, json!(bot));
        bot_core::router::route_interaction(&t, resolved(fi, bot, true, true).build()).await;
        assert_eq!(t.responses.names(), vec!["reply"], "{name}");
        assert_eq!(t.responses.calls()[0].embed_description(), Some("Bots can't hold coins or items."));
        assert!(t.responses.calls()[0].ephemeral());
    }
}

#[tokio::test]
async fn economy_admin_refuses_bot_members_before_deferring() {
    let t = offline_app().await;
    let bot = "955026900000000002";
    let fi = FakeInteraction::slash("economy-admin")
        .guild(OG)
        .user(ME)
        .permissions(Permissions::MANAGE_GUILD)
        .group("coins", "give")
        .option("member", option_type::USER, json!(bot))
        .option("amount", option_type::INTEGER, json!(5));
    bot_core::router::route_interaction(&t, resolved(fi, bot, true, true).build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(t.responses.calls()[0].embed_description(), Some("Bots can't hold coins or items."));
}

#[tokio::test]
async fn reply_fail_shows_the_error_privately_whatever_the_interaction_state() {
    let id = serenity::all::InteractionId::new(1);
    let fresh = Arc::new(RecordingBackend::new());
    let r = Responder::new(fresh.clone(), id, "t");
    svc::reply_fail(&r, "Oops", "Nope").await;
    assert_eq!(fresh.names(), vec!["reply"]);
    assert!(fresh.calls()[0].ephemeral());

    let public = Arc::new(RecordingBackend::new());
    let r = Responder::new(public.clone(), id, "t");
    r.defer_reply(false).await.expect("defer");
    svc::reply_fail(&r, "Oops", "Nope").await;
    assert_eq!(public.names(), vec!["deferReply", "deleteReply", "followUp"]);
    assert!(public.calls()[2].ephemeral());

    let private = Arc::new(RecordingBackend::new());
    let r = Responder::new(private.clone(), id, "t");
    r.defer_reply(true).await.expect("defer");
    svc::reply_fail(&r, "Oops", "Nope").await;
    assert_eq!(private.names(), vec!["deferReply", "editReply"]);

    let update = Arc::new(RecordingBackend::new());
    let r = Responder::new(update.clone(), id, "t");
    r.defer_update().await.expect("defer update");
    svc::reply_fail_embed(&r, warn_embed("custom", None, None)).await;
    assert_eq!(update.names(), vec!["deferUpdate", "followUp"], "the clicked panel is never overwritten");
    assert!(update.calls()[1].embed_title().unwrap_or("").contains("custom"));

    let gone = Arc::new(RecordingBackend::new());
    gone.set_failing(true);
    svc::reply_fail(&Responder::new(gone, id, "t"), "x", "y").await; // never fails
}

// ═══════════════════════════════════ DB cases ═══════════════════════════════════

fn gid(n: u32) -> String {
    format!("95502600000000{n:05}")
}

fn uid(n: u32) -> String {
    format!("95502690000000{n:05}")
}

struct Db {
    t: DbTestApp,
    g: String,
    users: Vec<String>,
}

impl Db {
    async fn new(g: u32, users: &[u32]) -> Option<Self> {
        let url = database_url()?;
        let g = gid(g);
        let t = DbTestApp::new(&url).await.with_guilds([g.clone()]);
        let db = Self { t, g, users: users.iter().map(|u| uid(*u)).collect() };
        db.clean().await;
        Some(db)
    }

    async fn clean(&self) {
        self.t.reset_guilds(&[self.g.as_str()]).await;
        sqlx::query(r#"DELETE FROM "UserEconomy" WHERE "userId" = ANY($1)"#).bind(&self.users).execute(&self.t.pool).await.expect("cleanup");
    }

    async fn done(self) {
        self.clean().await;
    }

    async fn account(&self, u: &str) -> Option<EconomyAccount> {
        sqlx::query_as(r#"SELECT * FROM "EconomyAccount" WHERE "guildId" = $1 AND "userId" = $2"#).bind(&self.g).bind(u).fetch_optional(&self.t.pool).await.expect("read")
    }

    async fn balance(&self, u: &str) -> i64 {
        self.account(u).await.map(|a| i64::from(a.balance)).unwrap_or(-1)
    }

    /// `seedAccount(guildId, userId, { balance })`: a fresh account with no streak or cooldowns.
    async fn seed(&self, u: &str, balance: i64) {
        self.t.settings().ensure_guild_settings(&self.g).await.expect("settings row");
        let q = format!(
            r#"INSERT INTO "EconomyAccount" ("id","guildId","userId","balance","dailyStreak","importedGlobal","createdAt","updatedAt")
               VALUES ($1, $2, $3, $4, 0, false, {SQL_NOW}, {SQL_NOW})
               ON CONFLICT ("guildId","userId") DO UPDATE SET "balance" = $4, "dailyStreak" = 0, "lastDaily" = NULL, "lastWork" = NULL, "lastPity" = NULL"#
        );
        sqlx::query(sqlx::AssertSqlSafe(q)).bind(bot_db::new_uuid()).bind(&self.g).bind(u).bind(balance as i32).execute(&self.t.pool).await.expect("seed");
    }

    async fn hours_ago(&self, u: &str, col: &str, hours: f64) {
        let q = format!(r#"UPDATE "EconomyAccount" SET "{col}" = {SQL_NOW} - make_interval(secs => $1) WHERE "guildId" = $2 AND "userId" = $3"#);
        sqlx::query(sqlx::AssertSqlSafe(q)).bind(hours * 3600.0).bind(&self.g).bind(u).execute(&self.t.pool).await.expect("update");
    }

    async fn set_balance_raw(&self, u: &str, balance: i64) {
        sqlx::query(r#"UPDATE "EconomyAccount" SET "balance" = $1 WHERE "guildId" = $2 AND "userId" = $3"#)
            .bind(balance as i32)
            .bind(&self.g)
            .bind(u)
            .execute(&self.t.pool)
            .await
            .expect("update");
    }

    async fn legacy(&self, u: &str, balance: i64) {
        let q = format!(
            r#"INSERT INTO "UserEconomy" ("id","userId","balance","updatedAt") VALUES ($1, $2, $3, {SQL_NOW}) ON CONFLICT ("userId") DO UPDATE SET "balance" = $3"#
        );
        sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(bot_db::new_cuid())
        .bind(u)
        .bind(balance as i32)
        .execute(&self.t.pool)
        .await
        .expect("legacy row");
    }

    async fn item(&self, data: Value) -> bot_db::ShopItem {
        svc::create_item(&self.t, &self.g, data.as_object().cloned().unwrap_or_default()).await.expect("item")
    }

    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string())).bind(&self.g).fetch_one(&self.t.pool).await.expect("count")
    }
}

fn code_of<T>(r: anyhow::Result<T>) -> Option<EconomyCode> {
    r.err().as_ref().and_then(svc::economy_error).map(|e| e.code)
}

#[tokio::test]
async fn every_statement_is_valid_sql_explain() {
    let Some(db) = Db::new(1, &[]).await else { return };
    db.t.settings().ensure_guild_settings(&db.g).await.expect("row");
    let mut conn = db.t.pool.begin().await.expect("tx");
    for (name, stmt) in svc::raw_sql().into_iter().chain(svc::client_sql()) {
        let explain = svc::Stmt { sql: format!("EXPLAIN {}", stmt.sql), args: stmt.args.clone() };
        explain.query().fetch_all(&mut *conn).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    conn.rollback().await.expect("rollback");
    db.done().await;
}

#[tokio::test]
async fn lazy_global_copy_legacy_balance_starting_balance_clamps_and_the_import_switch() {
    let Some(db) = Db::new(2, &[1, 2, 3, 4]).await else { return };
    let (u1, u2, u3, u4) = (uid(1), uid(2), uid(3), uid(4));
    db.legacy(&u1, 777).await;
    db.legacy(&u2, -50).await;
    db.legacy(&u3, 5000).await;
    let a = svc::get_account(&db.t, &db.g, &u1, Some(&s())).await.expect("account");
    assert_eq!((a.balance, a.imported_global), (777, true));
    assert_eq!(svc::get_account(&db.t, &db.g, &u1, Some(&s())).await.expect("again").id, a.id, "idempotent");
    assert_eq!(svc::get_account(&db.t, &db.g, &u2, Some(&s())).await.expect("u2").balance, 0, "a negative legacy balance is clamped");
    assert_eq!(svc::get_account(&db.t, &db.g, &u3, Some(&s_with(json!({ "economyMaxBalance": 1000 })))).await.expect("u3").balance, 1000);
    let fresh = svc::get_account(&db.t, &db.g, &u4, Some(&s_with(json!({ "economyStartingBalance": 321 })))).await.expect("u4");
    assert_eq!((fresh.balance, fresh.imported_global), (321, false));
    assert!(fresh.id.len() == 36, "the JS randomUUID() id shape");
    // The import switch: no legacy read, the starting balance applies (a second account of u1 would copy 777 again).
    let other = Db::new(3, &[]).await.expect("same database");
    assert_eq!(svc::get_account(&other.t, &other.g, &u1, Some(&s())).await.expect("g3").balance, 777);
    let off = svc::get_account(&other.t, &other.g, &u2, Some(&s_with(json!({ "economyImportGlobal": false, "economyStartingBalance": 55 })))).await.expect("off");
    assert_eq!((off.balance, off.imported_global), (55, false));
    other.done().await;
    // UserEconomy is never written.
    let legacy: i32 = sqlx::query_scalar(r#"SELECT "balance" FROM "UserEconomy" WHERE "userId" = $1"#).bind(&u1).fetch_one(&db.t.pool).await.expect("legacy");
    assert_eq!(legacy, 777);
    let none: Option<i32> = sqlx::query_scalar(r#"SELECT "balance" FROM "UserEconomy" WHERE "userId" = $1"#).bind(&u4).fetch_optional(&db.t.pool).await.expect("none");
    assert!(none.is_none());
    db.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lazy_global_copy_is_race_safe_and_peek_never_creates_rows() {
    let Some(db) = Db::new(5, &[5]).await else { return };
    let u = uid(5);
    db.legacy(&u, 400).await;
    db.t.settings().ensure_guild_settings(&db.g).await.expect("row");
    let peek = svc::peek_account(&db.t, &db.g, &u, Some(&s())).await.expect("peek");
    assert_eq!((peek.exists, peek.balance, peek.imported_global, peek.daily_streak), (false, 400, true, 0));
    assert!(db.account(&u).await.is_none());
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (app, g, u) = (db.t.app.clone(), db.g.clone(), u.clone());
        tasks.push(tokio::spawn(async move { svc::get_account(&app, &g, &u, Some(&s())).await.map(|a| a.id) }));
    }
    let mut ids = std::collections::HashSet::new();
    for task in tasks {
        ids.insert(task.await.expect("join").expect("account"));
    }
    assert_eq!(ids.len(), 1);
    assert!(svc::peek_account(&db.t, &db.g, &u, Some(&s())).await.expect("peek").exists);
    db.done().await;
}

#[tokio::test]
async fn credit_clamps_to_the_server_maximum_and_debit_never_goes_below_zero() {
    let Some(db) = Db::new(6, &[6, 7, 8]).await else { return };
    let (u6, u7, u8) = (uid(6), uid(7), uid(8));
    db.seed(&u6, 500).await;
    let c = |b, capped| svc::Credited { balance: b, capped };
    assert_eq!(svc::credit(&db.t, &db.g, &u6, 100, Some(&s())).await.expect("credit"), c(600, false));
    assert_eq!(svc::credit(&db.t, &db.g, &u6, 36_000_000_000, Some(&s())).await.expect("credit"), c(COIN_MAX, true), "far past int4");
    assert_eq!(svc::credit(&db.t, &db.g, &u6, 5, Some(&s_with(json!({ "economyMaxBalance": 2000 })))).await.expect("credit"), c(COIN_MAX, true));
    db.seed(&u6, 250).await;
    assert_eq!(svc::debit(&db.t, &db.g, &u6, 300, Some(&s())).await.expect("debit"), svc::Debited { ok: false, balance: 250 });
    assert_eq!(svc::debit(&db.t, &db.g, &u6, 250, Some(&s())).await.expect("debit"), svc::Debited { ok: true, balance: 0 });
    assert_eq!(svc::credit(&db.t, &db.g, &u6, 0, Some(&s())).await.expect("credit").balance, 0);
    let start90 = s_with(json!({ "economyStartingBalance": 90 }));
    assert_eq!(svc::credit(&db.t, &db.g, &u7, 10, Some(&start90)).await.expect("credit").balance, 100);
    assert_eq!(svc::debit(&db.t, &db.g, &u8, 5000, Some(&start90)).await.expect("debit"), svc::Debited { ok: false, balance: 90 });
    db.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_debits_never_overspend_and_two_games_cannot_spend_the_same_coins() {
    let Some(db) = Db::new(7, &[9, 11]).await else { return };
    let (u9, u11) = (uid(9), uid(11));
    db.seed(&u9, 250).await;
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (app, g, u) = (db.t.app.clone(), db.g.clone(), u9.clone());
        tasks.push(tokio::spawn(async move { svc::debit(&app, &g, &u, 100, Some(&s())).await.map(|r| r.ok) }));
    }
    let mut ok = 0;
    for task in tasks {
        if task.await.expect("join").expect("debit") {
            ok += 1;
        }
    }
    assert_eq!(ok, 2);
    assert_eq!(db.balance(&u9).await, 50);

    db.seed(&u11, 1000).await;
    let st = s();
    let (a, b) = tokio::join!(svc::reserve_bet(&db.t, &db.g, &u11, 1000, Some(&st)), svc::reserve_bet(&db.t, &db.g, &u11, 1000, Some(&st)));
    let oks = [a.expect("a"), b.expect("b")].iter().filter(|r| matches!(r, svc::Reserved::Ok { .. })).count();
    assert_eq!(oks, 1);
    assert_eq!(db.balance(&u11).await, 0);
    db.done().await;
}

#[tokio::test]
async fn reserve_bet_escrows_before_play_and_settle_bet_pays_the_gross_payout() {
    let Some(db) = Db::new(8, &[10]).await else { return };
    let u = uid(10);
    db.seed(&u, 1000).await;
    assert_eq!(svc::reserve_bet(&db.t, &db.g, &u, 400, Some(&s())).await.expect("bet"), svc::Reserved::Ok { balance: 600, before: 1000 });
    assert_eq!(svc::reserve_bet(&db.t, &db.g, &u, 700, Some(&s())).await.expect("bet"), svc::Reserved::Insufficient { balance: 600 });
    assert_eq!(svc::reserve_bet(&db.t, &db.g, &u, 300, Some(&s_with(json!({ "economyMaxBet": 200 })))).await.expect("bet"), svc::Reserved::MaxBet { max_bet: 200 });
    let settled = |b, c, p| svc::Settled { balance: b, capped: c, payout_capped: p };
    assert_eq!(svc::settle_bet(&db.t, &db.g, &u, 800, Some(&s()), 400).await.expect("settle"), settled(1400, false, false));
    assert_eq!(svc::settle_bet(&db.t, &db.g, &u, 0, Some(&s()), 400).await.expect("loss"), settled(1400, false, false));
    let capped = s_with(json!({ "economyMaxPayout": 500 }));
    db.seed(&u, 0).await;
    assert_eq!(svc::settle_bet(&db.t, &db.g, &u, 1000, Some(&capped), 1000).await.expect("push").balance, 1000);
    assert_eq!(svc::settle_bet(&db.t, &db.g, &u, 2000, Some(&capped), 1000).await.expect("win"), settled(2500, false, true));
    assert_eq!(svc::settle_bet(&db.t, &db.g, &u, 36_000_000_000, Some(&s()), 1_000_000_000).await.expect("max"), settled(COIN_MAX, true, false));
    db.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_success_insufficient_recipient_cap_and_no_deadlock() {
    let Some(db) = Db::new(10, &[12, 13, 14]).await else { return };
    let (a, b, c) = (uid(12), uid(13), uid(14));
    db.seed(&a, 500).await;
    db.seed(&b, 100).await;
    let t = |from: &str, to: &str, n, st: GuildSettings| {
        let (app, g, from, to) = (db.t.app.clone(), db.g.clone(), from.to_string(), to.to_string());
        async move { svc::transfer(&app, &g, &from, &to, n, Some(&st)).await.expect("transfer") }
    };
    assert_eq!(t(&a, &b, 200, s()).await, svc::Transfer::Done { from_balance: 300, to_balance: 300 });
    assert_eq!(t(&a, &b, 301, s()).await, svc::Transfer::Refused { code: EconomyCode::Insufficient, balance: 300 });
    let capped = t(&a, &b, 250, s_with(json!({ "economyMaxBalance": 500 }))).await;
    assert_eq!(capped.to_json(), json!({ "ok": false, "code": "RECIPIENT_CAP", "balance": 300 }));
    assert_eq!((db.balance(&a).await, db.balance(&b).await), (300, 300));
    let to_new = t(&a, &c, 10, s_with(json!({ "economyStartingBalance": 0 }))).await;
    assert_eq!(to_new, svc::Transfer::Done { from_balance: 290, to_balance: 10 });
    let (x, y) = tokio::join!(tokio::spawn(t(&a, &b, 5, s())), tokio::spawn(t(&b, &a, 7, s())));
    assert!(matches!(x.expect("join"), svc::Transfer::Done { .. }) && matches!(y.expect("join"), svc::Transfer::Done { .. }));
    assert_eq!(db.balance(&a).await + db.balance(&b).await + db.balance(&c).await, 600);
    db.done().await;
}

#[tokio::test]
async fn set_balance_remove_coins_clamp_top_accounts_and_rank() {
    let Some(db) = Db::new(11, &[15, 16, 17, 18]).await else { return };
    let (u15, u16, u17, u18) = (uid(15), uid(16), uid(17), uid(18));
    db.seed(&u15, 100).await;
    db.seed(&u16, 900).await;
    db.seed(&u17, 900).await;
    db.seed(&u18, 0).await;
    assert_eq!(svc::remove_coins(&db.t, &db.g, &u15, 40, Some(&s())).await.expect("rm"), svc::Removed { balance: 60, removed: 40 });
    assert_eq!(svc::remove_coins(&db.t, &db.g, &u15, 1_000_000, Some(&s())).await.expect("rm"), svc::Removed { balance: 0, removed: 60 });
    assert_eq!(svc::set_balance(&db.t, &db.g, &u15, 250, Some(&s())).await.expect("set"), 250);
    assert_eq!(code_of(svc::set_balance(&db.t, &db.g, &u15, 5000, Some(&s_with(json!({ "economyMaxBalance": 1000 })))).await), Some(EconomyCode::Invalid));
    let top: Vec<String> = svc::top_accounts(&db.t, &db.g, 100).await.expect("top").into_iter().map(|r| r.user_id).collect();
    assert_eq!(top, vec![u16.clone(), u17.clone(), u15.clone()]);
    assert_eq!(svc::get_rank(&db.t, &db.g, 900).await.expect("rank"), 1);
    assert_eq!(svc::get_rank(&db.t, &db.g, 250).await.expect("rank"), 3);
    assert_eq!(svc::clamp_balances_to_cap(&db.t, &db.g, 500).await.expect("clamp"), 2);
    assert_eq!(db.balance(&u16).await, 500);
    db.done().await;
}

#[tokio::test]
async fn daily_streak_multipliers_the_48h_window_the_cap_and_no_double_claims() {
    let Some(db) = Db::new(12, &[19]).await else { return };
    let u = uid(19);
    db.seed(&u, 0).await;
    let svc::DailyClaim::Claimed { amount, base, streak, balance, next_at, boost, .. } =
        svc::claim_daily(&db.t, &db.g, &u, Some(&s()), &[]).await.expect("day 1")
    else {
        panic!("day 1 is claimed")
    };
    assert_eq!((amount, base, streak, balance, boost), (100, 100, 1, 100, None));
    assert!((next_at - (Utc::now() + chrono::Duration::hours(24))).num_milliseconds().abs() < 5000);
    let again = svc::claim_daily(&db.t, &db.g, &u, Some(&s()), &[]).await.expect("again");
    assert!(matches!(again, svc::DailyClaim::NotYet { streak: 1, .. }), "{again:?}");
    assert_eq!(db.balance(&u).await, 100);

    let mut total = 100;
    for (want_streak, want_amount) in [(2, 110), (3, 120), (4, 140), (5, 180), (6, 250), (7, 400), (8, 400)] {
        db.hours_ago(&u, "lastDaily", 25.0).await;
        let r = svc::claim_daily(&db.t, &db.g, &u, Some(&s()), &[]).await.expect("claim");
        total += want_amount;
        let svc::DailyClaim::Claimed { streak, amount, balance, .. } = r else { panic!("day {want_streak} is claimed") };
        assert_eq!((streak, amount, balance), (want_streak, want_amount, total), "day {want_streak}");
    }
    db.hours_ago(&u, "lastDaily", 49.0).await;
    let r = svc::claim_daily(&db.t, &db.g, &u, Some(&s()), &[]).await.expect("reset");
    assert!(matches!(r, svc::DailyClaim::Claimed { streak: 1, amount: 100, .. }), "{r:?}");
    db.hours_ago(&u, "lastDaily", 25.0).await;
    db.set_balance_raw(&u, 950).await;
    let r = svc::claim_daily(&db.t, &db.g, &u, Some(&s_with(json!({ "economyMaxBalance": 1000 }))), &[]).await.expect("cap");
    assert!(matches!(r, svc::DailyClaim::Claimed { balance: 1000, capped: true, .. }), "{r:?}");
    db.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_daily_work_and_pity_claims_pay_once() {
    let Some(db) = Db::new(13, &[20, 22, 24]).await else { return };
    let (u20, u22, u24) = (uid(20), uid(22), uid(24));
    db.seed(&u20, 0).await;
    db.seed(&u22, 0).await;
    db.seed(&u24, 5).await;
    let spawn = |kind: u8, u: &str| {
        let (app, g, u) = (db.t.app.clone(), db.g.clone(), u.to_string());
        tokio::spawn(async move {
            match kind {
                0 => matches!(svc::claim_daily(&app, &g, &u, Some(&s()), &[]).await.expect("daily"), svc::DailyClaim::Claimed { .. }),
                1 => matches!(svc::claim_work(&app, &g, &u, Some(&s()), &[]).await.expect("work"), svc::WorkClaim::Claimed { .. }),
                _ => matches!(svc::claim_pity(&app, &g, &u, Some(&s())).await.expect("pity"), svc::PityClaim::Claimed { .. }),
            }
        })
    };
    for (kind, u, n, balance) in [(0u8, &u20, 3, 100), (1, &u22, 4, 200), (2, &u24, 3, 505)] {
        let tasks: Vec<_> = (0..n).map(|_| spawn(kind, u)).collect();
        let mut claimed = 0;
        for task in tasks {
            if task.await.expect("join") {
                claimed += 1;
            }
        }
        assert_eq!(claimed, 1, "kind {kind}");
        assert_eq!(db.balance(u).await, balance, "kind {kind}");
    }
    assert_eq!(db.account(&u20).await.map(|a| a.daily_streak), Some(1));
    db.done().await;
}

#[tokio::test]
async fn work_cooldown_and_custom_cooldown() {
    let Some(db) = Db::new(14, &[21]).await else { return };
    let u = uid(21);
    db.seed(&u, 0).await;
    let first = svc::claim_work(&db.t, &db.g, &u, Some(&s()), &[]).await.expect("work");
    let svc::WorkClaim::Claimed { amount, balance, boost, line, next_at, .. } = first else { panic!("claimed") };
    assert_eq!((amount, balance, boost), (200, 200, None));
    assert!(line.starts_with("You "));
    assert!((next_at - (Utc::now() + chrono::Duration::hours(1))).num_milliseconds().abs() < 5000);
    assert!(matches!(svc::claim_work(&db.t, &db.g, &u, Some(&s()), &[]).await.expect("again"), svc::WorkClaim::NotYet { .. }));
    db.hours_ago(&u, "lastWork", 0.5).await;
    let custom = svc::claim_work(&db.t, &db.g, &u, Some(&s_with(json!({ "workCooldownMinutes": 20 }))), &[]).await.expect("custom");
    assert!(matches!(custom, svc::WorkClaim::Claimed { .. }), "30 min ago, 20 min cooldown");
    db.done().await;
}

#[tokio::test]
async fn pity_threshold_and_cooldown() {
    let Some(db) = Db::new(15, &[23]).await else { return };
    let u = uid(23);
    db.seed(&u, 50).await;
    assert_eq!(svc::claim_pity(&db.t, &db.g, &u, Some(&s())).await.expect("pity").to_json(), json!({ "claimed": true, "amount": 500, "balance": 550 }));
    assert_eq!(svc::claim_pity(&db.t, &db.g, &u, Some(&s())).await.expect("rich"), svc::PityClaim::TooRich { balance: 550 });
    db.set_balance_raw(&u, 10).await;
    let svc::PityClaim::NotYet { next_at } = svc::claim_pity(&db.t, &db.g, &u, Some(&s())).await.expect("cooling") else { panic!("cooling") };
    assert!(next_at > Utc::now());
    db.hours_ago(&u, "lastPity", 13.0).await;
    assert!(matches!(svc::claim_pity(&db.t, &db.g, &u, Some(&s())).await.expect("again"), svc::PityClaim::Claimed { .. }));
    db.done().await;
}

#[tokio::test]
async fn boosts_create_limits_apply_once_per_period_and_roll_back_on_cooldown() {
    let Some(db) = Db::new(16, &[25, 26, 27]).await else { return };
    let (u25, u26, u27) = (uid(25), uid(26), uid(27));
    let create = |name: &str, percent: i64, period: BoostPeriod, role: Option<&str>| {
        let (app, g) = (db.t.app.clone(), db.g.clone());
        let data = NewBoost { name: name.into(), percent, period, role_id: role.map(str::to_string), created_by: "admin".into() };
        async move { svc::create_boost(&app, &g, data).await }
    };
    let reason = |r: anyhow::Result<EconomyBoost>| r.err().as_ref().and_then(svc::economy_error).and_then(|e| e.reason().map(str::to_string));
    assert_eq!(reason(create("Boost", 0, BoostPeriod::Daily, None).await).as_deref(), Some("percent"));
    assert_eq!(reason(create("Boost", 501, BoostPeriod::Daily, None).await).as_deref(), Some("percent"));
    let base = create("Base", 50, BoostPeriod::Daily, None).await.expect("base");
    assert_eq!(reason(create("BASE", 50, BoostPeriod::Daily, None).await).as_deref(), Some("name-taken"));
    let once = create("Welcome", 100, BoostPeriod::OneTime, Some("777")).await.expect("once");
    let rows: Vec<(String, i64)> = svc::list_boosts(&db.t, &db.g).await.expect("list").into_iter().map(|b| (b.boost.name, b.count.uses)).collect();
    assert_eq!(rows, vec![("Base".to_string(), 0), ("Welcome".to_string(), 0)]);
    assert_eq!(svc::resolve_boost(&db.t, &db.g, "welcome").await.expect("resolve").map(|b| b.id), Some(once.id.clone()));
    let found: Vec<String> = svc::search_boosts(&db.t, &db.g, "wel").await.expect("search").into_iter().map(|b| b.name).collect();
    assert_eq!(found, vec!["Welcome"]);

    db.seed(&u25, 0).await;
    let r1 = svc::claim_daily(&db.t, &db.g, &u25, Some(&s()), &[]).await.expect("claim 1");
    let svc::DailyClaim::Claimed { amount, boost, .. } = r1 else { panic!("claimed") };
    assert_eq!((amount, boost), (150, Some(svc::BoostInfo { name: "Base".into(), percent: 50 })));
    assert_eq!(db.balance(&u25).await, 150);
    let uses_sql = r#"SELECT COUNT(*)::bigint FROM "EconomyBoostUse" u JOIN "EconomyBoost" b ON b."id" = u."boostId" WHERE b."guildId" = $1"#;
    let before = db.count(uses_sql).await;
    let roles777 = vec!["777".to_string()];
    assert!(matches!(svc::claim_daily(&db.t, &db.g, &u25, Some(&s()), &roles777).await.expect("cooldown"), svc::DailyClaim::NotYet { .. }));
    assert_eq!(db.count(uses_sql).await, before, "a claim on cooldown does not burn a boost use");
    db.hours_ago(&u25, "lastDaily", 25.0).await;
    let r2 = svc::claim_daily(&db.t, &db.g, &u25, Some(&s()), &roles777).await.expect("claim 2");
    let svc::DailyClaim::Claimed { streak, amount, boost, balance, .. } = r2 else { panic!("claimed") };
    assert_eq!((streak, amount, boost.map(|b| b.name), balance), (2, 220, Some("Welcome".to_string()), 370));
    db.hours_ago(&u25, "lastDaily", 25.0).await;
    let r3 = svc::claim_daily(&db.t, &db.g, &u25, Some(&s()), &roles777).await.expect("claim 3");
    assert!(matches!(r3, svc::DailyClaim::Claimed { streak: 3, amount: 120, boost: None, .. }), "{r3:?}");

    db.seed(&u26, 0).await;
    let w1 = svc::claim_work(&db.t, &db.g, &u26, Some(&s()), &[]).await.expect("work 1");
    assert!(matches!(&w1, svc::WorkClaim::Claimed { amount: 300, boost: Some(b), .. } if b.name == "Base"), "{w1:?}");
    db.hours_ago(&u26, "lastWork", 2.0).await;
    assert!(matches!(svc::claim_work(&db.t, &db.g, &u26, Some(&s()), &[]).await.expect("work 2"), svc::WorkClaim::Claimed { boost: None, .. }));
    db.seed(&u27, 0).await;
    assert_eq!(svc::set_boost_enabled(&db.t, &db.g, &base.id, false).await.expect("toggle").map(|b| b.enabled), Some(false));
    assert!(matches!(svc::claim_work(&db.t, &db.g, &u27, Some(&s()), &[]).await.expect("work 3"), svc::WorkClaim::Claimed { boost: None, .. }));
    assert!(svc::set_boost_enabled(&db.t, &db.g, "missing", true).await.expect("missing").is_none());

    for n in 0..8 {
        create(&format!("Extra {n}"), n + 1, BoostPeriod::Daily, None).await.expect("extra");
    }
    assert_eq!(reason(create("One too many", 50, BoostPeriod::Daily, None).await).as_deref(), Some("limit"));
    assert!(svc::delete_boost(&db.t, &db.g, &once.id).await.expect("delete"));
    assert!(!svc::delete_boost(&db.t, &db.g, &once.id).await.expect("again"));
    let left: i64 = sqlx::query_scalar(r#"SELECT COUNT(*)::bigint FROM "EconomyBoostUse" WHERE "boostId" = $1"#).bind(&once.id).fetch_one(&db.t.pool).await.expect("uses");
    assert_eq!(left, 0);
    db.done().await;
}

#[tokio::test]
async fn boost_uses_are_recorded_per_iso_week_key() {
    let Some(db) = Db::new(17, &[28]).await else { return };
    let u = uid(28);
    db.t.settings().ensure_guild_settings(&db.g).await.expect("row");
    let b = svc::create_boost(&db.t, &db.g, NewBoost { name: "Keys".into(), percent: 10, period: BoostPeriod::Weekly, role_id: None, created_by: "a".into() })
        .await
        .expect("boost");
    db.seed(&u, 0).await;
    svc::claim_work(&db.t, &db.g, &u, Some(&s_with(json!({ "timezone": "Europe/London" }))), &[]).await.expect("work");
    let uses: Vec<(String, String)> = sqlx::query_as(r#"SELECT "userId", "periodKey" FROM "EconomyBoostUse" WHERE "boostId" = $1"#).bind(&b.id).fetch_all(&db.t.pool).await.expect("uses");
    assert_eq!(uses.len(), 1);
    assert!(regex::Regex::new(r"^W:\d{4}-W\d{2}$").expect("re").is_match(&uses[0].1), "{uses:?}");
    assert_eq!(uses[0].0, u);
    db.done().await;
}

#[tokio::test]
async fn shop_purchases_are_atomic_and_roll_back_on_failure() {
    let Some(db) = Db::new(18, &[29]).await else { return };
    let u = uid(29);
    db.seed(&u, 1000).await;
    let item = db.item(json!({ "name": "Gem", "price": 100, "type": "STATIC", "stock": 3, "canBuyMultiple": true, "maxPerUser": 4 })).await;
    let buy = |q: i64, id: &str| {
        let (app, g, u, id) = (db.t.app.clone(), db.g.clone(), u.clone(), id.to_string());
        async move { svc::purchase_item(&app, &g, &u, &id, q, Some(&s())).await }
    };
    let r1 = buy(2, &item.id).await.expect("buy");
    assert_eq!((r1.total, r1.balance, r1.owned, r1.to_inventory, r1.item.stock), (200, 800, Some(2), true, Some(1)));
    assert_eq!(code_of(buy(2, &item.id).await), Some(EconomyCode::OutOfStock));
    let upd = |data: Value| {
        let (app, g, id) = (db.t.app.clone(), db.g.clone(), item.id.clone());
        async move { svc::update_item(&app, &g, &id, data.as_object().cloned().unwrap_or_default()).await.expect("update") }
    };
    upd(json!({ "stock": 10, "price": 900 })).await;
    let err = buy(1, &item.id).await.expect_err("insufficient");
    assert_eq!(svc::economy_error(&err).map(|e| (e.code, e.data_i64("total"))), Some((EconomyCode::Insufficient, Some(900))));
    upd(json!({ "price": 1 })).await;
    let err = buy(3, &item.id).await.expect_err("max owned");
    assert_eq!(svc::economy_error(&err).map(|e| (e.code, e.data_i64("max"))), Some((EconomyCode::MaxOwned, Some(4))));
    let stock = svc::resolve_item(&db.t, &db.g, &item.id, false).await.expect("item").and_then(|i| i.stock);
    assert_eq!((stock, db.balance(&u).await, svc::get_owned_quantity(&db.t, &db.g, &u, &item.id).await.expect("owned")), (Some(10), 800, 2));
    assert_eq!(buy(2, &item.id).await.expect("buy").owned, Some(4));
    assert_eq!(code_of(buy(1, &item.id).await), Some(EconomyCode::MaxOwned));
    assert_eq!(db.balance(&u).await, 798);
    upd(json!({ "enabled": false })).await;
    assert_eq!(code_of(buy(1, &item.id).await), Some(EconomyCode::OutOfStock));
    assert_eq!(code_of(buy(1, "missing").await), Some(EconomyCode::OutOfStock));
    db.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shop_the_last_copy_and_a_double_click_go_to_exactly_one_purchase() {
    let Some(db) = Db::new(19, &[30, 31, 32]).await else { return };
    let (a, b, c) = (uid(30), uid(31), uid(32));
    db.seed(&a, 500).await;
    db.seed(&b, 500).await;
    db.seed(&c, 1000).await;
    let last = db.item(json!({ "name": "Last One", "price": 100, "stock": 1 })).await;
    let unique = db.item(json!({ "name": "Unique", "price": 100, "canBuyMultiple": false })).await;
    let buy = |u: &str, id: &str| {
        let (app, g, u, id) = (db.t.app.clone(), db.g.clone(), u.to_string(), id.to_string());
        tokio::spawn(async move { svc::purchase_item(&app, &g, &u, &id, 1, Some(&s())).await.map_err(|e| svc::economy_error(&e).map(|e| e.code)) })
    };
    let results = [buy(&a, &last.id).await.expect("join"), buy(&b, &last.id).await.expect("join")];
    let (x, y) = (buy(&a, &last.id), buy(&b, &last.id));
    let _ = (x.await, y.await);
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(results.iter().find_map(|r| r.as_ref().err().cloned()), Some(Some(EconomyCode::OutOfStock)));
    assert_eq!(svc::resolve_item(&db.t, &db.g, &last.id, false).await.expect("item").and_then(|i| i.stock), Some(0));
    assert_eq!(db.balance(&a).await + db.balance(&b).await, 900);
    let (p, q) = (buy(&c, &unique.id), buy(&c, &unique.id));
    let both = [p.await.expect("join"), q.await.expect("join")];
    assert_eq!(both.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(db.balance(&c).await, 900);
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &c, &unique.id).await.expect("owned"), 1);
    db.done().await;
}

#[tokio::test]
async fn use_on_buy_role_items_skip_the_inventory_and_refunds_restore_everything() {
    let Some(db) = Db::new(21, &[33, 51]).await else { return };
    let (u, v) = (uid(33), uid(51));
    db.seed(&u, 1000).await;
    let role = db
        .item(json!({ "name": "VIP", "price": 300, "type": "ROLE", "roleId": "4242", "useOnBuy": true, "destroyOnUse": true, "canBuyMultiple": false, "stock": 5 }))
        .await;
    let res = svc::purchase_item(&db.t, &db.g, &u, &role.id, 1, Some(&s())).await.expect("buy");
    assert_eq!((res.to_inventory, res.owned, res.balance, res.item.stock), (false, None, 700, Some(4)));
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &role.id).await.expect("owned"), 0);
    let refund = |item: &bot_db::ShopItem, user: &str, q, total, inv, st: &GuildSettings| {
        let (app, g, item, user, st) = (db.t.app.clone(), db.g.clone(), item.clone(), user.to_string(), st.clone());
        async move {
            svc::refund_purchase(&app, svc::Refund { guild_id: &g, user_id: &user, item: &item, quantity: q, total, to_inventory: inv, settings: Some(&st) })
                .await
                .expect("refund")
        }
    };
    refund(&res.item, &u, 1, res.total, res.to_inventory, &s()).await;
    let stock = |id: String| {
        let (app, g) = (db.t.app.clone(), db.g.clone());
        async move { svc::resolve_item(&app, &g, &id, false).await.expect("item").and_then(|i| i.stock) }
    };
    assert_eq!((db.balance(&u).await, stock(role.id.clone()).await), (1000, Some(5)));
    let gem = db.item(json!({ "name": "Gem", "price": 50, "stock": 2 })).await;
    let bought = svc::purchase_item(&db.t, &db.g, &u, &gem.id, 2, Some(&s())).await.expect("buy gem");
    assert_eq!(bought.owned, Some(2));
    refund(&bought.item, &u, 2, bought.total, true, &s()).await;
    let rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*)::bigint FROM "InventoryItem" WHERE "itemId" = $1"#).bind(&gem.id).fetch_one(&db.t.pool).await.expect("rows");
    assert_eq!((rows, db.balance(&u).await, stock(gem.id.clone()).await), (0, 1000, Some(2)));

    // A refund returns the full price even when the buyer reached the server maximum meanwhile.
    let capped = s_with(json!({ "economyMaxBalance": 1000 }));
    db.seed(&v, 1000).await;
    let cap_item = db
        .item(json!({ "name": "Cap Refund", "price": 300, "type": "ROLE", "roleId": "4243", "useOnBuy": true, "destroyOnUse": true, "canBuyMultiple": false, "stock": 3 }))
        .await;
    let res = svc::purchase_item(&db.t, &db.g, &v, &cap_item.id, 1, Some(&capped)).await.expect("buy");
    assert_eq!(res.balance, 700);
    svc::set_balance(&db.t, &db.g, &v, 1000, Some(&capped)).await.expect("set");
    refund(&res.item, &v, 1, res.total, res.to_inventory, &capped).await;
    assert_eq!(db.balance(&v).await, 1300, "the refund is not clamped to economyMaxBalance");
    assert_eq!(stock(cap_item.id.clone()).await, Some(3));
    db.done().await;
}

#[tokio::test]
async fn inventory_consume_restore_give_take_search_and_delete_cascade() {
    let Some(db) = Db::new(22, &[34]).await else { return };
    let u = uid(34);
    db.seed(&u, 0).await;
    let potion = db.item(json!({ "name": "Potion", "price": 10, "type": "USABLE", "destroyOnUse": true })).await;
    let relic = db.item(json!({ "name": "Relic", "price": 10, "type": "STATIC" })).await;
    assert_eq!(svc::give_item(&db.t, &db.g, &u, &potion.id, 2).await.expect("give"), 2);
    svc::give_item(&db.t, &db.g, &u, &relic.id, 1).await.expect("give");
    assert_eq!(svc::consume_item(&db.t, &db.g, &u, &potion.id).await.expect("use"), 1);
    assert_eq!(svc::consume_item(&db.t, &db.g, &u, &potion.id).await.expect("use"), 0);
    let rows = |id: String| {
        let pool = db.t.pool.clone();
        async move { sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*)::bigint FROM "InventoryItem" WHERE "itemId" = $1"#).bind(id).fetch_one(&pool).await.expect("rows") }
    };
    assert_eq!(rows(potion.id.clone()).await, 0, "rows at 0 are deleted");
    assert_eq!(code_of(svc::consume_item(&db.t, &db.g, &u, &potion.id).await), Some(EconomyCode::NotOwned));
    svc::restore_item(&db.t, &db.g, &u, &potion.id, 1).await;
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &potion.id).await.expect("owned"), 1);
    let err = svc::give_item(&db.t, &db.g, &u, &potion.id, 200).await.expect_err("hard cap");
    assert_eq!(svc::economy_error(&err).map(|e| (e.code, e.data_i64("owned"), e.data_i64("max"))), Some((EconomyCode::MaxOwned, Some(1), Some(200))));
    assert_eq!(code_of(svc::give_item(&db.t, &db.g, &u, "missing", 1).await), Some(EconomyCode::ItemGone));
    assert_eq!(svc::take_item(&db.t, &db.g, &u, &potion.id, 5).await.expect("take"), svc::Taken { removed: 1, remaining: 0 });
    assert_eq!(code_of(svc::take_item(&db.t, &db.g, &u, &potion.id, 1).await), Some(EconomyCode::NotOwned));
    let inv: Vec<(String, i32)> = svc::get_inventory(&db.t, &db.g, &u).await.expect("inv").into_iter().map(|r| (r.item.name, r.row.quantity)).collect();
    assert_eq!(inv, vec![("Relic".to_string(), 1)]);
    svc::give_item(&db.t, &db.g, &u, &potion.id, 3).await.expect("give");
    let names = |v: Vec<svc::InventoryEntry>| v.into_iter().map(|r| r.item.name).collect::<Vec<_>>();
    assert_eq!(names(svc::search_inventory(&db.t, &db.g, &u, "pot", true).await.expect("search")), vec!["Potion"]);
    assert!(names(svc::search_inventory(&db.t, &db.g, &u, "rel", true).await.expect("search")).is_empty(), "collectibles cannot be used");
    assert_eq!(names(svc::search_inventory(&db.t, &db.g, &u, "rel", false).await.expect("search")), vec!["Relic"]);
    assert_eq!(svc::delete_item(&db.t, &db.g, &potion.id).await.expect("delete"), 3);
    assert_eq!(rows(potion.id.clone()).await, 0);
    assert_eq!(svc::delete_item(&db.t, &db.g, &potion.id).await.expect("again"), 0);
    db.done().await;
}

#[tokio::test]
async fn shop_items_unique_names_caps_guild_scoping_and_lookups() {
    let Some(db) = Db::new(23, &[34]).await else { return };
    let other = Db::new(24, &[]).await.expect("same database");
    let a = db.item(json!({ "name": "Sword", "price": 50 })).await;
    other.item(json!({ "name": "Sword", "price": 70 })).await;
    let reason = |r: anyhow::Result<bot_db::ShopItem>| r.err().as_ref().and_then(svc::economy_error).and_then(|e| e.reason().map(str::to_string));
    let named = |n: &str| json!({ "name": n, "price": 1 }).as_object().cloned().unwrap_or_default();
    assert_eq!(reason(svc::create_item(&db.t, &db.g, named("SWORD")).await).as_deref(), Some("name-taken"));
    assert_eq!(svc::find_item_by_name_insensitive(&db.t, &db.g, "sWoRd", None).await.expect("find").map(|i| i.id), Some(a.id.clone()));
    assert!(svc::find_item_by_name_insensitive(&db.t, &db.g, "sword", Some(&a.id)).await.expect("find").is_none());
    assert_eq!(svc::resolve_item(&db.t, &db.g, "  sword ", false).await.expect("resolve").map(|i| i.id), Some(a.id.clone()));
    assert!(svc::resolve_item(&other.t, &other.g, &a.id, false).await.expect("resolve").is_none(), "an id from another guild never resolves");
    assert_eq!(code_of(svc::give_item(&other.t, &other.g, &uid(34), &a.id, 1).await), Some(EconomyCode::ItemGone));
    assert!(svc::resolve_item(&db.t, &db.g, &a.id, true).await.expect("resolve").is_some());
    let price1 = json!({ "price": 1 }).as_object().cloned().unwrap_or_default();
    svc::update_item(&db.t, &db.g, &a.id, json!({ "enabled": false }).as_object().cloned().unwrap_or_default()).await.expect("disable");
    assert!(svc::resolve_item(&db.t, &db.g, &a.id, true).await.expect("resolve").is_none());
    assert_eq!(code_of(svc::update_item(&other.t, &other.g, &a.id, price1.clone()).await), Some(EconomyCode::ItemGone));
    assert_eq!(code_of(svc::update_item(&db.t, &db.g, "missing", price1).await), Some(EconomyCode::ItemGone));
    let shield = db.item(json!({ "name": "Shield", "price": 20 })).await;
    let rename = svc::update_item(&db.t, &db.g, &shield.id, json!({ "name": "sword" }).as_object().cloned().unwrap_or_default()).await;
    assert_eq!(rename.err().as_ref().and_then(svc::economy_error).and_then(|e| e.reason().map(str::to_string)).as_deref(), Some("name-taken"));
    assert_eq!(svc::count_items(&db.t, &db.g).await.expect("count"), 2);
    let names = |v: Vec<svc::ItemChoice>| v.into_iter().map(|i| i.name).collect::<Vec<_>>();
    assert_eq!(names(svc::search_items(&db.t, &db.g, "S", false, 25).await.expect("search")), vec!["Shield", "Sword"]);
    assert_eq!(names(svc::search_items(&db.t, &db.g, "S", true, 25).await.expect("search")), vec!["Shield"]);
    let list = |enabled, by_name| {
        let (app, g) = (db.t.app.clone(), db.g.clone());
        async move { svc::list_items(&app, &g, enabled, by_name).await.expect("list").into_iter().map(|i| i.name).collect::<Vec<_>>() }
    };
    assert_eq!(list(false, true).await, vec!["Shield", "Sword"]);
    assert_eq!(list(true, false).await, vec!["Shield"]);
    // The shop cap.
    let q = format!(
        r#"INSERT INTO "ShopItem" ("id","guildId","name","price","updatedAt") SELECT 'f' || n || $1, $1, 'Filler ' || n, 1, {SQL_NOW} FROM generate_series(1, 298) AS n"#
    );
    sqlx::query(sqlx::AssertSqlSafe(q)).bind(&db.g).execute(&db.t.pool).await.expect("fillers");
    assert_eq!(reason(svc::create_item(&db.t, &db.g, named("Over the cap")).await).as_deref(), Some("limit"));
    other.done().await;
    db.done().await;
}

#[tokio::test]
async fn reset_economy_coins_items_one_member_and_the_import_switch() {
    let Some(db) = Db::new(25, &[35, 36, 40, 41]).await else { return };
    let (u35, u36, u40, u41) = (uid(35), uid(36), uid(40), uid(41));
    db.legacy(&u40, 9000).await;
    db.seed(&u35, 5000).await;
    db.hours_ago(&u35, "lastDaily", 1.0).await;
    db.hours_ago(&u35, "lastWork", 1.0).await;
    db.hours_ago(&u35, "lastPity", 1.0).await;
    db.seed(&u36, 800).await;
    let item = db.item(json!({ "name": "ResetMe", "price": 5 })).await;
    svc::give_item(&db.t, &db.g, &u35, &item.id, 2).await.expect("give");
    svc::give_item(&db.t, &db.g, &u36, &item.id, 1).await.expect("give");
    let st = s_with(json!({ "economyStartingBalance": 250 }));
    let counts = |a, i| svc::ResetCounts { accounts: a, inventories: i };
    assert_eq!(svc::reset_economy(&db.t, &db.g, Some(&u35), ResetWhat::Coins, Some(&st)).await.expect("reset"), counts(1, 0));
    let one = db.account(&u35).await.expect("row");
    assert_eq!((one.balance, one.daily_streak, one.last_daily, one.last_work, one.last_pity), (250, 0, None, None, None));
    assert_eq!(db.balance(&u36).await, 800);
    let fresh = |app: &App, g: String| {
        let app = app.clone();
        async move { app.settings().get_settings_with(&g, true).await.expect("settings").economy_import_global }
    };
    assert!(fresh(&db.t, db.g.clone()).await, "a single-member reset leaves the switch alone");
    assert_eq!(svc::reset_economy(&db.t, &db.g, Some(&u41), ResetWhat::Coins, Some(&st)).await.expect("reset"), counts(1, 0));
    assert_eq!(db.balance(&u41).await, 250);
    assert_eq!(svc::reset_economy(&db.t, &db.g, Some(&u36), ResetWhat::Items, Some(&st)).await.expect("reset"), counts(0, 1));
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u35, &item.id).await.expect("owned"), 2);
    let all = svc::reset_economy(&db.t, &db.g, None, ResetWhat::All, Some(&st)).await.expect("reset all");
    assert_eq!(all.inventories, 1);
    assert!(all.accounts >= 3);
    assert_eq!(db.count(r#"SELECT COUNT(*)::bigint FROM "InventoryItem" WHERE "guildId" = $1"#).await, 0);
    assert!(svc::resolve_item(&db.t, &db.g, &item.id, false).await.expect("item").is_some());
    assert!(!fresh(&db.t, db.g.clone()).await);
    let mut row = (*db.t.settings().get_settings_with(&db.g, true).await.expect("settings")).clone();
    row.economy_starting_balance = 250;
    let late = svc::get_account(&db.t, &db.g, &u40, Some(&row)).await.expect("latecomer");
    assert_eq!((late.balance, late.imported_global), (250, false));
    db.done().await;
}

#[tokio::test]
async fn coins_rewards_provider_grant_take_insufficient_disabled_and_bad_amounts() {
    let Some(db) = Db::new(26, &[42]).await else { return };
    let u = uid(42);
    let patch = SettingsPatch::new().set("economyEnabled", true).set("economyMaxBalance", 1000).set("economyStartingBalance", 100);
    db.t.settings().update_settings(&db.g, patch).await.expect("settings");
    let guild = serenity::all::GuildId::new(db.g.parse().expect("snowflake"));
    let args = |n: i64| RewardArgs::new(guild, u.clone(), n).reason("test");
    let ok = |total| RewardResult { ok: true, total: Some(total), reason: None };
    assert_eq!(svc::grant_coins_reward(&db.t, &args(50)).await, ok(150));
    assert_eq!(svc::grant_coins_reward(&db.t, &args(5000)).await, ok(1000), "clamped to the server maximum, still ok");
    assert_eq!(svc::take_coins_reward(&db.t, &args(400)).await, ok(600));
    assert_eq!(svc::take_coins_reward(&db.t, &args(601)).await, RewardResult { ok: false, total: Some(600), reason: Some("insufficient".into()) });
    assert_eq!(db.balance(&u).await, 600);
    for n in [0, -1, COIN_MAX + 1] {
        assert_eq!(svc::grant_coins_reward(&db.t, &args(n)).await.reason.as_deref(), Some("invalid-amount"));
    }
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", false)).await.expect("off");
    assert_eq!(svc::grant_coins_reward(&db.t, &args(5)).await, RewardResult::fail("module-disabled"));
    assert_eq!(svc::take_coins_reward(&db.t, &args(5)).await, RewardResult::fail("module-disabled"));
    assert_eq!(db.balance(&u).await, 600);
    // Through the rewards bus, as giveaways and automations call it.
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("on");
    db.t.install(registry());
    let r = db.t.rewards().grant(&db.t.app, RewardKind::Coins, args(10)).await;
    assert_eq!((r.ok, r.total), (true, Some(610)));
    db.done().await;
}

// ═══════════════════════ commands against the database (through the router) ═══════════════════════

fn slash(name: &str, g: &str, u: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user(u)
}

fn admin(g: &str, group: Option<&str>, sub: &str) -> FakeInteraction {
    let fi = FakeInteraction::slash("economy-admin").guild(g).user(&uid(49)).roles(&["820000000000000002"]).permissions(Permissions::MANAGE_GUILD);
    match group {
        Some(grp) => fi.group(grp, sub),
        None => fi.subcommand(sub),
    }
}

async fn route(t: &DbTestApp, fi: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    t.responses.clear();
    bot_core::router::route_interaction(t, fi.build()).await;
    t.responses.calls()
}

fn titles(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.iter().filter_map(|c| c.embed_title()).collect::<Vec<_>>().join(" | ")
}

fn method_names(calls: &[bot_core::interaction::RecordedCall]) -> Vec<&'static str> {
    calls.iter().map(|c| c.method).collect()
}

#[tokio::test]
async fn daily_and_work_claim_then_a_private_cooldown_notice_and_pity_too_rich() {
    let Some(db) = Db::new(30, &[43]).await else { return };
    let u = uid(43);
    db.t.install(registry());
    let patch = SettingsPatch::new().set("economyEnabled", true).set("dailyMin", 100).set("dailyMax", 100).set("workMin", 200).set("workMax", 200);
    db.t.settings().update_settings(&db.g, patch).await.expect("settings");
    db.seed(&u, 0).await;
    for (name, title, cooldown) in [("daily", "Daily Reward Claimed", "Already Claimed"), ("work", "Shift Complete", "Still Tired")] {
        let first = route(&db.t, slash(name, &db.g, &u)).await;
        assert_eq!(method_names(&first), vec!["deferReply", "editReply"], "{name}");
        assert!(first[1].embed_title().unwrap_or("").contains(title), "{}", titles(&first));
        let second = route(&db.t, slash(name, &db.g, &u)).await;
        assert_eq!(method_names(&second), vec!["deferReply", "deleteReply", "followUp"], "{name}");
        assert!(second[2].ephemeral());
        assert!(second[2].embed_title().unwrap_or("").contains(cooldown));
    }
    let daily_desc = route(&db.t, slash("balance", &db.g, &u)).await;
    let body = &daily_desc[1].body["embeds"][0];
    assert_eq!(body["description"], "🪙 **300** coins");
    assert_eq!(body["title"], "💰 member's balance");
    let pity = route(&db.t, slash("pity", &db.g, &u)).await;
    assert!(pity[2].embed_title().unwrap_or("").contains("Pity Not Available"), "{}", titles(&pity));
    let richest = route(&db.t, slash("richest", &db.g, &u)).await;
    assert_eq!(richest[1].body["embeds"][0]["description"], format!("🥇 <@{u}> — 🪙 **300** coins"));
    assert!(richest[1].body["embeds"][0]["footer"]["text"].as_str().unwrap_or("").starts_with("Your rank: #1 · 🪙 300 coins"));
    db.done().await;
}

#[tokio::test]
async fn pay_moves_coins_and_refuses_an_overdraft_privately() {
    let Some(db) = Db::new(31, &[44, 45]).await else { return };
    let (payer, rec) = (uid(44), uid(45));
    db.t.install(registry());
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("settings");
    db.seed(&payer, 500).await;
    db.seed(&rec, 10).await;
    let pay = |n: i64| {
        let fi = slash("pay", &db.g, &payer).option("member", option_type::USER, json!(rec)).option("amount", option_type::INTEGER, json!(n));
        resolved(fi, &rec, false, true)
    };
    let ok = route(&db.t, pay(200)).await;
    assert_eq!(method_names(&ok), vec!["deferReply", "editReply"]);
    assert!(ok[1].embed_title().unwrap_or("").contains("Coins Sent"));
    assert_eq!(ok[1].embed_description(), Some(format!("member gave 🪙 **200** coins to <@{rec}>.").as_str()));
    assert_eq!((db.balance(&payer).await, db.balance(&rec).await), (300, 210));
    let broke = route(&db.t, pay(301)).await;
    assert_eq!(method_names(&broke), vec!["deferReply", "deleteReply", "followUp"]);
    assert!(broke[2].embed_title().unwrap_or("").contains("Insufficient Funds"));
    assert_eq!(db.balance(&payer).await, 300);
    db.done().await;
}

fn shop_guild(g: &str, buyer: &str, role: &str) -> FakeGuild {
    FakeGuild::new(g, "955026900000000099")
        .role("820000000000000001", 10, Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role(role, 1, Permissions::empty())
        .member(buyer, &[])
}

#[tokio::test]
async fn shop_buy_purchase_hook_payload_and_a_refund_when_the_role_cannot_be_granted() {
    let Some(db) = Db::new(32, &[46]).await else { return };
    let u = uid(46);
    db.t.install(registry());
    db.t.set_discord(offline_discord_as(&[shop_guild(&db.g, &u, "820000000000004242").json()]));
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("settings");
    db.seed(&u, 1000).await;
    let gem = db.item(json!({ "name": "Shiny Gem", "price": 100, "stock": 5, "buyMessage": "Enjoy {quantity}x {item}!" })).await;
    let vip = db
        .item(json!({ "name": "VIP Role", "price": 400, "type": "ROLE", "roleId": "820000000000004242", "useOnBuy": true, "destroyOnUse": true, "canBuyMultiple": false }))
        .await;
    let buy = |item: &str, q: Option<i64>| {
        let mut fi = slash("shop", &db.g, &u).subcommand("buy").option("item", option_type::STRING, json!(item));
        if let Some(q) = q {
            fi = fi.option("quantity", option_type::INTEGER, json!(q));
        }
        fi
    };
    let first = route(&db.t, buy(&gem.id, Some(2))).await;
    assert_eq!(method_names(&first), vec!["deferReply", "editReply"]);
    assert!(first[1].embed_title().unwrap_or("").contains("Purchase Complete"));
    assert!(first[1].embed_description().unwrap_or("").contains("Enjoy 2x Shiny Gem!"));
    assert_eq!(db.balance(&u).await, 800);
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &gem.id).await.expect("owned"), 2);

    // Offline the role grant fails after the commit: coins and stock come back.
    let failing = route(&db.t, buy(&vip.id, None)).await;
    assert_eq!(method_names(&failing), vec!["deferReply", "deleteReply", "followUp"]);
    assert!(failing[2].embed_title().unwrap_or("").contains("Purchase Refunded"), "{}", titles(&failing));
    assert_eq!(db.balance(&u).await, 800);
    let unknown = route(&db.t, buy("nope", None)).await;
    assert!(unknown[2].embed_title().unwrap_or("").contains("Item Not Found"));
    assert_eq!(unknown[2].embed_description(), Some("There is no item called **nope** in the shop."));
    let too_many = route(&db.t, buy(&vip.id, Some(2))).await;
    assert!(too_many[2].embed_title().unwrap_or("").contains("One Per Member"));

    // view and info
    let view = route(&db.t, slash("shop", &db.g, &u).subcommand("view")).await;
    let page = &view[1].body["embeds"][0];
    assert_eq!(page["title"], "🛒 Shop");
    assert_eq!(page["description"], "Buy with `/shop buy`. You have 🪙 **800** coins.");
    assert_eq!(page["fields"][0]["name"], "Shiny Gem — 🪙 100");
    assert_eq!(page["fields"][0]["value"], "*No description*\n📦 Collectible · 3 left");
    assert_eq!(page["fields"][1]["value"], "*No description*\n🎭 Role · gives <@&820000000000004242> · one per member");
    let info = route(&db.t, slash("shop", &db.g, &u).subcommand("info").option("item", option_type::STRING, json!("shiny gem"))).await;
    let fields: Vec<(String, String)> = info[1].body["embeds"][0]["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into()))
        .collect();
    assert_eq!(fields.last(), Some(&("You own".to_string(), "2".to_string())));
    assert_eq!(fields[0], ("Price".to_string(), "🪙 **100** coins".to_string()));

    // inventory
    let inv = route(&db.t, slash("inventory", &db.g, &u)).await;
    assert_eq!(inv[1].body["embeds"][0]["description"], "**Shiny Gem** ×2 · 📦 Collectible");
    db.done().await;
}

#[tokio::test]
async fn shop_autocomplete_and_the_economy_purchase_hook() {
    let Some(db) = Db::new(33, &[47]).await else { return };
    let u = uid(47);
    db.t.install(registry());
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true).set("currencyName", "gems")).await.expect("settings");
    db.seed(&u, 1000).await;
    let gem = db.item(json!({ "name": "Shiny Gem", "price": 1234 })).await;
    db.item(json!({ "name": "Hidden Gem", "price": 1, "enabled": false })).await;
    let ac = FakeInteraction::autocomplete("shop").guild(&db.g).user(&u).subcommand("buy").focused("item", "gem");
    let calls = route(&db.t, ac).await;
    assert_eq!(calls[0].choices(), vec![json!({ "name": "Shiny Gem — 1,234 gems", "value": gem.id })]);
    let ac = FakeInteraction::autocomplete("economy-admin").guild(&db.g).user(&u).permissions(Permissions::MANAGE_GUILD).group("item", "delete").focused("item", " gem ");
    let calls = route(&db.t, ac).await;
    let names: Vec<String> = calls[0].choices().iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
    assert_eq!(names, vec!["Hidden Gem (disabled) — 1", "Shiny Gem — 1,234"]);
    // The economyPurchase payload reaches the hook runner with the JS fields.
    let dispatched = db
        .t
        .run_hooks(EconomyPurchase {
            guild_id: serenity::all::GuildId::new(db.g.parse().expect("id")),
            member: None,
            user_id: serenity::all::UserId::new(u.parse().expect("id")),
            item: gem.clone(),
            quantity: 2,
            total_price: 2468,
            channel_id: None,
        })
        .await;
    assert_eq!(dispatched.data.total_price, 2468);
    db.done().await;
}

#[tokio::test]
async fn use_consumes_a_copy_returns_it_when_the_role_grant_fails_and_refuses_collectibles() {
    let Some(db) = Db::new(34, &[48]).await else { return };
    let u = uid(48);
    db.t.install(registry());
    db.t.set_discord(offline_discord_as(&[shop_guild(&db.g, &u, "820000000000004243").json()]));
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("settings");
    db.seed(&u, 0).await;
    let potion = db.item(json!({ "name": "Elixir", "price": 5, "type": "USABLE", "destroyOnUse": true, "useMessage": "{user.name} drank {item}." })).await;
    let relic = db.item(json!({ "name": "Trinket", "price": 5, "type": "STATIC" })).await;
    let badge = db.item(json!({ "name": "Badge", "price": 5, "type": "ROLE", "roleId": "820000000000004243", "destroyOnUse": true })).await;
    for it in [&potion, &relic, &badge] {
        svc::give_item(&db.t, &db.g, &u, &it.id, 2).await.expect("give");
    }
    let run = |item: &str| slash("use", &db.g, &u).option("item", option_type::STRING, json!(item));
    let used = route(&db.t, run(&potion.id)).await;
    assert!(used[1].embed_title().unwrap_or("").contains("Item Used"), "{}", titles(&used));
    assert!(used[1].embed_description().unwrap_or("").ends_with(" drank Elixir."), "{:?}", used[1].embed_description());
    assert_eq!(used[1].body["embeds"][0]["fields"][0]["value"], "1");
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &potion.id).await.expect("owned"), 1);
    let collectible = route(&db.t, run(&relic.id)).await;
    assert!(collectible[2].embed_title().unwrap_or("").contains("Not Usable"));
    let failed = route(&db.t, run(&badge.id)).await;
    assert!(failed[2].embed_title().unwrap_or("").contains("Could Not Use"), "{}", titles(&failed));
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &badge.id).await.expect("owned"), 2, "returned");
    let ghost = route(&db.t, run("ghost")).await;
    assert!(ghost[2].embed_title().unwrap_or("").contains("Item Not Found"));
    let ac = route(&db.t, FakeInteraction::autocomplete("use").guild(&db.g).user(&u).focused("item", "")).await;
    let names: Vec<String> = ac[0].choices().iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
    assert_eq!(names, vec!["Badge ×2", "Elixir ×1"]);
    db.done().await;
}

#[tokio::test]
async fn economy_admin_boost_add_toggle_list_and_remove() {
    let Some(db) = Db::new(35, &[49]).await else { return };
    db.t.install(registry());
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("settings");
    let add = |name: &str, pct: i64, period: &str| {
        admin(&db.g, Some("boost"), "add")
            .option("name", option_type::STRING, json!(name))
            .option("percent", option_type::INTEGER, json!(pct))
            .option("period", option_type::STRING, json!(period))
    };
    let added = route(&db.t, add("Happy Hour", 25, "daily")).await;
    assert_eq!(method_names(&added), vec!["deferReply", "editReply"]);
    assert!(added[1].embed_title().unwrap_or("").contains("Boost Added"));
    assert_eq!(added[1].embed_description(), Some("**Happy Hour** gives everyone **+25%** on one /daily or /work payout, once per member per day."));
    let dup = route(&db.t, add("happy hour", 5, "weekly")).await;
    assert!(dup[2].embed_title().unwrap_or("").contains("Name Taken"));
    let listed = route(&db.t, admin(&db.g, Some("boost"), "list")).await;
    assert_eq!(listed[1].embed_description(), Some("**Happy Hour** · +25% · Daily · everyone · ✅ on · 0 uses"));
    let named = |sub: &str| admin(&db.g, Some("boost"), sub).option("name", option_type::STRING, json!("Happy Hour"));
    assert!(route(&db.t, named("toggle")).await[1].embed_title().unwrap_or("").contains("Boost Disabled"));
    assert_eq!(svc::resolve_boost(&db.t, &db.g, "Happy Hour").await.expect("boost").map(|b| b.enabled), Some(false));
    let ac = route(&db.t, FakeInteraction::autocomplete("economy-admin").guild(&db.g).user(&uid(49)).permissions(Permissions::MANAGE_GUILD).group("boost", "remove").focused("name", "happy")).await;
    assert_eq!(ac[0].choices()[0]["name"], "Happy Hour (+25%) (off)");
    assert!(route(&db.t, named("remove")).await[1].embed_title().unwrap_or("").contains("Boost Removed"));
    assert!(route(&db.t, named("remove")).await[2].embed_title().unwrap_or("").contains("Boost Not Found"));
    assert!(route(&db.t, admin(&db.g, Some("boost"), "list")).await[1].embed_description().unwrap_or("").contains("No boosts yet"));
    db.done().await;
}

/// Runs a command that asks for confirmation and clicks the confirm button.
async fn route_confirmed(t: &DbTestApp, g: &str, fi: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    t.responses.clear();
    let app = t.app.clone();
    let task = tokio::spawn(async move { bot_core::router::route_interaction(&app, fi.build()).await });
    for _ in 0..200 {
        if !t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = t.responses.calls().into_iter().find(|c| c.body["components"].is_array()).expect("confirm prompt");
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or("").to_string();
    bot_core::router::route_interaction(t, FakeInteraction::button(&yes).guild(g).user(&uid(49)).message_id("1000000000000000100").build()).await;
    task.await.expect("command");
    t.responses.calls()
}

#[tokio::test]
async fn economy_admin_coins_item_lifecycle_limits_casino_channels_and_reset() {
    let Some(db) = Db::new(36, &[49, 50]).await else { return };
    let target = uid(50);
    db.t.install(registry());
    let guild = FakeGuild::new(&db.g, "955026900000000099")
        .role("820000000000000001", 10, Permissions::MANAGE_ROLES)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role("820000000000000002", 2, Permissions::MANAGE_GUILD)
        .member(&uid(49), &["820000000000000002"])
        .member(&target, &[])
        .channel("810000000000000601", 0, "casino", &[])
        .channel("810000000000000602", 0, "games", &[]);
    db.t.set_discord(offline_discord_as(&[guild.json()]));
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true).set("economyStartingBalance", 500)).await.expect("settings");
    let with_member = |fi: FakeInteraction| resolved(fi.option("member", option_type::USER, json!(target)), &target, false, true);
    let coins = |sub: &str, n: i64| with_member(admin(&db.g, Some("coins"), sub).option("amount", option_type::INTEGER, json!(n)));

    let give = route(&db.t, coins("give", 250)).await;
    assert!(titles(&give).contains("Coins Added"));
    assert_eq!(give[1].embed_description(), Some(format!("Gave 🪙 **250** coins to <@{target}>.\nNew balance: 🪙 **750** coins").as_str()));
    assert_eq!(db.balance(&target).await, 750);
    let removed = route(&db.t, coins("remove", 5000)).await;
    assert!(titles(&removed).contains("Coins Removed"));
    assert!(removed[1].embed_description().unwrap_or("").ends_with("\n(They only had 🪙 **750** coins.)"));
    assert_eq!(db.balance(&target).await, 0);
    assert!(titles(&route(&db.t, coins("set", 400)).await).contains("Balance Set"));
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyMaxBalance", 1000)).await.expect("max");
    assert!(titles(&route(&db.t, coins("set", 2000)).await).contains("Too High"));
    assert_eq!(db.balance(&target).await, 400);

    // config limits: lowering the maximum balance clamps balances
    svc::set_balance(&db.t, &db.g, &target, 900, Some(&s_with(json!({ "economyMaxBalance": 1000 })))).await.expect("set");
    let limits = route(
        &db.t,
        admin(&db.g, Some("config"), "limits").option("max_balance", option_type::INTEGER, json!(600)).option("max_payout", option_type::INTEGER, json!(250)),
    )
    .await;
    assert!(limits[1].embed_description().unwrap_or("").contains("1 balance was lowered"));
    assert_eq!(db.balance(&target).await, 600);
    let fresh = db.t.settings().get_settings_with(&db.g, true).await.expect("settings");
    assert_eq!((fresh.economy_max_balance, fresh.economy_max_payout), (600, 250));
    assert!(titles(&route(&db.t, admin(&db.g, Some("config"), "limits").option("starting_balance", option_type::INTEGER, json!(700))).await).contains("Invalid Limits"));
    let daily = admin(&db.g, Some("config"), "daily").option("min", option_type::INTEGER, json!(90)).option("max", option_type::INTEGER, json!(10));
    assert!(titles(&route(&db.t, daily).await).contains("Invalid Range"));
    assert!(titles(&route(&db.t, admin(&db.g, Some("config"), "currency").option("name", option_type::STRING, json!("**x**"))).await).contains("Invalid Currency"));
    let currency = admin(&db.g, Some("config"), "currency").option("name", option_type::STRING, json!("gems")).option("emoji", option_type::STRING, json!("💎"));
    route(&db.t, currency).await;
    let cur = db.t.settings().get_settings_with(&db.g, true).await.expect("settings");
    assert_eq!((cur.currency_name.as_str(), cur.currency_emoji.as_str()), ("gems", "💎"));

    // casino channels: allow list, then deny mode in the same call
    let channels = |action: &str, ch: Option<&str>, mode: Option<&str>| {
        let mut fi = admin(&db.g, Some("config"), "channels").option("action", option_type::STRING, json!(action));
        if let Some(c) = ch {
            fi = fi.option("channel", option_type::CHANNEL, json!(c));
        }
        if let Some(m) = mode {
            fi = fi.option("mode", option_type::STRING, json!(m));
        }
        fi
    };
    let added = route(&db.t, channels("add", Some("810000000000000601"), None)).await;
    assert!(added[1].embed_description().unwrap_or("").contains("allowed only in: <#810000000000000601>"), "{:?}", added[1].embed_description());
    assert!(titles(&route(&db.t, channels("add", Some("810000000000000601"), None)).await).contains("Not Changed"));
    let denied = route(&db.t, channels("add", Some("810000000000000602"), Some("deny"))).await;
    assert!(denied[1].embed_description().unwrap_or("").contains("blocked in: <#810000000000000601>, <#810000000000000602>"));
    let deny = db.t.settings().get_settings_with(&db.g, true).await.expect("settings");
    assert_eq!(deny.economy_game_channel_mode, bot_db::ChannelListMode::Deny);
    assert_eq!(deny.economy_game_channel_ids, vec!["810000000000000601".to_string(), "810000000000000602".to_string()]);
    let check = svc::game_channel_check_cached(&db.t, Some(&deny), serenity::all::GuildId::new(db.g.parse().expect("id")), None, serenity::all::ChannelId::new(810000000000000601));
    assert_eq!(check, svc::GameChannelCheck::Denied { denied_ids: deny.economy_game_channel_ids.clone() });
    assert!(route(&db.t, channels("list", None, None)).await[1].embed_description().unwrap_or("").contains("blocked in"));
    route(&db.t, channels("clear", None, Some("allow"))).await;
    let cleared = db.t.settings().get_settings_with(&db.g, true).await.expect("settings");
    assert_eq!((cleared.economy_game_channel_mode, cleared.economy_game_channel_ids.len()), (bot_db::ChannelListMode::Allow, 0));
    let show = route(&db.t, admin(&db.g, Some("config"), "show")).await;
    let payout = show[1].body["embeds"][0]["fields"].as_array().into_iter().flatten().find(|f| f["name"] == "Maximum payout").cloned().unwrap_or(Value::Null);
    assert!(payout["value"].as_str().unwrap_or("").contains("250"));

    // items: create, validation, edit, give, remove, list, delete (with confirmation)
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyMaxBalance", 1_000_000_000)).await.expect("max");
    let create = |name: &str, price: i64| {
        admin(&db.g, Some("item"), "create").option("name", option_type::STRING, json!(name)).option("price", option_type::INTEGER, json!(price))
    };
    let created = route(&db.t, create("Trophy", 150).option("description", option_type::STRING, json!("Shiny")).option("stock", option_type::INTEGER, json!(3))).await;
    assert!(titles(&created).contains("Item Created"), "{}", titles(&created));
    let trophy = svc::resolve_item(&db.t, &db.g, "trophy", false).await.expect("item").expect("created");
    assert_eq!((trophy.r#type, trophy.can_buy_multiple, trophy.use_on_buy, trophy.stock, trophy.description.as_deref()), (ShopItemType::Static, true, false, Some(3), Some("Shiny")));
    assert!(titles(&route(&db.t, create("TROPHY", 1)).await).contains("Invalid Item"));
    let crown = route(&db.t, create("Crown", 1).option("type", option_type::STRING, json!("ROLE"))).await;
    assert_eq!(crown[2].embed_description(), Some("A Role item needs a role."));
    let stat = route(&db.t, create("Static", 1).option("use_on_buy", option_type::BOOLEAN, json!(true))).await;
    assert!(titles(&stat).contains("Invalid Item"));
    let edit = || admin(&db.g, Some("item"), "edit").option("item", option_type::STRING, json!(trophy.id));
    assert!(titles(&route(&db.t, edit()).await).contains("Nothing To Change"));
    let edited = route(
        &db.t,
        edit()
            .option("price", option_type::INTEGER, json!(99))
            .option("description", option_type::STRING, json!("none"))
            .option("stock", option_type::INTEGER, json!(0)),
    )
    .await;
    assert_eq!(edited[1].embed_description(), Some("**Trophy** was updated: price, description, stock."));
    let after = svc::resolve_item(&db.t, &db.g, &trophy.id, false).await.expect("item").expect("still there");
    assert_eq!((after.price, after.description, after.stock), (99, None, Some(0)));
    assert!(titles(&route(&db.t, edit().option("name", option_type::STRING, json!("trophy"))).await).contains("Item Updated"));
    let item_cmd = |sub: &str, item: &str, q: Option<i64>| {
        let mut fi = with_member(admin(&db.g, Some("item"), sub)).option("item", option_type::STRING, json!(item));
        if let Some(q) = q {
            fi = fi.option("quantity", option_type::INTEGER, json!(q));
        }
        fi
    };
    assert!(titles(&route(&db.t, item_cmd("give", &trophy.id, Some(4))).await).contains("Item Given"));
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &target, &trophy.id).await.expect("owned"), 4);
    let rm = route(&db.t, item_cmd("remove", &trophy.id, Some(9))).await;
    assert_eq!(rm[1].embed_description(), Some(format!("Removed 4× **trophy** from <@{target}>. They have 0 left.").as_str()));
    assert!(titles(&route(&db.t, item_cmd("remove", &trophy.id, None)).await).contains("Not Owned"));
    assert!(titles(&route(&db.t, item_cmd("give", "ghost", None)).await).contains("Item Not Found"));
    let list = route(&db.t, admin(&db.g, Some("item"), "list")).await;
    assert_eq!(list[1].embed_description(), Some("**trophy** · 💎 99 gems · 📦 Collectible · stock 0"));
    svc::give_item(&db.t, &db.g, &target, &trophy.id, 2).await.expect("give");
    let del = route_confirmed(&db.t, &db.g, admin(&db.g, Some("item"), "delete").option("item", option_type::STRING, json!(trophy.id))).await;
    let names = method_names(&del);
    assert_eq!(names.first(), Some(&"deferReply"));
    assert!(del[0].ephemeral(), "item delete confirms privately");
    assert!(titles(&del).contains("Item Deleted"), "{}", titles(&del));
    let deleted = del.iter().find(|c| c.embed_title().unwrap_or("").contains("Item Deleted")).expect("result");
    assert_eq!(deleted.method, "followUp");
    assert_eq!(deleted.embed_description(), Some("**trophy** was deleted (2 owned copies removed)."));
    assert!(svc::resolve_item(&db.t, &db.g, &trophy.id, false).await.expect("lookup").is_none());

    // reset: confirmation text, public result, the import switch
    let everyone = route_confirmed(&db.t, &db.g, admin(&db.g, None, "reset").option("what", option_type::STRING, json!("coins"))).await;
    assert!(everyone[0].ephemeral());
    let prompt = everyone.iter().find(|c| c.body["components"].is_array()).expect("prompt");
    assert!(prompt.embed_description().unwrap_or("").contains("Members who never used the economy here will start at"));
    assert!(titles(&everyone).contains("Economy Reset"));
    assert_eq!(db.balance(&target).await, 500);
    assert!(!db.t.settings().get_settings_with(&db.g, true).await.expect("settings").economy_import_global);
    db.done().await;
}

// ═══════════════════════ cross-language: the JS service writes, Rust reads; Rust writes, the JS reads ═══════════════════════

#[tokio::test]
async fn js_and_rust_read_each_others_economy_data() {
    let Some(db) = Db::new(40, &[60, 61]).await else { return };
    let (a, b) = (uid(60), uid(61));
    db.t.settings().ensure_guild_settings(&db.g).await.expect("row");
    let js = run_node("rust/fixtures/gen/feat-economy-js.mjs", &["seed", &db.g, &a, &b]);
    assert!(js.get("error").is_none(), "{js}");
    db.t.settings().invalidate_settings(&db.g).await;
    let settings = db.t.settings().get_settings_with(&db.g, true).await.expect("settings");
    let st = Some(settings.as_ref());
    assert_eq!(settings.timezone, "Europe/London");

    // JS writes, Rust reads: every shape reads back JSON-equal to what the JS read.
    let rust_view = async || {
        json!({
            "peekA": svc::peek_account(&db.t, &db.g, &a, st).await.expect("peek a"),
            "peekB": svc::peek_account(&db.t, &db.g, &b, st).await.expect("peek b"),
            "inventoryA": svc::get_inventory(&db.t, &db.g, &a).await.expect("inv a"),
            "inventoryB": svc::get_inventory(&db.t, &db.g, &b).await.expect("inv b"),
            "searchB": svc::search_inventory(&db.t, &db.g, &b, "e", false).await.expect("search b"),
            "items": svc::list_items(&db.t, &db.g, false, true).await.expect("items"),
            "search": svc::search_items(&db.t, &db.g, "", false, 25).await.expect("search"),
            "boosts": svc::list_boosts(&db.t, &db.g).await.expect("boosts"),
            "top": svc::top_accounts(&db.t, &db.g, 100).await.expect("top"),
            "count": svc::count_items(&db.t, &db.g).await.expect("count"),
        })
    };
    let ours = rust_view().await;
    for key in ["peekA", "peekB", "inventoryA", "inventoryB", "searchB", "items", "search", "boosts", "top", "count"] {
        assert_eq!(ours[key], js[key], "{key}");
    }
    let rank = svc::get_rank(&db.t, &db.g, js["peekA"]["balance"].as_i64().unwrap_or(0)).await.expect("rank");
    assert_eq!(json!(rank), js["rankA"]);
    assert_eq!(js["peekA"]["importedGlobal"], true);
    let account: EconomyAccount = serde_json::from_value(js["account"].clone()).expect("the JS row reads as the Rust model");
    assert_eq!(db.account(&a).await.map(|r| r.id), Some(account.id));
    // The JS claim and boost use are respected: Rust sees the daily on cooldown with the JS streak.
    let daily = svc::claim_daily(&db.t, &db.g, &a, st, &[]).await.expect("daily");
    assert!(matches!(daily, svc::DailyClaim::NotYet { streak: 1, .. }), "{daily:?}");
    let purchase: svc::Purchase = svc::Purchase {
        item: serde_json::from_value(js["purchase"]["item"].clone()).expect("item"),
        quantity: 2,
        total: 200,
        balance: 1050 - 200,
        owned: Some(2),
        to_inventory: true,
    };
    assert_eq!(serde_json::to_value(&purchase).expect("json"), js["purchase"], "the purchase result shape");

    // Rust writes, JS reads.
    let work = svc::claim_work(&db.t, &db.g, &a, st, &[]).await.expect("work");
    assert!(matches!(&work, svc::WorkClaim::Claimed { amount: 200, boost: None, .. }), "the JS boost use of today blocks this one: {work:?}");
    svc::credit(&db.t, &db.g, &b, 25, st).await.expect("credit");
    let boost_id = js["boost"]["id"].as_str().unwrap_or("").to_string();
    svc::set_boost_enabled(&db.t, &db.g, &boost_id, false).await.expect("toggle");
    let elixir = svc::resolve_item(&db.t, &db.g, "elixir", false).await.expect("lookup").expect("elixir");
    svc::purchase_item(&db.t, &db.g, &b, &elixir.id, 1, st).await.expect("buy");
    svc::create_item(&db.t, &db.g, json!({ "name": "Rust Relic", "price": 7, "type": "ROLE", "roleId": "4242", "useOnBuy": true, "canBuyMultiple": false }).as_object().cloned().unwrap_or_default())
        .await
        .expect("create");
    svc::create_boost(&db.t, &db.g, NewBoost { name: "Rust Boost".into(), percent: 5, period: BoostPeriod::Monthly, role_id: None, created_by: a.clone() })
        .await
        .expect("boost");
    svc::take_item(&db.t, &db.g, &a, &elixir.id, 1).await.expect("take");
    let back = run_node("rust/fixtures/gen/feat-economy-js.mjs", &["read", &db.g, &a, &b]);
    assert!(back.get("error").is_none(), "{back}");
    let ours = rust_view().await;
    for key in ["peekA", "peekB", "inventoryA", "inventoryB", "searchB", "items", "search", "boosts", "top", "count"] {
        assert_eq!(ours[key], back[key], "{key} after Rust wrote");
    }
    assert_eq!(back["workA"]["claimed"], false, "the JS sees the Rust /work cooldown");
    assert_eq!(back["pityB"]["tooRich"], true);
    db.done().await;
}

// ═══════════════════════ settleBet settles at most once (review-economy) ═══════════════════════

/// A closed pool fails every statement before it is sent (`PoolClosed`): the credit is retried exactly once, after
/// 250 ms, then the error is returned. A loss (payout 0) only reads, so it fails at once.
#[tokio::test]
async fn settle_bet_retries_once_only_when_the_credit_provably_never_left_the_process() {
    let t = TestApp::new();
    t.db().close().await;
    let started = std::time::Instant::now();
    let err = svc::settle_bet(&t, "g", "u", 300, None, 100).await.expect_err("no database");
    assert!(svc::failed_before_send(&err), "{err:#}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(250), "the single retry waited 250 ms");
    assert!(started.elapsed() < std::time::Duration::from_millis(2_000), "and did not retry again");
    let started = std::time::Instant::now();
    svc::settle_bet(&t, "g", "u", 0, None, 100).await.expect_err("no database");
    assert!(started.elapsed() < std::time::Duration::from_millis(250), "a loss never retries");
    assert_eq!(svc::big(-12.9), -12);
    assert_eq!(svc::big(36_000_000_000.7), 36_000_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_use_on_buy_role_buys_sent_together_are_serialised_and_never_charge_twice() {
    let Some(db) = Db::new(37, &[52]).await else { return };
    let u = uid(52);
    db.t.install(registry());
    db.t.set_discord(offline_discord_as(&[shop_guild(&db.g, &u, "820000000000004343").json()]));
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("settings");
    db.seed(&u, 1000).await;
    let vip = db
        .item(json!({ "name": "VIP Race", "price": 400, "type": "ROLE", "roleId": "820000000000004343", "useOnBuy": true, "destroyOnUse": true, "canBuyMultiple": false }))
        .await;
    let buy = || slash("shop", &db.g, &u).subcommand("buy").option("item", option_type::STRING, json!(vip.id)).build();
    let (a, b) = (db.t.app.clone(), db.t.app.clone());
    let (ia, ib) = (buy(), buy());
    let (x, y) = tokio::join!(
        tokio::spawn(async move { bot_core::router::route_interaction(&a, ia).await }),
        tokio::spawn(async move { bot_core::router::route_interaction(&b, ib).await })
    );
    x.expect("first");
    y.expect("second");
    // Offline the grants fail, so both buys are refunded; the per-member lock makes them run one after the other.
    let refunded = db.t.responses.calls().iter().filter(|c| c.embed_title().unwrap_or("").contains("Purchase Refunded")).count();
    assert_eq!(refunded, 2, "{}", titles(&db.t.responses.calls()));
    assert_eq!(db.balance(&u).await, 1000);
    assert_eq!(svc::resolve_item(&db.t, &db.g, &vip.id, false).await.expect("item").and_then(|i| i.stock), None);
    assert!(db.t.ext::<feat_economy::commands::shop::RoleBuys>().0.is_empty(), "the lock is released");
    db.done().await;
}

// ═══════════════════════ case-insensitive lookups with LIKE wildcards (audit) ═══════════════════════

/// The JS lookups pass names and queries to PostgreSQL unescaped (Prisma's `mode: 'insensitive'` is a bare `ILIKE`), so
/// `%`, `_` and `\` act as wildcards and a lone trailing backslash is an error. The Rust lookups must answer the same.
#[tokio::test]
async fn lookups_treat_like_wildcards_exactly_as_the_js() {
    let Some(db) = Db::new(41, &[62]).await else { return };
    let u = uid(62);
    let js = run_node("rust/fixtures/gen/feat-economy-search.mjs", &[&db.g, &u]);
    assert!(js.get("error").is_none(), "{js}");
    let t = &db.t;
    let g = db.g.as_str();
    let queries = ["%", "_", "50%", "50% off", "a_b", "A_B", r"\", r"back\slash", r"back\\slash", "x", "%off", "a%", "plain", ""];
    let err = || json!("ERR");
    for q in queries {
        let items = svc::search_items(t, g, q, false, 25).await.map(|r| json!(r.iter().map(|i| i.name.clone()).collect::<Vec<_>>())).unwrap_or_else(|_| err());
        assert_eq!(items, js["items"][q], "searchItems {q:?}");
        let found = svc::resolve_item(t, g, q, false).await.map(|r| json!(r.map(|i| i.name))).unwrap_or_else(|_| err());
        assert_eq!(found, js["resolve"][q], "resolveItem {q:?}");
        let taken = svc::find_item_by_name_insensitive(t, g, q, None).await.map(|r| json!(r.map(|i| i.name))).unwrap_or_else(|_| err());
        // A lone backslash is plan-dependent in PostgreSQL itself: an index plan turns the pattern into `name = ''` and
        // finds nothing, a sequential scan evaluates ILIKE and raises. Either answer is the JS answer.
        if q == r"\" {
            assert!(taken == err() || taken.is_null(), "findItemByNameInsensitive {q:?}: {taken}");
        } else {
            assert_eq!(taken, js["taken"][q], "findItemByNameInsensitive {q:?}");
        }
        let boosts = svc::search_boosts(t, g, q).await.map(|r| json!(r.iter().map(|b| b.name.clone()).collect::<Vec<_>>())).unwrap_or_else(|_| err());
        assert_eq!(boosts, js["boosts"][q], "searchBoosts {q:?}");
        let boost = svc::resolve_boost(t, g, q).await.map(|r| json!(r.map(|b| b.name))).unwrap_or_else(|_| err());
        assert_eq!(boost, js["resolveBoost"][q], "resolveBoost {q:?}");
        let inv = svc::search_inventory(t, g, &u, q, true).await.map(|r| json!(r.iter().map(|e| e.item.name.clone()).collect::<Vec<_>>())).unwrap_or_else(|_| err());
        assert_eq!(inv, js["inventory"][q], "searchInventory {q:?}");
    }
    db.done().await;
}

/// discord.js's `member.roles.cache` (what /daily and /work pass as `memberRoleIds`) holds the @everyone role and only
/// the member's roles that still exist: a boost on the @everyone role applies to everyone, and a boost on a deleted
/// role that the interaction payload still lists does not.
#[tokio::test]
async fn daily_boosts_see_the_everyone_role_and_only_existing_roles() {
    let Some(db) = Db::new(42, &[63]).await else { return };
    let u = uid(63);
    db.t.install(registry());
    db.t.set_discord(offline_discord_as(&[FakeGuild::new(&db.g, "955026900000000099").member(&u, &[]).json()]));
    db.t.settings()
        .update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true).set("dailyMin", 100).set("dailyMax", 100))
        .await
        .expect("settings");
    db.seed(&u, 1000).await;
    let everyone = NewBoost { name: "Everyone".into(), percent: 50, period: BoostPeriod::Daily, role_id: Some(db.g.clone()), created_by: u.clone() };
    svc::create_boost(&db.t, &db.g, everyone).await.expect("everyone boost");
    let ghost = NewBoost { name: "Ghost".into(), percent: 100, period: BoostPeriod::Daily, role_id: Some("820000000000009999".into()), created_by: u.clone() };
    svc::create_boost(&db.t, &db.g, ghost).await.expect("ghost boost");

    let calls = route(&db.t, slash("daily", &db.g, &u).roles(&["820000000000009999"])).await;
    assert!(titles(&calls).contains("Daily Reward Claimed"), "{}", titles(&calls));
    assert!(calls.iter().any(|c| c.embed_description().unwrap_or("").contains("Boost **Everyone** +50%")), "{calls:?}");
    assert_eq!(db.balance(&u).await, 1150, "the @everyone boost (+50%) applies, the deleted role's (+100%) does not");
    db.done().await;
}

/// `item create` checks `guild.roles.cache.get(picked.id) ?? picked`: a role the interaction resolved but the cache does
/// not hold yet is judged from the resolved role, not refused as "Role not found."
#[tokio::test]
async fn item_create_judges_an_uncached_role_from_the_interaction() {
    let Some(db) = Db::new(43, &[49]).await else { return };
    db.t.install(registry());
    let guild = FakeGuild::new(&db.g, "955026900000000099")
        .role("820000000000000001", 10, Permissions::MANAGE_ROLES)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role("820000000000000002", 2, Permissions::MANAGE_GUILD)
        .member(&uid(49), &["820000000000000002"]);
    db.t.set_discord(offline_discord_as(&[guild.json()]));
    let fresh_role = "820000000000007777"; // not in the cached guild
    let fi = admin(&db.g, Some("item"), "create")
        .option("name", option_type::STRING, json!("Fresh Role"))
        .option("price", option_type::INTEGER, json!(10))
        .option("type", option_type::STRING, json!("ROLE"))
        .option("role", option_type::ROLE, json!(fresh_role))
        .resolve_roles_by_default();
    let created = route(&db.t, fi).await;
    assert!(titles(&created).contains("Item Created"), "{}", titles(&created));
    let item = svc::resolve_item(&db.t, &db.g, "fresh role", false).await.expect("lookup").expect("created");
    assert_eq!(item.role_id.as_deref(), Some(fresh_role));
    db.done().await;
}

/// The role-grant success paths (a REST double answers Discord): a use-on-buy role item is bought and its role
/// granted with the JS audit reason, and /use on a role item grants the role and consumes one copy.
#[tokio::test]
async fn role_items_grant_their_role_when_discord_accepts() {
    let Some(db) = Db::new(44, &[64]).await else { return };
    let u = uid(64);
    db.t.install(registry());
    let rest = FakeRest::new();
    db.t.set_discord(rest.discord(&[shop_guild(&db.g, &u, "820000000000004444").role("820000000000004445", 2, Permissions::empty()).json()]));
    db.t.settings().update_settings(&db.g, SettingsPatch::new().set("economyEnabled", true)).await.expect("settings");
    db.seed(&u, 1000).await;
    let vip = db
        .item(json!({ "name": "VIP Role", "price": 400, "type": "ROLE", "roleId": "820000000000004444", "useOnBuy": true, "destroyOnUse": true, "canBuyMultiple": false }))
        .await;
    let bought = route(&db.t, slash("shop", &db.g, &u).subcommand("buy").option("item", option_type::STRING, json!(vip.id))).await;
    assert!(titles(&bought).contains("Purchase Complete"), "{}", titles(&bought));
    assert_eq!(db.balance(&u).await, 600);
    let grants = rest.calls_to("PUT", &format!("/guilds/{}/members/{u}/roles/820000000000004444", db.g));
    assert_eq!(grants.len(), 1, "{:?}", rest.calls_to("PUT", "*"));
    assert_eq!(grants[0].reason.as_deref(), Some("Bought shop item: VIP Role"));
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &vip.id).await.expect("owned"), 0, "used on buy and destroyed");

    let badge = db.item(json!({ "name": "Badge Role", "price": 5, "type": "ROLE", "roleId": "820000000000004445", "useOnBuy": false, "destroyOnUse": true })).await;
    svc::give_item(&db.t, &db.g, &u, &badge.id, 2).await.expect("give");
    let used = route(&db.t, slash("use", &db.g, &u).option("item", option_type::STRING, json!(badge.id))).await;
    assert!(titles(&used).contains("Item Used"), "{}", titles(&used));
    let grants = rest.calls_to("PUT", &format!("/guilds/{}/members/{u}/roles/820000000000004445", db.g));
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].reason.as_deref(), Some("Used item: Badge Role"));
    assert_eq!(svc::get_owned_quantity(&db.t, &db.g, &u, &badge.id).await.expect("owned"), 1);
    db.done().await;
}
