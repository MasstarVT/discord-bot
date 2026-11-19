//! Port of the offline parts of `test/moderation.test.js` and `test/moderation-commands.test.js`: the pure helpers
//! (each also swept against the JS through `rust/fixtures/gen/feat-moderation-js.mjs`), preflight and immunity, the ban
//! markers, the command payloads, the plugin declaration and the commands' early refusals through the router.

use bot_core::prelude::*;
use bot_core::testing::{FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, run_node};
use bot_db::{Infraction, InfractionType};
use feat_moderation::commands::{modconfig, purge, slowmode};
use feat_moderation::guild_ops::{DiscordError, GuildView, MemberView, RoleView, UserRef};
use feat_moderation::moderation_service::{self as svc, ModCode, Preflight, PriorEscalation, msg};
use serde_json::{Value, json};
use serenity::all::Permissions as P;

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_moderation::register(&mut reg);
    reg
}

fn js_pure(input: Value) -> Value {
    let out = run_node("rust/fixtures/gen/feat-moderation-js.mjs", &["pure", &input.to_string()]);
    assert!(out.get("error").is_none(), "JS failed: {out}");
    out
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &feat_moderation::COMMANDS);
    bot_core::testing::assert_plugin_matches_golden(&reg, "moderation");
}

fn char_count(node: &Value) -> usize {
    let len = |v: &Value| v.as_str().map(|s| s.encode_utf16().count()).unwrap_or(0);
    let mut n = len(&node["name"]) + len(&node["description"]);
    for c in node["choices"].as_array().into_iter().flatten() {
        n += len(&c["name"]) + c["value"].to_string().trim_matches('"').len();
    }
    for o in node["options"].as_array().into_iter().flatten() {
        n += char_count(o);
    }
    n
}

#[test]
fn modconfig_payload() {
    let json = modconfig::command().payload();
    assert_eq!(json["name"], "modconfig");
    assert!(char_count(&json) < 4000);
    let names: Vec<&str> = json["options"].as_array().map(|a| a.iter().filter_map(|o| o["name"].as_str()).collect()).unwrap_or_default();
    assert_eq!(names, vec!["escalation", "immune", "modroles", "muterole", "logchannel", "visibility", "dm", "show"]);
    let meta = modconfig::command().metadata();
    assert_eq!(meta["gated"], false);
    assert_eq!(meta["permissionLevel"], 2);
    assert_eq!(meta["module"], "moderation");
    assert_eq!(meta["hasAutocomplete"], true);
    assert_eq!(json["default_member_permissions"], P::MANAGE_GUILD.bits().to_string());
    let remove = &json["options"][0]["options"][1]["options"][0];
    assert_eq!(remove["autocomplete"], true);
    assert_eq!(json["options"][0]["options"][0]["options"].as_array().map(Vec::len), Some(4));
    let case = feat_moderation::commands::case::command().metadata();
    assert_eq!(case["subcommandPermissions"], json!({ "clear-history": 2 }));
}

#[test]
fn plugin_declaration() {
    let reg = registry();
    let p = reg.plugin_named("moderation").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Moderation));
    let mut hooks: Vec<String> = p.hooks.iter().map(|h| format!("{}:{}", h.event().as_str(), h.id())).collect();
    hooks.sort();
    assert_eq!(hooks, vec!["guildBanAdd:moderation", "guildBanRemove:moderation", "guildMemberAdd:moderation", "guildReady:moderation"]);
    let manifest = bot_core::testing::plugin_manifest(p);
    assert_eq!(manifest["jobs"], json!(["mod.unban", "mod.expire"]));
    assert_eq!(manifest["components"], json!([]));
    assert_eq!(bot_config::hook_index("guildMemberAdd", "moderation"), bot_config::hook_index("guildMemberAdd", "invites").map(|i| i + 1), "right after invites");
}

/// The JS static scan: no console, no customId literal, no deleteMessageDays, no bare now() in raw SQL, the case
/// aggregate only inside the allocation functions.
#[test]
fn static_scan_of_the_crate() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![dir.join("moderation_service.rs"), dir.join("plugin.rs"), dir.join("guild_ops.rs")];
    for f in std::fs::read_dir(dir.join("commands")).expect("commands dir") {
        files.push(f.expect("entry").path());
    }
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        for bad in ["println!", "eprintln!", "dbg!", "deleteMessageDays", "unwrap()", "create_response", "todo!", "unimplemented!"] {
            assert!(!text.contains(bad), "{}: {bad}", f.display());
        }
        for (i, line) in text.lines().enumerate() {
            let l = line.to_lowercase();
            if l.contains("now()") && ["select ", "update ", "insert ", "where "].iter().any(|k| l.contains(k)) {
                assert!(l.contains("now() at time zone 'utc'") || l.contains("now_ms") || l.contains("clock().now"), "{}:{}: bare now()", f.display(), i + 1);
            }
        }
    }
    let service = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/moderation_service.rs")).unwrap_or_default();
    assert!(service.contains("pg_advisory_xact_lock"));
    assert_eq!(service.matches(r#"MAX("caseId")"#).count(), 1, "the case aggregate lives in max_case_id only");
}

// ── 1. resolveEffectiveAction ─────────────────────────────────────────────────

fn eff_json(r: Result<svc::EffectiveAction, svc::ModError>) -> Value {
    match r {
        Ok(e) => json!({ "ok": true, "action": e.action, "durationMs": e.duration_ms, "note": e.note }),
        Err(e) => json!({ "ok": false, "code": e.code.as_str(), "error": e.error }),
    }
}

#[test]
fn resolve_effective_action_matches_the_js_test() {
    let r = |a: &str, d: Option<f64>, m: Option<&str>| svc::resolve_effective_action(a, d, m);
    assert_eq!(eff_json(r("TIMEOUT", Some(600_000.0), None)), json!({ "ok": true, "action": "TIMEOUT", "durationMs": 600_000, "note": null }));
    let long = r("TIMEOUT", Some((30 * DAY) as f64), Some("1")).expect("ok");
    assert_eq!(long.action, "MUTE");
    assert!(long.note.unwrap_or("").contains("mute role was used"));
    assert_eq!(r("TIMEOUT", Some((30 * DAY) as f64), None).map_err(|e| e.code), Err(ModCode::Invalid));
    assert!(r("TIMEOUT", None, None).is_err());
    assert!(r("TIMEOUT", Some(30_000.0), None).is_err());
    assert_eq!(r("TIMEOUT", Some(-5.0), None).err().map(|e| e.error), Some("Invalid duration.".into()));
    assert_eq!(r("TIMEOUT", Some(f64::NAN), None).err().map(|e| e.error), Some("Invalid duration.".into()));
    assert_eq!(r("TIMEOUT", Some((400 * DAY) as f64), Some("1")).err().map(|e| e.error), Some("Mutes can last at most 1 year.".into()));
    let mute1d = r("MUTE", Some(DAY as f64), None).expect("ok");
    assert_eq!(mute1d.action, "TIMEOUT");
    assert!(mute1d.note.unwrap_or("").contains("No mute role is set"));
    assert!(r("MUTE", None, None).is_err());
    assert!(r("MUTE", Some((40 * DAY) as f64), None).is_err());
    assert_eq!(eff_json(r("MUTE", None, Some("1"))), json!({ "ok": true, "action": "MUTE", "durationMs": null, "note": null }));
    assert_eq!(r("BAN", Some(DAY as f64), None).map(|e| e.action), Ok("TEMPBAN"));
    assert_eq!(r("BAN", None, None).map(|e| e.action), Ok("BAN"));
    assert!(r("TEMPBAN", Some((730 * DAY) as f64), None).is_err());
    assert!(r("TEMPBAN", None, None).is_err());
    assert_eq!(r("TEMPBAN", Some(DAY as f64), None).map(|e| e.duration_ms), Ok(Some(DAY)));
    for a in ["KICK", "WARN", "SOFTBAN"] {
        assert_eq!(r(a, Some(DAY as f64), None).map(|e| e.duration_ms), Ok(None));
    }
    assert_eq!(r("FOO", None, None).map_err(|e| e.code), Err(ModCode::Invalid));
}

#[test]
fn resolve_effective_action_agrees_with_the_js_on_a_sweep() {
    let actions = ["WARN", "TIMEOUT", "MUTE", "KICK", "BAN", "TEMPBAN", "SOFTBAN", "FOO"];
    let durations: [Option<f64>; 9] = [None, Some(1.0), Some(59_999.0), Some(60_000.0), Some(1.5 * DAY as f64), Some((28 * DAY) as f64), Some((28 * DAY + 1) as f64), Some((365 * DAY) as f64), Some((366 * DAY) as f64)];
    let roles = [None, Some("1")];
    let mut cases = Vec::new();
    let mut input = Vec::new();
    for a in actions {
        for d in durations {
            for m in roles {
                cases.push((a, d, m));
                input.push(json!({ "action": a, "durationMs": d, "muteRoleId": m }));
            }
        }
    }
    let js = js_pure(json!({ "effective": input }));
    let js = js["effective"].as_array().cloned().unwrap_or_default();
    assert_eq!(js.len(), cases.len());
    for ((a, d, m), expected) in cases.iter().zip(js) {
        assert_eq!(eff_json(svc::resolve_effective_action(a, *d, *m)), expected, "{a} {d:?} {m:?}");
    }
}

// ── 2-4. Escalation tiers ─────────────────────────────────────────────────────

fn n(v: Value) -> Vec<svc::Tier> {
    svc::normalize_escalation_tiers(Some(&v))
}

#[test]
fn normalize_escalation_tiers_matches_the_js_test() {
    let legacy = n(json!([{ "warnCount": 3, "action": "TIMEOUT", "duration": 600 }, { "warnCount": 5, "action": "BAN", "duration": 86400 }]));
    type Shape = (i64, i64, Option<i64>, String, Option<i64>);
    let shape: Vec<Shape> =
        legacy.iter().map(|t| (t.id, t.warn_count, t.within_seconds, t.action.clone(), t.duration_seconds)).collect();
    assert_eq!(shape, vec![(1, 3, None, "TIMEOUT".into(), Some(600)), (2, 5, None, "TEMPBAN".into(), Some(86400))]);
    assert!(legacy.iter().all(|t| t.count_from.as_deref() == Some(svc::LEGACY_ESCALATION_SINCE)));
    assert_eq!(n(json!([{ "warnCount": 3, "action": "BAN" }]))[0].action, "BAN");
    assert!(n(json!([{ "warnCount": 3, "action": "TIMEOUT" }, { "warnCount": 0, "action": "KICK" }, { "warnCount": 101, "action": "KICK" }, { "warnCount": 2, "action": "EXPLODE" }, null, "x", 5, []])).is_empty());
    assert_eq!(n(json!([{ "warnCount": 2, "action": "timeout", "duration": 5_000_000 }]))[0].duration_seconds, Some(2_419_200));
    assert_eq!(n(json!([{ "warnCount": 2, "withinSeconds": null, "action": "KICK", "durationSeconds": 99 }]))[0].duration_seconds, None);
    assert_eq!(n(json!([{ "warnCount": 2, "withinSeconds": null, "action": "TEMPBAN", "durationSeconds": 5 }]))[0].duration_seconds, Some(60));
    let dup = n(json!([
        { "id": 1, "warnCount": 2, "withinSeconds": 3600, "action": "KICK", "durationSeconds": null },
        { "id": 2, "warnCount": 2, "withinSeconds": 3600, "action": "BAN", "durationSeconds": null }
    ]));
    assert_eq!(dup.len(), 1);
    assert_eq!(dup[0].action, "BAN", "the last duplicate wins");
    let sorted = n(json!([
        { "id": 1, "warnCount": 5, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
        { "id": 2, "warnCount": 2, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
        { "id": 3, "warnCount": 2, "withinSeconds": 86400, "action": "KICK", "durationSeconds": null },
        { "id": 4, "warnCount": 2, "withinSeconds": 3600, "action": "KICK", "durationSeconds": null }
    ]));
    assert_eq!(sorted.iter().map(|t| t.id).collect::<Vec<_>>(), vec![4, 3, 2, 1]);
    let many: Vec<Value> = (1..=12).map(|i| json!({ "warnCount": i, "withinSeconds": null, "action": "KICK", "durationSeconds": null })).collect();
    assert_eq!(n(json!(many)).len(), bot_config::limits::ESCALATION_TIERS);
    let kept = n(json!([
        { "id": 7, "warnCount": 2, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
        { "id": 7, "warnCount": 3, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
        { "warnCount": 4, "withinSeconds": null, "action": "KICK", "durationSeconds": null }
    ]));
    assert_eq!(kept.iter().map(|t| t.id).collect::<Vec<_>>(), vec![7, 8, 9]);
    for bad in [json!(null), json!({}), json!("x"), json!(42)] {
        assert!(n(bad).is_empty());
    }
    assert!(svc::normalize_escalation_tiers(None).is_empty());
    let both = n(json!([
        { "id": 1, "warnCount": 2, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": "2026-01-02T00:00:00.000Z" },
        { "id": 2, "warnCount": 3, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": "not a date" }
    ]));
    assert_eq!(both[0].count_from.as_deref(), Some("2026-01-02T00:00:00.000Z"));
    assert_eq!(both[1].count_from, None);
    assert_eq!(n(json!([{ "warnCount": 3, "action": "KICK", "countFrom": "2026-05-05T05:05:05.000Z" }]))[0].count_from.as_deref(), Some("2026-05-05T05:05:05.000Z"));
}

#[test]
fn normalize_and_describe_agree_with_the_js_on_a_sweep() {
    let raws = vec![
        json!([{ "warnCount": 3, "action": "TIMEOUT", "duration": 600 }, { "warnCount": 5, "action": "BAN", "duration": 86400 }]),
        json!([{ "warnCount": 3, "action": "mute" }, { "warnCount": "4", "action": "KICK" }, { "warnCount": 2.0, "action": "kick" }]),
        json!([{ "id": 3, "warnCount": 2, "withinSeconds": "3600", "action": "TEMPBAN", "durationSeconds": "86400" }]),
        json!([{ "id": 3, "warnCount": 2, "withinSeconds": 59, "action": "TIMEOUT", "durationSeconds": 3_000_000 }, { "id": 3, "warnCount": 4, "withinSeconds": 31_536_001, "action": "MUTE", "durationSeconds": 30 }]),
        json!([{ "id": -1, "warnCount": 1, "withinSeconds": 120.5, "action": "BAN", "durationSeconds": 99, "countFrom": "2026-03-01T00:00:00.000Z" }, { "id": 2.5, "warnCount": 1, "withinSeconds": 120, "action": "KICK" }]),
        json!([{ "warnCount": 9, "action": "TEMPBAN", "durationSeconds": 0 }, { "warnCount": 9, "action": "TEMPBAN", "durationSeconds": true }, { "warnCount": 8, "action": "TIMEOUT", "duration": "" }]),
        json!((1..=14usize)
            .map(|i| {
                let action = ["TIMEOUT", "MUTE", "KICK", "TEMPBAN", "BAN"][i % 5];
                let within = if i % 2 == 0 { json!(i * 3600) } else { json!(null) };
                json!({ "id": 15 - i, "warnCount": 1 + (i % 4), "withinSeconds": within, "action": action, "durationSeconds": i * 600 })
            })
            .collect::<Vec<_>>()),
        json!(null),
        json!("x"),
    ];
    let js = js_pure(json!({ "tiers": raws }));
    let js_tiers = js["tiers"].as_array().cloned().unwrap_or_default();
    assert_eq!(js_tiers.len(), raws.len());
    let mut all = Vec::new();
    for (raw, expected) in raws.iter().zip(js_tiers) {
        let ours = svc::normalize_escalation_tiers(Some(raw));
        assert_eq!(serde_json::to_value(&ours).unwrap_or(Value::Null), expected, "{raw}");
        all.extend(ours);
    }
    all.push(svc::Tier { id: 99, warn_count: 100, within_seconds: Some(31_535_999), action: "TEMPBAN".into(), duration_seconds: Some(31_535_999), count_from: None });
    let js = js_pure(json!({ "describe": all }));
    let ours: Vec<Value> = all.iter().map(|t| json!(svc::describe_tier(t))).collect();
    assert_eq!(json!(ours), js["describe"]);
    assert_eq!(
        svc::describe_tier(&svc::Tier { id: 3, warn_count: 5, within_seconds: Some(604_800), action: "TEMPBAN".into(), duration_seconds: Some(259_200), count_from: None }),
        "T3: 5 warnings within 1 week → Temporary ban for 3 days"
    );
    assert!(all.iter().all(|t| svc::describe_tier(t).chars().count() <= 100));
}

const NOW: i64 = 1_000_000_000_000;

fn tiers3() -> Vec<svc::Tier> {
    n(json!([
        { "id": 1, "warnCount": 2, "withinSeconds": 86400, "action": "TIMEOUT", "durationSeconds": 86400, "countFrom": null },
        { "id": 2, "warnCount": 5, "withinSeconds": 604800, "action": "TEMPBAN", "durationSeconds": 259200, "countFrom": null },
        { "id": 3, "warnCount": 10, "withinSeconds": 1_209_600, "action": "BAN", "durationSeconds": null, "countFrom": null }
    ]))
}

#[test]
fn select_escalation_tier_matches_the_js_test() {
    let tiers = tiers3();
    let ago = |ms: i64| NOW - ms;
    let pick = |warns: &[i64], prior: &[PriorEscalation]| svc::select_escalation_tier(&tiers, warns, prior, NOW).map(|(t, _)| t.id);
    assert_eq!(pick(&[ago(1000), ago(HOUR)], &[]), Some(1));
    assert_eq!(pick(&[ago(1000), ago(HOUR)], &[PriorEscalation { tier_id: 1, at: NOW - 1000 }]), None);
    let five = [ago(3 * DAY), ago(2 * DAY), ago(DAY + HOUR), ago(2 * HOUR), ago(HOUR)];
    assert_eq!(pick(&five, &[]), Some(2));
    assert_eq!(pick(&five, &[PriorEscalation { tier_id: 2, at: NOW - DAY }]), Some(1));
    let ten: Vec<i64> = (1..=10).map(|i| ago(i * DAY)).collect();
    assert_eq!(pick(&ten, &[]), Some(3));
    assert_eq!(pick(&[ago(1000)], &[]), None);
    assert_eq!(pick(&[], &[]), None);
    assert_eq!(svc::select_escalation_tier(&tiers, &[ago(1), ago(2)], &[], NOW).map(|(_, c)| c), Some(2));

    let open = n(json!([{ "id": 1, "warnCount": 2, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": null }]));
    let warns = [NOW - 5 * DAY, NOW - 4 * DAY];
    assert_eq!(svc::select_escalation_tier(&open, &warns, &[PriorEscalation { tier_id: 1, at: NOW - 10 * DAY }], NOW).map(|(t, _)| t.id), Some(1));
    assert!(svc::select_escalation_tier(&open, &warns, &[PriorEscalation { tier_id: 1, at: NOW - 9 * DAY / 2 }], NOW).is_none());

    let count_from = bot_db::format_js_date(&bot_db::serde_js::from_epoch_ms(NOW - HOUR).expect("date"));
    let since = n(json!([{ "id": 1, "warnCount": 2, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": count_from }]));
    let old5: Vec<i64> = (0..5).map(|i| NOW - (2 + i) * HOUR).collect();
    let mut one_new = old5.clone();
    one_new.push(NOW - 1000);
    assert!(svc::select_escalation_tier(&since, &one_new, &[], NOW).is_none());
    let mut two_new = one_new.clone();
    two_new.push(NOW - 2000);
    assert_eq!(svc::select_escalation_tier(&since, &two_new, &[], NOW).map(|(t, _)| t.id), Some(1));
    let windowed = n(json!([{ "id": 1, "warnCount": 2, "withinSeconds": 86400, "action": "KICK", "durationSeconds": null, "countFrom": count_from }]));
    assert!(svc::select_escalation_tier(&windowed, &one_new, &[], NOW).is_none());
}

#[test]
fn select_escalation_tier_agrees_with_the_js_on_a_sweep() {
    let tier_sets = vec![
        tiers3(),
        n(json!([
            { "id": 1, "warnCount": 2, "withinSeconds": null, "action": "MUTE", "durationSeconds": null, "countFrom": null },
            { "id": 2, "warnCount": 2, "withinSeconds": 3600, "action": "MUTE", "durationSeconds": 7200, "countFrom": null },
            { "id": 3, "warnCount": 3, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": "2001-09-09T00:00:00.000Z" },
            { "id": 4, "warnCount": 1, "withinSeconds": 60, "action": "TIMEOUT", "durationSeconds": 60, "countFrom": null }
        ])),
    ];
    let warn_sets: Vec<Vec<i64>> = vec![
        vec![],
        vec![NOW - 1000],
        vec![NOW - 1000, NOW - HOUR],
        vec![NOW - 30_000, NOW - 2 * DAY, NOW - 3 * DAY],
        (1..=12).map(|i| NOW - i * DAY / 2).collect(),
        vec![NOW - 59_000, NOW - 61_000, NOW - 5 * HOUR, NOW - 6 * HOUR],
    ];
    let priors: Vec<Vec<(i64, i64)>> = vec![vec![], vec![(1, NOW - 1000)], vec![(2, NOW - DAY), (4, NOW - 120_000)], vec![(3, NOW - 13 * DAY), (1, NOW - 3 * DAY)]];
    let mut input = Vec::new();
    let mut cases = Vec::new();
    for tiers in &tier_sets {
        for warns in &warn_sets {
            for prior in &priors {
                input.push(json!({
                    "tiers": tiers, "warnTimes": warns, "now": NOW,
                    "prior": prior.iter().map(|(t, at)| json!({ "tierId": t, "at": at })).collect::<Vec<_>>()
                }));
                cases.push((tiers.clone(), warns.clone(), prior.iter().map(|(t, at)| PriorEscalation { tier_id: *t, at: *at }).collect::<Vec<_>>()));
            }
        }
    }
    let js = js_pure(json!({ "select": input }));
    let js = js["select"].as_array().cloned().unwrap_or_default();
    assert_eq!(js.len(), cases.len());
    for ((tiers, warns, prior), expected) in cases.iter().zip(js) {
        let ours = svc::select_escalation_tier(tiers, warns, prior, NOW).map(|(t, c)| json!({ "tier": t, "count": c })).unwrap_or(Value::Null);
        assert_eq!(ours, expected, "{warns:?} {prior:?}");
    }
}

// ── 5-6. Preflight and immunity ───────────────────────────────────────────────

const BOT: &str = "100000000000000001";
const OWNER: &str = "100000000000000002";
const ALICE: &str = "100000000000000003";
const BOB: &str = "100000000000000004";

fn member(id: &str, highest: usize, perms: P, roles: &[&str]) -> MemberView {
    MemberView {
        user: UserRef { id: id.into(), username: Some(format!("user{}", &id[id.len() - 2..])), tag: None, avatar_url: None, bot: false },
        role_ids: roles.iter().map(|r| r.to_string()).collect(),
        permissions: perms,
        highest,
        manageable: true,
        kickable: true,
        bannable: true,
        moderatable: true,
        communication_disabled_until: None,
    }
}

fn guild_view() -> GuildView {
    GuildView {
        id: "9550030000000000".into(),
        name: "Test Guild".into(),
        owner_id: OWNER.into(),
        bot_id: BOT.into(),
        available: true,
        me: Some(member(BOT, 100, P::BAN_MEMBERS | P::KICK_MEMBERS | P::MODERATE_MEMBERS | P::MANAGE_ROLES | P::VIEW_AUDIT_LOG, &[])),
        roles: Default::default(),
    }
}

fn role(id: &str, position: usize) -> RoleView {
    RoleView { id: id.into(), name: "r".into(), position, permissions: P::empty(), managed: false }
}

#[test]
fn preflight_matches_the_js_test() {
    let guild = guild_view();
    let me = guild.me.clone().expect("me");
    let alice = member(ALICE, 5, P::empty(), &[]);
    let exec_mod = member(BOB, 10, P::KICK_MEMBERS, &[]);
    let exec_owner = member(OWNER, 1, P::empty(), &[]);
    let base = |action: &'static str| Preflight {
        action,
        guild: &guild,
        me: Some(&me),
        member: Some(&alice),
        target_id: ALICE,
        executor: Some(&exec_mod),
        settings: None,
        source: "command",
        mute_role: None,
        bot_owner_id: None,
    };
    let code = |p: Preflight<'_>| svc::preflight_mod_action(&p).map(|e| (e.code, e.error));
    assert_eq!(code(Preflight { target_id: BOT, ..base("KICK") }), Some((ModCode::Hierarchy, msg::BOT_SELF.into())));
    assert_eq!(code(Preflight { target_id: OWNER, ..base("KICK") }), Some((ModCode::Hierarchy, msg::OWNER.into())));
    assert_eq!(code(Preflight { executor: Some(&alice), ..base("KICK") }), Some((ModCode::Invalid, msg::SELF.into())));
    assert_eq!(code(Preflight { member: None, ..base("KICK") }).map(|c| c.0), Some(ModCode::NotInGuild));
    assert_eq!(code(Preflight { member: None, ..base("WARN") }).map(|c| c.0), Some(ModCode::NotInGuild));
    assert_eq!(code(Preflight { member: None, ..base("BAN") }), None, "a ban by id needs no member");
    let no_ban = MemberView { permissions: P::KICK_MEMBERS, ..me.clone() };
    let e = code(Preflight { me: Some(&no_ban), ..base("BAN") }).expect("refused");
    assert_eq!(e.0, ModCode::NotPermitted);
    assert_eq!(e.1, "I need the **Ban Members** permission to do that.");

    let mut settings = settings_row();
    settings.immune_role_ids = vec!["900000000000000001".into()];
    let immune = member(ALICE, 5, P::empty(), &["900000000000000001"]);
    assert_eq!(code(Preflight { member: Some(&immune), settings: Some(&settings), source: "automod", executor: None, ..base("KICK") }).map(|c| c.0), Some(ModCode::Immune));
    assert_eq!(code(Preflight { member: Some(&immune), settings: Some(&settings), ..base("KICK") }).map(|c| c.1), Some(msg::IMMUNE_ROLE.into()));
    assert_eq!(code(Preflight { member: Some(&immune), settings: Some(&settings), executor: Some(&exec_owner), ..base("KICK") }), None);

    let admin = member(ALICE, 5, P::ADMINISTRATOR, &[]);
    assert_eq!(code(Preflight { member: Some(&admin), ..base("KICK") }).map(|c| c.1), Some(msg::IMMUNE_ADMIN.into()));
    let admin_exec = member(BOB, 10, P::ADMINISTRATOR, &[]);
    assert_eq!(code(Preflight { member: Some(&admin), executor: Some(&admin_exec), ..base("KICK") }).map(|c| c.1), Some(msg::IMMUNE_ADMIN.into()));
    assert_eq!(code(Preflight { member: Some(&admin), bot_owner_id: Some(BOB), ..base("KICK") }), None, "BOT_OWNER_ID -> admin is allowed");
    assert_eq!(code(Preflight { member: Some(&admin), executor: Some(&exec_owner), ..base("TIMEOUT") }).map(|c| c.1), Some(msg::ADMIN_TIMEOUT.into()));
    let not_kickable = MemberView { kickable: false, ..alice.clone() };
    assert_eq!(code(Preflight { member: Some(&not_kickable), ..base("KICK") }).map(|c| c.1), Some(msg::BOT_ROLE.into()));
    let not_mod = MemberView { moderatable: false, ..alice.clone() };
    assert_eq!(code(Preflight { member: Some(&not_mod), ..base("TIMEOUT") }).map(|c| c.1), Some(msg::BOT_ROLE.into()));
    let not_ban = MemberView { bannable: false, ..alice.clone() };
    assert_eq!(code(Preflight { member: Some(&not_ban), ..base("BAN") }).map(|c| c.1), Some(msg::BOT_ROLE.into()));
    let equal = member(BOB, 5, P::KICK_MEMBERS, &[]);
    assert_eq!(code(Preflight { executor: Some(&equal), ..base("KICK") }).map(|c| c.1), Some(msg::EXEC_ROLE.into()), "equal roles are refused");
    let lower = member(BOB, 3, P::KICK_MEMBERS, &[]);
    assert_eq!(code(Preflight { executor: Some(&lower), ..base("KICK") }).map(|c| c.1), Some(msg::EXEC_ROLE.into()));
    assert_eq!(code(Preflight { executor: Some(&exec_owner), ..base("KICK") }), None);
    let high = MemberView { highest: 150, manageable: false, kickable: false, ..alice.clone() };
    let high_exec = member(BOB, 200, P::MODERATE_MEMBERS, &[]);
    assert_eq!(code(Preflight { member: Some(&high), executor: Some(&high_exec), ..base("WARN") }), None, "WARN has no bot-side role check");

    assert_eq!(code(Preflight { mute_role: None, ..base("MUTE") }).map(|c| c.1), Some(msg::MUTE_ROLE_GONE.into()));
    let top = role("900000000000000002", 150);
    let refused = code(Preflight { mute_role: Some(&top), ..base("MUTE") }).map(|c| c.1).unwrap_or_default();
    assert!(refused.starts_with("Mute role problem: ") && refused.contains("above my highest role"), "{refused}");
    let ok_role = role("900000000000000003", 5);
    assert_eq!(code(Preflight { mute_role: Some(&ok_role), ..base("MUTE") }), None);

    assert_eq!(svc::assert_hierarchy(&guild, Some(&alice), Some(&exec_mod)), Ok(()));
    assert_eq!(svc::assert_hierarchy(&guild, Some(&not_ban), Some(&exec_mod)), Err(msg::BOT_ROLE.into()));
    assert_eq!(svc::assert_hierarchy(&guild, None, None), Ok(()));
}

#[test]
fn is_immune_matches_the_js_test() {
    let mut settings = settings_row();
    settings.immune_role_ids = vec!["900000000000000009".into()];
    let s = Some(&settings);
    assert!(svc::is_immune(Some(&member(OWNER, 1, P::empty(), &[])), OWNER, s, None), "owner");
    assert!(svc::is_immune(Some(&member(ALICE, 1, P::ADMINISTRATOR, &[])), OWNER, s, None), "administrator");
    assert!(svc::is_immune(Some(&member(ALICE, 1, P::empty(), &["900000000000000009"])), OWNER, s, None), "immune role");
    assert!(!svc::is_immune(Some(&member(ALICE, 1, P::empty(), &[])), OWNER, s, None), "plain member");
    assert!(!svc::is_immune(None, OWNER, s, None));
    assert!(!svc::is_immune(Some(&member(ALICE, 1, P::empty(), &[])), OWNER, None, None));
}

// ── 7-12. Errors, audit reasons, the case embed, constants, markers ──────────

#[test]
fn map_discord_error_and_audit_reasons_agree_with_the_js() {
    let ys = "y".repeat(300);
    let errors: Vec<(Option<u64>, &str)> = vec![(Some(50013), "x"), (Some(50001), "x"), (Some(10007), "x"), (Some(10026), "x"), (Some(10011), "x"), (Some(10013), "x"), (Some(50035), "bad"), (None, "x"), (Some(500), ys.as_str())];
    let js = js_pure(json!({ "mapErr": errors.iter().map(|(c, m)| json!({ "code": c, "message": m })).collect::<Vec<_>>() }));
    for ((c, m), expected) in errors.iter().zip(js["mapErr"].as_array().cloned().unwrap_or_default()) {
        let e = svc::map_discord_error(Some(&DiscordError::new(*c, *m)));
        assert_eq!(json!({ "code": e.code.as_str(), "error": e.error }), expected);
    }
    assert_eq!(svc::map_discord_error(None).code, ModCode::DiscordError);

    let long = "x".repeat(600);
    let audits = vec![
        (long.as_str(), json!({ "username": "mod" }), "command"),
        ("spam", json!({ "username": "mod" }), "command"),
        ("spam", json!({ "id": "1" }), "automod"),
        ("[Automod] spam", json!({ "id": "1" }), "automod"),
        ("Captcha: failed", json!({ "id": "1" }), "captcha"),
        ("[Escalation T1] Reached 2", json!({ "id": "1" }), "escalation"),
        ("r", json!({ "id": "1" }), "captcha"),
        ("r", json!({ "tag": "t#1", "id": "1" }), "dashboard"),
        ("raid", json!({ "id": "1" }), "antiraid"),
        ("x", json!({}), "weird"),
    ];
    let js = js_pure(json!({ "audit": audits.iter().map(|(r, m, s)| json!({ "reason": r, "moderator": m, "source": s })).collect::<Vec<_>>() }));
    for ((r, m, s), expected) in audits.iter().zip(js["audit"].as_array().cloned().unwrap_or_default()) {
        let moderator = UserRef {
            id: m["id"].as_str().unwrap_or("").into(),
            username: m["username"].as_str().map(String::from),
            tag: m["tag"].as_str().map(String::from),
            ..Default::default()
        };
        assert_eq!(json!(svc::build_audit_reason(r, &moderator, s)), expected, "{s}");
    }
    assert!(svc::build_audit_reason(&long, &UserRef { username: Some("mod".into()), ..Default::default() }, "command").chars().count() <= 512);
}

fn infraction(case_id: i32, t: InfractionType, reason: &str, proof: Option<&str>, expires_ms: Option<i64>) -> Infraction {
    let at = |ms: i64| bot_db::serde_js::from_epoch_ms(ms).expect("date");
    Infraction {
        id: 1,
        case_id,
        guild_id: "955003".into(),
        target_user_id: ALICE.into(),
        moderator_id: BOB.into(),
        r#type: t,
        reason: Some(reason.into()),
        proof_url: proof.map(String::from),
        active: true,
        expires_at: expires_ms.map(at),
        escalation_tier_id: None,
        created_at: at(1_767_225_600_000),
        updated_at: at(1_767_225_600_000),
    }
}

/// An embed as discord.js `toJSON()` shows it: no timestamp, no `type`, no nulls.
fn embed_json(v: Value) -> Value {
    fn clean(v: Value) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.into_iter().filter(|(k, v)| !v.is_null() && k != "timestamp" && k != "type").map(|(k, v)| (k, clean(v))).collect(),
            ),
            Value::Array(a) => Value::Array(a.into_iter().map(clean).collect()),
            other => other,
        }
    }
    clean(v)
}

#[test]
fn build_mod_log_embed_agrees_with_the_js() {
    let long_proof = format!("https://example.com/{}", "p".repeat(2000));
    let infs = [
        infraction(7, InfractionType::Ban, &"r".repeat(3000), Some(&long_proof), Some(1_767_225_600_000 + DAY)),
        infraction(8, InfractionType::Warn, "", None, None),
        infraction(9, InfractionType::Timeout, "spam\nmore", None, Some(1_767_225_600_000 + HOUR)),
        infraction(10, InfractionType::Note, "[Pardon] Cleared 2 warning(s): ok", None, None),
    ];
    let targets = [
        json!({ "id": ALICE, "tag": "a`b", "avatar": "https://cdn.example/a.png" }),
        json!({ "id": ALICE }),
        json!({ "id": ALICE, "username": "alice" }),
        json!({ "id": ALICE, "tag": "x".repeat(150) }),
    ];
    let mods = [json!({ "username": "mod" }), Value::Null, json!({ "tag": "boss#0001", "id": BOB }), json!({ "id": BOB })];
    let sources = ["automod", "command", "dashboard", "escalation"];
    let fields: Vec<Value> = (1..=5).map(|i| json!({ "name": format!("n{i}"), "value": "v".repeat(2000) })).collect();
    let mut input = Vec::new();
    for (i, inf) in infs.iter().enumerate() {
        input.push(json!({ "infraction": inf, "target": targets[i], "moderator": mods[i], "source": sources[i], "logFields": if i == 0 { json!(fields) } else { Value::Null } }));
    }
    let js = js_pure(json!({ "modLog": input }));
    let js = js["modLog"].as_array().cloned().unwrap_or_default();
    for (i, inf) in infs.iter().enumerate() {
        let t = &targets[i];
        let target = UserRef {
            id: ALICE.into(),
            username: t["username"].as_str().map(String::from),
            tag: t["tag"].as_str().map(String::from),
            avatar_url: t["avatar"].as_str().map(String::from),
            bot: false,
        };
        let m = &mods[i];
        let moderator = (!m.is_null()).then(|| UserRef {
            id: m["id"].as_str().unwrap_or("").into(),
            username: m["username"].as_str().map(String::from),
            tag: m["tag"].as_str().map(String::from),
            ..Default::default()
        });
        let lf: Vec<(String, String)> = if i == 0 { (1..=5).map(|k| (format!("n{k}"), "v".repeat(2000))).collect() } else { Vec::new() };
        let embed = svc::build_mod_log_embed(&svc::ModLogInput { infraction: inf, target: Some(&target), moderator: moderator.as_ref(), source: sources[i], log_fields: &lf });
        let ours = serde_json::to_value(&embed).unwrap_or(Value::Null);
        assert_eq!(embed_json(ours.clone()), embed_json(js[i].clone()), "case {i}");
        assert_eq!(
            ours["timestamp"].as_str().and_then(bot_db::parse_js_date),
            js[i]["timestamp"].as_str().and_then(bot_db::parse_js_date),
            "the same timestamp"
        );
    }
}

#[test]
fn constants_display_type_max_snowflake_and_keys() {
    assert!(svc::action_meta("WARN").is_some_and(|m| m.active));
    assert_eq!(svc::display_type_of(InfractionType::Ban, true), "TEMPBAN");
    assert_eq!(svc::display_type_of(InfractionType::Ban, false), "BAN");
    assert_eq!(svc::display_type_of(InfractionType::Warn, true), "WARN");
    assert_eq!(svc::OVERDUE_BAN_LIFT_LIMIT_MS, 30 * DAY);
    assert_eq!(svc::max_snowflake(&["9", "10", "100000000000000001"]), Some("100000000000000001".into()));
    assert_eq!(svc::max_snowflake::<&str>(&[]), None);
    assert_eq!(svc::max_snowflake(&["abc", "5"]), Some("5".into()));
    let sweep = vec![vec!["1", "18446744073709551615", "99999999999999999999"], vec!["x"], vec![]];
    let js = js_pure(json!({ "maxSnowflake": sweep }));
    let ours: Vec<Value> = sweep.iter().map(|s| json!(svc::max_snowflake(s))).collect();
    assert_eq!(json!(ours), js["maxSnowflake"]);
    // Keys and job shapes, byte for byte with the JS.
    assert_eq!(svc::unban_key("1", "2"), "mod.unban:1:2");
    assert_eq!(svc::expire_key(42), "mod.expire:42");
    assert_eq!(feat_moderation::plugin::bansync_key("9"), "mod:bansync:9");
    assert_eq!(svc::case_lock_key("9"), "infraction:9");
    let at = bot_db::now_utc();
    let job = svc::unban_job("1", "2", Some(5), at);
    assert_eq!((job.dedupe_key.as_deref(), job.max_attempts, job.payload.clone()), (Some("mod.unban:1:2"), 20, json!({ "userId": "2", "infractionId": 5 })));
    assert_eq!(svc::unban_job("1", "2", None, at).payload, json!({ "userId": "2", "infractionId": null }));
    assert_eq!(svc::expire_job("1", 7, Some("9"), at).payload, json!({ "infractionId": 7, "roleId": "9" }));
    assert_eq!(svc::expire_job("1", 7, None, at).payload, json!({ "infractionId": 7 }));
    assert_eq!(svc::expire_job("1", 7, None, at).job_type, JobType::ModExpire);
    assert_eq!(svc::imported_reason(None), "[Imported] Pre-bot ban — reason unknown");
    assert_eq!(svc::imported_reason(Some("raid")), "[Imported] raid");
}

#[tokio::test]
async fn ban_markers() {
    let t = TestApp::new();
    svc::mark_bot_ban_action(&t, "g1", "u1", "ban", None);
    assert!(svc::was_bot_ban_action(&t, "g1", "u1", "ban"));
    assert!(svc::was_bot_ban_action(&t, "g1", "u1", "ban"), "non-destructive");
    assert!(!svc::was_bot_ban_action(&t, "g1", "u1", "unban"), "kinds are separate");
    svc::mark_bot_ban_action(&t, "g1", "u2", "unban", Some(1));
    t.clock.advance(15);
    assert!(!svc::was_bot_ban_action(&t, "g1", "u2", "unban"), "expired");
    for i in 0..(svc::BOT_MARK_CAP + 5) {
        svc::mark_bot_ban_action(&t, "g2", &i.to_string(), "ban", None);
    }
    assert!(!svc::was_bot_ban_action(&t, "g2", "0", "ban"), "the oldest marks are evicted past the cap");
    assert!(svc::was_bot_ban_action(&t, "g2", &(svc::BOT_MARK_CAP + 4).to_string(), "ban"));
}

#[test]
fn notify_embed_and_reply_helpers_text() {
    let n = svc::Notify {
        guild_name: "Guild".into(),
        action: "MUTE".into(),
        reason: String::new(),
        duration_ms: None,
        expires_at: None,
        case_id: Some(3),
        moderator: Some(UserRef { username: Some("mod".into()), ..Default::default() }),
        show_moderator: true,
    };
    let v = serde_json::to_value(svc::build_notify_embed(&n)).unwrap_or(Value::Null);
    assert_eq!(v["title"], "🔇 You were muted in Guild");
    assert_eq!(v["fields"][0], json!({ "name": "Reason", "value": "No reason provided", "inline": false }));
    assert_eq!(v["fields"][1], json!({ "name": "Duration", "value": "Until a moderator unmutes you", "inline": true }));
    assert_eq!(v["fields"][2]["value"], "mod");
    assert_eq!(v["footer"]["text"], "Case #3");
    let soft = svc::Notify { action: "SOFTBAN".into(), show_moderator: false, ..n };
    let v = serde_json::to_value(svc::build_notify_embed(&soft)).unwrap_or(Value::Null);
    assert_eq!(v["description"], "Your recent messages were removed. You can rejoin with a new invite.");
    assert_eq!(v["fields"].as_array().map(Vec::len), Some(1));

    let e = serde_json::to_value(svc::mod_error_embed(&svc::ModError::new(ModCode::Immune, "x"))).unwrap_or(Value::Null);
    assert_eq!(e["title"], "❌  Member Is Immune");
    let applied = svc::ModError { code: ModCode::DiscordError, error: "y".into(), applied: true };
    assert_eq!(serde_json::to_value(svc::mod_error_embed(&applied)).unwrap_or(Value::Null)["title"], "⚠️  Done, But Not Recorded");
}

// ── /modconfig helpers ────────────────────────────────────────────────────────

#[test]
fn modconfig_validation_and_tier_editing() {
    use modconfig::{add_tier, validate_tier};
    assert_eq!(validate_tier("TIMEOUT", None, Some("1h")), Ok((None, Some(3600))));
    assert!(validate_tier("TIMEOUT", None, None).unwrap_err().starts_with("Timeout tiers need"));
    assert!(validate_tier("TIMEOUT", None, Some("29d")).is_err());
    assert!(validate_tier("TEMPBAN", None, Some("2y")).unwrap_err().starts_with("Temporary ban tiers"));
    assert!(validate_tier("MUTE", None, Some("30s")).unwrap_err().starts_with("Mute tiers need"));
    assert_eq!(validate_tier("MUTE", Some("7d"), None), Ok((Some(604_800), None)));
    assert!(validate_tier("KICK", None, Some("1h")).unwrap_err().starts_with("Kick and permanent ban"));
    assert!(validate_tier("KICK", Some("30s"), None).unwrap_err().starts_with("Invalid `within`"));
    assert!(validate_tier("KICK", None, Some("soon")).unwrap_err().starts_with("Invalid `duration`"));

    let mut s = settings_row();
    assert!(add_tier(&s, 3, "MUTE", None, None).unwrap_err().0.contains("need a mute role"));
    assert!(add_tier(&s, 3, "MUTE", None, Some(3600)).is_ok(), "a short mute tier falls back to a timeout");
    let (tiers, replaced) = add_tier(&s, 3, "KICK", None, None).expect("added");
    assert!(!replaced);
    assert_eq!(tiers[0].id, 1);
    s.infraction_escalation = Some(serde_json::to_value(&tiers).unwrap_or(Value::Null));
    let (tiers, replaced) = add_tier(&s, 3, "BAN", None, None).expect("updated");
    assert!(replaced);
    assert_eq!((tiers.len(), tiers[0].id, tiers[0].action.as_str()), (1, 1, "BAN"));
    let many: Vec<Value> = (1..=10).map(|i| json!({ "id": i, "warnCount": i, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": null })).collect();
    s.infraction_escalation = Some(json!(many));
    assert!(add_tier(&s, 50, "KICK", None, None).unwrap_err().0.contains("at most 10 escalation tiers"));
    let text = modconfig::add_tier_text(&tiers, 3, None, false);
    assert!(text.starts_with("All tiers (the one just saved is in bold):\n**T1: 3 warnings → Permanent ban**"), "{text}");
    assert!(text.ends_with("\n\nModeration is off: tiers apply after `/module enable moderation`."));
    let choices = modconfig::tier_choices(Some(&s), "3 WARN");
    assert_eq!(choices.len(), 1);
    assert_eq!(modconfig::escalation_field(&[]), "None");
    assert_eq!(modconfig::roles_line(&[]), "None");
    assert_eq!(modconfig::roles_line(&["1".into(), "2".into()]), "<@&1>, <@&2>");
    assert_eq!(modconfig::visibility_text("PRIVATE"), "Only the moderator sees confirmations. /case replies (history included) are private.");
}

// ── /purge and /slowmode helpers (moderation-commands.test.js) ────────────────

fn slow_json(r: Result<i64, slowmode::SlowmodeError>) -> Value {
    match r {
        Ok(s) => json!({ "ok": true, "seconds": s }),
        Err(slowmode::SlowmodeError::Invalid) => json!({ "ok": false, "code": "invalid" }),
        Err(slowmode::SlowmodeError::TooShort) => json!({ "ok": false, "code": "too-short" }),
        Err(slowmode::SlowmodeError::TooLong) => json!({ "ok": false, "code": "too-long" }),
    }
}

#[test]
fn slowmode_helpers_agree_with_the_js() {
    let inputs = [
        "10s", "off", "OFF", " none ", "0", "0s", "5", "21600", "21601", "6h", "7h", "1h30m", "0.4s", "500ms", "abc", "", "12345678901234567", "1m", "2 hours",
        "disable", "1d",
    ];
    let seconds = [0, 1, 59, 60, 3600, 5400, 21_600];
    let embeds = [(0, 30, Some("spam"), Some("5"), false), (30, 0, None, None, true), (10, 30, None, Some("6"), false)];
    let js = js_pure(json!({
        "slowmode": inputs,
        "formatSlowmode": seconds,
        "slowmodeEmbed": embeds.iter().map(|(o, n, r, m, f)| json!({ "channelId": "1", "oldSeconds": o, "newSeconds": n, "reason": r, "moderatorId": m, "forum": f })).collect::<Vec<_>>(),
    }));
    let ours: Vec<Value> = inputs.iter().map(|s| slow_json(slowmode::parse_slowmode(s))).collect();
    assert_eq!(json!(ours), js["slowmode"]);
    let ours: Vec<Value> = seconds.iter().map(|s| json!(slowmode::format_slowmode(*s))).collect();
    assert_eq!(json!(ours), js["formatSlowmode"]);
    for (i, (o, nw, r, m, f)) in embeds.iter().enumerate() {
        let e = serde_json::to_value(slowmode::build_slowmode_embed("1", *o, *nw, *r, *m, *f)).unwrap_or(Value::Null);
        assert_eq!(embed_json(e), embed_json(js["slowmodeEmbed"][i].clone()));
    }
    let names = |typed: &str| slowmode::slowmode_choices(typed).iter().map(|c| json!(c.name.clone())).collect::<Vec<_>>();
    assert_eq!(names("").len(), 14);
    assert_eq!(names("7h")[0], "Too long: the maximum is 6 hours");
    assert_eq!(names("45s")[0], "45 seconds (custom)");
    assert_eq!(names("10m"), vec![json!("10 minutes")], "a preset is not repeated as custom");
}

fn msg_json(author_bot: bool, webhook: bool, embeds: usize, attachments: usize, content: &str, pinned: bool, kind: u8) -> serenity::all::Message {
    let atts: Vec<Value> = (0..attachments).map(|i| json!({ "id": format!("90000000000000000{i}"), "filename": "a.png", "size": 1, "url": "https://x/a.png", "proxy_url": "https://x/a.png" })).collect();
    let embeds: Vec<Value> = (0..embeds).map(|_| json!({ "title": "e" })).collect();
    let mut v = json!({
        "id": "700000000000000001", "channel_id": "800000000000000001", "guild_id": "900000000000000001",
        "author": { "id": "600000000000000001", "username": "u", "discriminator": "0", "avatar": null, "bot": author_bot },
        "content": content, "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": atts, "embeds": embeds, "pinned": pinned,
        "type": kind, "components": []
    });
    if webhook {
        v["webhook_id"] = json!("500000000000000001");
    }
    serde_json::from_value(v).expect("message")
}

#[test]
fn purge_filters_and_report_agree_with_the_js() {
    use purge::{Collected, Deleted, Filters, ReportOpts, Stop};
    let msgs = [(false, false, 0, 0, "Hello World"), (true, false, 1, 0, "bot"), (false, true, 0, 1, "hook"), (false, false, 2, 3, "both")];
    let opts = [
        json!({}),
        json!({ "filter": "bots" }),
        json!({ "filter": "humans" }),
        json!({ "filter": "embeds" }),
        json!({ "filter": "attachments" }),
        json!({ "contains": "world" }),
        json!({ "userId": "600000000000000001", "contains": "both" }),
        json!({ "userId": "6" }),
    ];
    let mut input = Vec::new();
    let mut cases = Vec::new();
    for m in &msgs {
        for o in &opts {
            input.push(json!({
                "message": { "author": { "id": "600000000000000001", "bot": m.0 }, "webhookId": if m.1 { json!("5") } else { Value::Null }, "embeds": vec![json!({}); m.2], "attachments": m.3, "content": m.4 },
                "opts": o,
            }));
            let f = Filters {
                user_id: o["userId"].as_str().map(String::from),
                filter: o["filter"].as_str().map(String::from),
                contains: o["contains"].as_str().map(String::from),
            };
            cases.push((msg_json(m.0, m.1, m.2, m.3, m.4, false, 0), f));
        }
    }
    let js = js_pure(json!({ "filters": input }));
    let ours: Vec<Value> = cases.iter().map(|(m, f)| json!(purge::matches_filters(m, f))).collect();
    assert_eq!(json!(ours), js["filters"]);
    assert!(purge::deletable(&msg_json(false, false, 0, 0, "", false, 0)));
    assert!(!purge::deletable(&msg_json(false, false, 0, 0, "", false, 21)), "a thread starter message cannot be deleted");

    let stop_name = |s: Stop| match s {
        Stop::Exhausted => "exhausted",
        Stop::Limit => "limit",
        Stop::Age => "age",
        Stop::Timeframe => "timeframe",
        Stop::ScanCap => "scan-cap",
        Stop::Deadline => "deadline",
        Stop::Permission => "permission",
        Stop::ChannelGone => "channel-gone",
        Stop::Error => "error",
    };
    type Row = (usize, usize, usize, usize, Stop, Option<u64>, usize, usize, usize, Option<Stop>, ReportOpts);
    let rows: Vec<Row> = vec![
        (5, 50, 0, 0, Stop::Limit, None, 5, 0, 0, None, ReportOpts { amount: Some(5), ..Default::default() }),
        (3, 1000, 2, 1, Stop::ScanCap, None, 2, 1, 0, None, ReportOpts { timeframe_ms: Some(2 * HOUR), user_id: Some("7".into()), filter: Some("bots".into()), contains: Some("a`b".into()), ..Default::default() }),
        (4, 10, 0, 0, Stop::Exhausted, None, 2, 0, 2, Some(Stop::Permission), ReportOpts::default()),
        (0, 0, 0, 0, Stop::Exhausted, None, 0, 0, 0, None, ReportOpts { amount: Some(10), ..Default::default() }),
        (0, 0, 0, 0, Stop::Age, None, 0, 0, 0, None, ReportOpts::default()),
        (0, 0, 0, 0, Stop::Timeframe, None, 0, 0, 0, None, ReportOpts { timeframe_ms: Some(30 * 60_000), ..Default::default() }),
        (0, 12, 3, 0, Stop::Age, None, 0, 0, 0, None, ReportOpts { filter: Some("embeds".into()), ..Default::default() }),
        (0, 0, 0, 0, Stop::Permission, Some(50013), 0, 0, 0, None, ReportOpts::default()),
        (0, 40, 0, 0, Stop::Error, Some(500), 0, 0, 0, None, ReportOpts::default()),
        (2, 40, 0, 0, Stop::Deadline, None, 0, 0, 2, Some(Stop::Deadline), ReportOpts::default()),
        (2, 40, 0, 0, Stop::Limit, None, 0, 2, 0, None, ReportOpts::default()),
    ];
    let input: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "targets": r.0, "scanned": r.1, "skippedPinned": r.2, "skippedUndeletable": r.3, "stopReason": stop_name(r.4), "errorCode": r.5,
                "deleted": r.6, "alreadyGone": r.7, "failed": r.8, "deleteStop": r.9.map(stop_name),
                "opts": { "amount": r.10.amount, "timeframeMs": r.10.timeframe_ms, "userId": r.10.user_id, "filter": r.10.filter, "contains": r.10.contains }
            })
        })
        .collect();
    let js = js_pure(json!({ "purgeReport": input }));
    for (i, r) in rows.iter().enumerate() {
        let c = Collected {
            targets: (0..r.0).map(|k| serenity::all::MessageId::new(k as u64 + 1)).collect(),
            scanned: r.1,
            skipped_pinned: r.2,
            skipped_undeletable: r.3,
            stop_reason: r.4,
            error_code: r.5,
        };
        let d = Deleted { deleted: r.6, already_gone: r.7, failed: r.8, stop_reason: r.9, error_code: None };
        let ours = serde_json::to_value(purge::build_purge_report(&c, &d, &r.10)).unwrap_or(Value::Null);
        assert_eq!(embed_json(ours), embed_json(js["purgeReport"][i].clone()), "report {i}");
    }
}

// ── The commands' early refusals, offline through the router ──────────────────

const G: &str = "920000000000000001";
const MOD: &str = "600000000000000010";

fn guild() -> FakeGuild {
    FakeGuild::new(G, "600000000000000099")
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role("820000000000000001", 10, P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::READ_MESSAGE_HISTORY)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role("820000000000000002", 5, P::MODERATE_MEMBERS | P::KICK_MEMBERS)
        .member(MOD, &["820000000000000002"])
        .channel("810000000000000001", 0, "general", &[])
        .channel("810000000000000002", 5, "news", &[])
        .channel("810000000000000003", 4, "category", &[])
}

fn moderator_slash(name: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(G).user(MOD).roles(&["820000000000000002"]).permissions(P::MODERATE_MEMBERS | P::KICK_MEMBERS).channel("810000000000000001")
}

#[tokio::test]
async fn purge_and_slowmode_refuse_bad_input_before_deferring() {
    use bot_core::command::option_type::{CHANNEL, INTEGER, STRING};
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild()]);
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modEnabled", true)).await.expect("memory store");
    let route = |i| bot_core::router::route_interaction(&t, i);

    route(moderator_slash("purge").build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls.last().and_then(|c| c.embed_title()), Some("❌  Missing Options"));
    assert!(calls.last().is_some_and(|c| c.ephemeral()));
    t.responses.clear();
    route(moderator_slash("purge").option("timeframe", STRING, json!("15d")).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title()), Some("❌  Timeframe Too Large"));
    t.responses.clear();
    route(moderator_slash("purge").option("timeframe", STRING, json!("soon")).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_description()), Some("Could not read `soon`. Try `30m`, `2h` or `7d`."));
    t.responses.clear();
    route(moderator_slash("purge").option("amount", INTEGER, json!(5)).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title()), Some("❌  Missing Permissions"));
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_description()), Some("I need **Manage Messages** in <#810000000000000001> to do that."));
    t.responses.clear();
    route(moderator_slash("purge").channel("810000000000000003").option("amount", INTEGER, json!(5)).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title()), Some("❌  Unsupported Channel"));

    t.responses.clear();
    route(moderator_slash("slowmode").option("duration", STRING, json!("7h")).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title()), Some("❌  Duration Too Long"));
    t.responses.clear();
    route(moderator_slash("slowmode").option("duration", STRING, json!("x`y")).build()).await;
    assert_eq!(
        t.responses.calls().last().and_then(|c| c.embed_description()),
        Some("Couldn't read `xy`. Use a duration such as `10s`, `5m` or `1h` (max 6 hours), or `off`.")
    );
    t.responses.clear();
    route(moderator_slash("slowmode").option("duration", STRING, json!("10s")).option("channel", CHANNEL, json!("810000000000000002")).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_description()), Some("Announcement channels do not support slowmode."));
    t.responses.clear();
    route(moderator_slash("slowmode").option("duration", STRING, json!("10s")).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_description()), Some("I need **Manage Channels** in <#810000000000000001> to do that."));

    t.responses.clear();
    route(FakeInteraction::autocomplete("slowmode").guild(G).user(MOD).roles(&["820000000000000002"]).permissions(P::MODERATE_MEMBERS).focused("duration", "2").build()).await;
    let choices = t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert!(!choices.is_empty());
}

#[tokio::test]
async fn ban_refuses_an_unreadable_duration_privately() {
    use bot_core::command::option_type::{STRING, USER};
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild()]);
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modEnabled", true)).await.expect("memory store");
    let ban = FakeInteraction::slash("ban").guild(G).user(MOD).roles(&["820000000000000002"]).permissions(P::BAN_MEMBERS).option("user", USER, json!(ALICE)).option("duration", STRING, json!("forever"));
    bot_core::router::route_interaction(&t, with_user(ban, ALICE).build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(calls[0].ephemeral(), "moderation commands defer privately");
    assert_eq!(calls[1].embed_title(), Some("❌  Invalid Duration"));
    assert_eq!(calls[1].embed_description(), Some("Couldn't read `forever`. Use something like `12h`, `7d` or `1mo`."));
}

fn settings_row() -> bot_db::GuildSettings {
    bot_core::settings::MemorySettingsStore::new_row("900000000000000001", serde_json::Map::new()).expect("a default row")
}

/// A user option's resolved user (`FakeInteraction` leaves `data.resolved` empty).
pub fn with_user(mut i: FakeInteraction, id: &str) -> FakeInteraction {
    i.json_mut()["data"]["resolved"]["users"][id] = json!({ "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null });
    i
}

// ── /purge: collect and delete (mod-utilities.test.js) ───────────────────────

mod purge_rest {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use bot_core::error::BoxFuture;
    use feat_moderation::guild_ops::{AuditEntry, AuditKind, DiscordError, ModRest, Rest};
    use serenity::all::{Ban, ChannelId, CreateMessage, GuildId, Member, Message, MessageId, Permissions, Role, RoleId, User, UserId};

    /// A channel's messages behind the REST calls `/purge` makes.
    #[derive(Default)]
    pub struct MsgRest {
        pub msgs: Vec<Message>,
        pub fetches: Mutex<Vec<(Option<u64>, u8)>>,
        pub calls: Mutex<Vec<String>>,
        pub fetch_error: Option<u64>,
        pub bulk_error: Option<u64>,
        pub single_fail: HashMap<u64, u64>,
    }

    fn off<T: Send + 'static>() -> Rest<T> {
        Box::pin(async { Err(DiscordError::offline()) })
    }

    fn ready<T: Send + 'static>(r: Result<T, DiscordError>) -> BoxFuture<Result<T, DiscordError>> {
        Box::pin(async move { r })
    }

    impl MsgRest {
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().map(|c| c.clone()).unwrap_or_default()
        }
    }

    impl ModRest for MsgRest {
        fn get_messages(&self, _: ChannelId, before: Option<MessageId>, limit: u8) -> Rest<Vec<Message>> {
            if let Ok(mut f) = self.fetches.lock() {
                f.push((before.map(|b| b.get()), limit));
            }
            if let Some(code) = self.fetch_error {
                return ready(Err(DiscordError::code(code, "fetch failed")));
            }
            let mut page: Vec<Message> = self.msgs.iter().filter(|m| before.is_none_or(|b| m.id < b)).cloned().collect();
            page.sort_by_key(|m| std::cmp::Reverse(m.id));
            page.truncate(usize::from(limit));
            ready(Ok(page))
        }
        fn bulk_delete(&self, _: ChannelId, ids: Vec<MessageId>) -> Rest<()> {
            if let Ok(mut c) = self.calls.lock() {
                c.push(format!("bulk {}", ids.len()));
            }
            ready(match self.bulk_error {
                Some(code) => Err(DiscordError::code(code, "bulk failed")),
                None => Ok(()),
            })
        }
        fn delete_message(&self, _: ChannelId, id: MessageId) -> Rest<()> {
            if let Ok(mut c) = self.calls.lock() {
                c.push(format!("single {id}"));
            }
            ready(match self.single_fail.get(&id.get()) {
                Some(code) => Err(DiscordError::code(*code, "delete failed")),
                None => Ok(()),
            })
        }
        fn get_member(&self, _: GuildId, _: UserId) -> Rest<Member> {
            off()
        }
        fn get_user(&self, _: UserId) -> Rest<User> {
            off()
        }
        fn get_ban(&self, _: GuildId, _: UserId) -> Rest<Option<Ban>> {
            off()
        }
        fn get_bans(&self, _: GuildId, _: Option<UserId>, _: u16) -> Rest<Vec<Ban>> {
            off()
        }
        fn ban(&self, _: GuildId, _: UserId, _: u32, _: String) -> Rest<()> {
            off()
        }
        fn unban(&self, _: GuildId, _: UserId, _: String) -> Rest<()> {
            off()
        }
        fn kick(&self, _: GuildId, _: UserId, _: String) -> Rest<()> {
            off()
        }
        fn timeout(&self, _: GuildId, _: UserId, _: Option<chrono::DateTime<chrono::Utc>>, _: String) -> Rest<()> {
            off()
        }
        fn add_role(&self, _: GuildId, _: UserId, _: RoleId, _: String) -> Rest<()> {
            off()
        }
        fn remove_role(&self, _: GuildId, _: UserId, _: RoleId, _: String) -> Rest<()> {
            off()
        }
        fn send_dm(&self, _: UserId, _: CreateMessage) -> Rest<(ChannelId, MessageId)> {
            off()
        }
        fn send_message(&self, _: ChannelId, _: CreateMessage) -> Rest<()> {
            off()
        }
        fn audit_entries(&self, _: GuildId, _: AuditKind, _: u8) -> Rest<Vec<AuditEntry>> {
            off()
        }
        fn get_role_ids(&self, _: GuildId) -> Rest<Vec<RoleId>> {
            off()
        }
        fn create_role(&self, _: GuildId, _: String, _: String) -> Rest<Role> {
            off()
        }
        fn deny_for_role(&self, _: ChannelId, _: RoleId, _: Permissions, _: Permissions, _: String) -> Rest<()> {
            off()
        }
        fn edit_slowmode(&self, _: ChannelId, _: u16, _: String) -> Rest<()> {
            off()
        }
    }
}

use purge_rest::MsgRest;

const T0: i64 = 1_767_225_600_000;

/// Message `n` (newer for a larger `n`), created `n` seconds after T0.
fn chat(n: u64, bot: bool, pinned: bool, kind: u8) -> serenity::all::Message {
    let mut m = msg_json(bot, false, 0, 0, &format!("message {n}"), pinned, kind);
    m.id = serenity::all::MessageId::new(1_000_000 + n);
    let at = bot_db::serde_js::from_epoch_ms(T0 + n as i64 * 1000).expect("date");
    m.timestamp = serenity::all::Timestamp::from(at);
    m
}

fn channel_of(n: u64) -> MsgRest {
    MsgRest { msgs: (1..=n).map(|i| chat(i, i % 2 == 0, false, 0)).collect(), ..Default::default() }
}

const CH: serenity::all::ChannelId = serenity::all::ChannelId::new(810_000_000_000_000_001);

#[tokio::test]
async fn collect_pages_counts_matches_and_stops_where_it_must() {
    use purge::{CollectOptions, Filters, Stop, collect_purge_targets};
    let now = || T0 + 10_000_000;
    // An amount above 100 pages through the channel.
    let rest = channel_of(250);
    let c = collect_purge_targets(&rest, CH, &CollectOptions { limit: 150, ..Default::default() }, &now).await;
    assert_eq!((c.targets.len(), c.stop_reason), (150, Stop::Limit));
    assert_eq!(rest.fetches.lock().map(|f| f.clone()).unwrap_or_default(), vec![(None, 100), (Some(1_000_151), 100)]);
    assert_eq!(c.targets[0].get(), 1_000_250, "newest first");
    // Filters are applied inside the scan, so the amount counts matches.
    let rest = channel_of(250);
    let bots = CollectOptions { limit: 5, filters: Filters { filter: Some("bots".into()), ..Default::default() }, ..Default::default() };
    let c = collect_purge_targets(&rest, CH, &bots, &now).await;
    assert_eq!((c.targets.len(), c.scanned), (5, 9));
    // A small amount reads one page.
    let rest = channel_of(250);
    collect_purge_targets(&rest, CH, &CollectOptions { limit: 3, ..Default::default() }, &now).await;
    assert_eq!(rest.fetches.lock().map(|f| f.len()).unwrap_or(0), 1);
    // Every mode is capped at SCAN_CAP examined messages.
    let rest = channel_of(1500);
    let none = CollectOptions { filters: Filters { contains: Some("nothing like this".into()), ..Default::default() }, ..Default::default() };
    let c = collect_purge_targets(&rest, CH, &none, &now).await;
    assert_eq!((c.scanned, c.stop_reason, c.targets.len()), (purge::SCAN_CAP, Stop::ScanCap, 0));
    assert_eq!(rest.fetches.lock().map(|f| f.len()).unwrap_or(0), 10);
    // Never past the cut-off.
    let rest = channel_of(50);
    let cut = CollectOptions { cutoff_ts: T0 + 20_000, cutoff_is_timeframe: true, ..Default::default() };
    let c = collect_purge_targets(&rest, CH, &cut, &now).await;
    assert_eq!((c.targets.len(), c.stop_reason), (30, Stop::Timeframe));
    assert!(c.targets.iter().all(|m| m.get() > 1_000_020));
    let c = collect_purge_targets(&rest, CH, &CollectOptions { cutoff_ts: T0 + 20_000, ..Default::default() }, &now).await;
    assert_eq!(c.stop_reason, Stop::Age);
    // Pinned kept unless asked; undeletable skipped; a short page means exhausted.
    let rest = MsgRest { msgs: vec![chat(1, false, true, 0), chat(2, false, false, 21), chat(3, false, false, 0)], ..Default::default() };
    let c = collect_purge_targets(&rest, CH, &CollectOptions::default(), &now).await;
    assert_eq!((c.targets.len(), c.skipped_pinned, c.skipped_undeletable, c.stop_reason), (1, 1, 1, Stop::Exhausted));
    let c = collect_purge_targets(&rest, CH, &CollectOptions { include_pinned: true, ..Default::default() }, &now).await;
    assert_eq!(c.targets.len(), 2);
    // Fetch errors become stop reasons.
    for (code, stop) in [(50013, Stop::Permission), (50001, Stop::Permission), (10003, Stop::ChannelGone), (500, Stop::Error)] {
        let rest = MsgRest { fetch_error: Some(code), ..Default::default() };
        let c = collect_purge_targets(&rest, CH, &CollectOptions::default(), &now).await;
        assert_eq!((c.stop_reason, c.error_code), (stop, Some(code)));
    }
    // An expired deadline makes no REST call.
    let rest = channel_of(10);
    let c = collect_purge_targets(&rest, CH, &CollectOptions { deadline: T0, ..Default::default() }, &now).await;
    assert_eq!(c.stop_reason, Stop::Deadline);
    assert!(rest.fetches.lock().map(|f| f.is_empty()).unwrap_or(false));
}

#[tokio::test]
async fn delete_goes_out_in_chunks_counts_exactly_and_stops_cleanly() {
    use purge::{Stop, delete_in_chunks};
    let t = TestApp::new();
    let ids = |n: u64| (1..=n).map(|i| serenity::all::MessageId::new(1_000_000 + i)).collect::<Vec<_>>();
    // 250 messages: 100 + 100 + 50, marked for the log hooks.
    let rest = MsgRest::default();
    let mut progress = Vec::new();
    let d = delete_in_chunks(&t, &rest, CH, &ids(250), "purge", i64::MAX, |n| progress.push(n)).await;
    assert_eq!(d.deleted, 250);
    assert_eq!(rest.calls(), vec!["bulk 100", "bulk 100", "bulk 50"]);
    assert_eq!(progress, vec![100, 200, 250]);
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&t, "1000250").as_deref(), Some("purge"));
    // A single message uses the single endpoint.
    let rest = MsgRest::default();
    let d = delete_in_chunks(&t, &rest, CH, &ids(1), "purge", i64::MAX, |_| {}).await;
    assert_eq!((d.deleted, rest.calls()), (1, vec!["single 1000001".to_string()]));
    // A rejected bulk call falls back per message: 10008 is already gone, another error is a failure.
    let rest = MsgRest { bulk_error: Some(50034), single_fail: [(1_000_002, 10008), (1_000_003, 500)].into_iter().collect(), ..Default::default() };
    let d = delete_in_chunks(&t, &rest, CH, &ids(4), "purge", i64::MAX, |_| {}).await;
    assert_eq!((d.deleted, d.already_gone, d.failed, d.stop_reason), (2, 1, 1, None));
    assert_eq!(rest.calls().len(), 5);
    // Losing the permission stops the run.
    let rest = MsgRest { bulk_error: Some(50013), ..Default::default() };
    let d = delete_in_chunks(&t, &rest, CH, &ids(250), "purge", i64::MAX, |_| {}).await;
    assert_eq!((d.deleted, d.stop_reason, d.error_code), (0, Some(Stop::Permission), Some(50013)));
    assert_eq!(rest.calls().len(), 1);
    let rest = MsgRest { single_fail: [(1_000_001, 10003)].into_iter().collect(), ..Default::default() };
    let d = delete_in_chunks(&t, &rest, CH, &ids(1), "purge", i64::MAX, |_| {}).await;
    assert_eq!((d.deleted, d.stop_reason), (0, Some(Stop::ChannelGone)), "a failing single delete is never counted");
    // An expired deadline makes no REST call.
    let rest = MsgRest::default();
    let d = delete_in_chunks(&t, &rest, CH, &ids(5), "purge", 0, |_| {}).await;
    assert_eq!((d.deleted, d.stop_reason), (0, Some(Stop::Deadline)));
    assert!(rest.calls().is_empty());
}

/// Audit: `message.deletable` follows discord.js 14.27's `UndeletableMessageTypes` only. The guild-discovery notices
/// (types 14-17) are deletable, so /purge must not count them as "system messages that cannot be deleted".
#[test]
fn purge_deletable_matches_discord_js_for_every_message_type() {
    let js = run_node("rust/fixtures/gen/feat-moderation-audit.mjs", &[]);
    for kind in 0u8..=60 {
        let expected = js["deletable"][kind.to_string()].as_bool().expect("JS answer");
        assert_eq!(purge::deletable(&msg_json(false, false, 0, 0, "x", false, kind)), expected, "message type {kind}");
    }
}
