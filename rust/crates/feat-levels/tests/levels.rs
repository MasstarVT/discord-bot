//! Port of `test/levels.test.js`, plus the golden, database and cross-language checks every feature crate runs.
//!
//! Offline: the XP maths, rolls, boosts, voice rules, role plans, announcement routing and level-up payloads against
//! a JS-generated fixture (`rust/fixtures/gen/feat-levels-js.mjs golden`), the cache keys, values and TTLs, the
//! command payloads and the plugin declaration. With `TEST_DATABASE_URL` / `TEST_REDIS_URL`: the raw SQL, the atomic
//! XP writes, the bulk give and the rank tie rule, the level-change pipeline, voice sessions and the sweep job, the
//! role cleanup, the rewards provider, the commands through the router, and both directions of the JS round trip.

use std::sync::OnceLock;

use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, load_golden, offline_discord_as, redis_url, run_node};
use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser, RenderContext};
use feat_levels::commands::levelconfig::{self, is_expiring_discord_link, merged_style, noxp_patch, status_fields, xp_change_lines};
use feat_levels::commands::{leaderboard, rank};
use feat_levels::xp_service::{self as svc, *};
use serde_json::{Value, json};
use serenity::all::{GuildId, Member, Permissions as P, UserId, VoiceState};

const JS: &str = "rust/fixtures/gen/feat-levels-js.mjs";

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_levels::register(&mut reg);
    reg
}

fn golden() -> &'static Value {
    static G: OnceLock<Value> = OnceLock::new();
    G.get_or_init(|| load_golden("feat-levels/pure.json"))
}

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn opt_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn member(guild: &str, id: &str, roles: &[&str], bot: bool) -> Member {
    serde_json::from_value(json!({
        "guild_id": guild,
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": bot },
        "roles": roles, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    }))
    .expect("member json")
}

fn voice_state(guild: &str, user: &str, channel: Option<&str>, self_deaf: bool, deaf: bool) -> VoiceState {
    serde_json::from_value(json!({
        "guild_id": guild, "channel_id": channel, "user_id": user, "session_id": "s", "deaf": deaf, "mute": false,
        "self_deaf": self_deaf, "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null
    }))
    .expect("voice state json")
}

// ── Commands and plugin ───────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["levelconfig", "rank", "leaderboard"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "levels");
}

#[test]
fn plugin_declaration() {
    let reg = registry();
    let p = reg.plugin_named("levels").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Leveling));
    let hooks: Vec<(&str, &str)> = p.hooks.iter().map(|h| (h.event().as_str(), h.id())).collect();
    assert_eq!(hooks, vec![("messageCreate", "levels"), ("voiceStateUpdate", "levels"), ("roleDelete", "levels"), ("guildReady", "levels")]);
    assert_eq!(p.jobs.iter().map(|j| j.0.as_str()).collect::<Vec<_>>(), vec!["levels.voiceSweep"]);
    assert_eq!(p.provides.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["xp"]);
    let meta = levelconfig::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("leveling"), json!(false), json!(2)));
    assert_eq!(rank::command().metadata()["permissionLevel"], 0);
    assert_eq!(leaderboard::command().metadata()["gated"], true);
}

#[test]
fn constants_match_the_js() {
    let c = &golden()["constants"];
    assert_eq!(c["DEFAULT_LEVELUP_MESSAGE"], DEFAULT_LEVELUP_MESSAGE);
    assert_eq!(c["ANNOUNCE_MODES"], json!(ANNOUNCE_MODES));
    for (mode, label) in ANNOUNCE_MODE_LABELS {
        assert_eq!(c["ANNOUNCE_MODE_LABELS"][mode], label);
    }
    assert_eq!(c["VOICE_KEY_TTL_SECONDS"], VOICE_KEY_TTL_SECONDS);
    assert_eq!(c["VOICE_CHECKPOINT_MS"], VOICE_CHECKPOINT_MS);
    assert_eq!(c["ROLE_GIVE_MAX_MEMBERS"], ROLE_GIVE_MAX_MEMBERS);
    assert_eq!(c["BULK_SYNC_MAX_MEMBERS"], BULK_SYNC_MAX_MEMBERS);
    assert_eq!(c["LEADERBOARD_MAX_ROWS"], LEADERBOARD_MAX_ROWS);
    let names: Vec<&str> = raw_sql().iter().map(|r| r.0).collect();
    assert_eq!(json!(names), c["RAW_SQL"]);
}

// ── 1. Level maths ────────────────────────────────────────────────────────────

#[test]
fn level_maths_total_xp_for_level_and_level_from_xp_are_inverses() {
    for l in 1..=900i64 {
        let need = total_xp_for_level(l as f64);
        if need > f64::from(bot_config::XP_MAX) * 2.0 {
            break;
        }
        assert_eq!(level_from_xp(need), l, "level {l}");
        assert_eq!(level_from_xp(need - 1.0), l - 1, "level {l} minus 1 xp");
    }
    assert_eq!(total_xp_for_level(1.0), 100.0);
    assert_eq!(total_xp_for_level(2.0), 255.0);
    assert_eq!(total_xp_for_level(5.0), 1150.0);
    assert_eq!(total_xp_for_level(10.0), 4675.0);
    assert_eq!(total_xp_for_level(100.0), 1_899_250.0);
    assert_eq!(total_xp_for_level(0.0), 0.0);
    assert_eq!(total_xp_for_level(-3.0), 0.0);
    assert_eq!(level_from_xp(1e6), 79);
    assert_eq!(level_from_xp(1e9), 838);
    assert_eq!(level_from_xp(-5.0), 0);
    assert_eq!(level_from_xp(f64::NAN), 0);
    assert_eq!(xp_into_level(1150.0), 0.0);
    assert_eq!(xp_into_level(1200.0), 50.0);
    assert_eq!(xp_into_level(-1.0), 0.0);
    assert!(total_xp_for_level(839.0) > f64::from(bot_config::XP_MAX), "level 839 is unreachable");
    assert_eq!(xp_to_next_level(5), 475);
    assert_eq!(xp_to_next_level(0), 100);
}

#[test]
fn level_maths_agree_with_the_js() {
    for c in arr(&golden()["levels"]) {
        let xp = c["xp"].as_f64().unwrap_or_default();
        assert_eq!(json!(level_from_xp(xp)), c["level"], "levelFromXp({xp})");
        assert_eq!(bot_core::command::js_number(xp_into_level(xp)), c["into"], "xpIntoLevel({xp})");
    }
    for c in arr(&golden()["totals"]) {
        let l = c["level"].as_f64().unwrap_or_default();
        assert_eq!(bot_core::command::js_number(total_xp_for_level(l)), c["total"], "totalXpForLevel({l})");
    }
}

// ── 2. rollXp ─────────────────────────────────────────────────────────────────

#[test]
fn roll_xp_range_swap_multiplier_boost_and_clamps() {
    let at = |r: f64| move || r;
    assert_eq!(roll_xp(RollXp::new(15.0, 25.0), at(0.0)), 15);
    assert_eq!(roll_xp(RollXp::new(15.0, 25.0), at(0.99999)), 25);
    assert_eq!(roll_xp(RollXp::new(25.0, 15.0), at(0.0)), 15, "swapped");
    assert_eq!(roll_xp(RollXp { multiplier: 1.5, boost: 2.0, ..RollXp::new(15.0, 25.0) }, at(0.0)), 45);
    assert_eq!(roll_xp(RollXp { multiplier: 0.0, ..RollXp::new(15.0, 25.0) }, at(0.0)), 15, "multiplier 0 counts as 1");
    assert_eq!(roll_xp(RollXp { boost: 0.5, ..RollXp::new(15.0, 25.0) }, at(0.0)), 15, "boost below 1 counts as 1");
    assert_eq!(roll_xp(RollXp::new(0.0, 0.0), at(0.0)), 1, "min 0 clamps to 1");
    assert_eq!(roll_xp(RollXp::new(5.0, 5000.0), at(0.99999)), 1000, "max clamps to 1000");
    assert_eq!(normalize_xp_range(None, None), (15, 25));
    assert_eq!(normalize_xp_range(Some(30.9), Some(10.0)), (10, 30));
    for _ in 0..200 {
        let v = roll_xp(RollXp::new(15.0, 25.0), math_random);
        assert!((15..=25).contains(&v));
    }
}

#[test]
fn ranges_and_rolls_agree_with_the_js() {
    for c in arr(&golden()["ranges"]) {
        let (lo, hi) = normalize_xp_range(opt_f64(&c["min"]), opt_f64(&c["max"]));
        assert_eq!(json!({ "min": lo, "max": hi }), c["out"], "{c}");
    }
    for c in arr(&golden()["rolls"]) {
        let f = |k: &str| c[k].as_f64().unwrap_or_default();
        let r = f("r");
        let got = roll_xp(RollXp { min: Some(f("min")), max: Some(f("max")), multiplier: f("multiplier"), boost: f("boost") }, move || r);
        assert_eq!(json!(got), c["xp"], "{c}");
    }
}

// ── 3. Boost roles ────────────────────────────────────────────────────────────

#[test]
fn boost_roles_agree_with_the_js() {
    for c in arr(&golden()["boosts"]) {
        let out = normalize_boost_roles(Some(&c["input"]));
        assert_eq!(boost_roles_json(&out), c["out"], "{}", c["input"]);
    }
    let out = normalize_boost_roles(Some(&golden()["boosts"][2]["input"]));
    let has = |held: &'static [&'static str]| move |id: &str| held.contains(&id);
    assert_eq!(best_boost_multiplier(&out, has(&["955009200000000001", "955009200000000004"])), 3.0);
    assert_eq!(best_boost_multiplier(&out, has(&["955009200000000004"])), 1.23);
    assert_eq!(best_boost_multiplier(&out, |_| false), 1.0);
    assert_eq!(best_boost_multiplier(&[], |_| true), 1.0);
    assert!(normalize_boost_roles(None).is_empty());
}

#[test]
fn has_no_xp_role_and_no_xp_channels() {
    let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(has_no_xp_role(&ids(&["1", "2"]), |id| id == "2"));
    assert!(!has_no_xp_role(&ids(&["1"]), |_| false));
    assert!(!has_no_xp_role(&[], |_| true));

    let ch = |id: &str, parent: Option<&str>, thread: bool, pp: Option<&str>| ChannelRef {
        id: id.into(),
        parent_id: parent.map(String::from),
        is_thread: thread,
        parent_parent_id: pp.map(String::from),
    };
    let thread = ch("T", Some("C"), true, Some("CAT"));
    assert!(is_no_xp_channel(&ids(&["X"]), Some(&ch("X", None, false, None))));
    assert!(is_no_xp_channel(&ids(&["C"]), Some(&thread)));
    assert!(is_no_xp_channel(&ids(&["CAT"]), Some(&thread)));
    assert!(is_no_xp_channel(&ids(&["T"]), Some(&thread)));
    assert!(is_no_xp_channel(&ids(&["CAT"]), Some(&ch("X", Some("CAT"), false, None))));
    assert!(!is_no_xp_channel(&ids(&["OTHER"]), Some(&ch("X", Some("CAT"), false, None))));
    assert!(!is_no_xp_channel(&ids(&["CAT"]), Some(&ch("X", None, false, Some("CAT")))), "the grandparent counts for threads only");
    assert!(!is_no_xp_channel(&ids(&["X"]), None));
    assert!(!is_no_xp_channel(&[], Some(&ch("X", None, false, None))));
}

// ── 5. Voice rules ────────────────────────────────────────────────────────────

#[test]
fn voice_eligibility_every_reason_in_order() {
    let base = VoiceInput {
        channel_id: Some("V".into()),
        parent_id: Some("CAT".into()),
        afk_channel_id: Some("AFK".into()),
        other_listeners: 1,
        ..Default::default()
    };
    let reason = |i: VoiceInput| voice_eligibility(&i).reason;
    assert_eq!(voice_eligibility(&base), Eligibility { eligible: true, reason: None });
    assert_eq!(reason(VoiceInput { channel_id: None, ..base.clone() }), Some("no-channel"));
    assert_eq!(reason(VoiceInput { channel_id: Some("AFK".into()), ..base.clone() }), Some("afk"));
    assert_eq!(reason(VoiceInput { no_xp_channel_ids: vec!["V".into()], ..base.clone() }), Some("no-xp-channel"));
    assert_eq!(reason(VoiceInput { no_xp_channel_ids: vec!["CAT".into()], ..base.clone() }), Some("no-xp-channel"), "via the category");
    assert_eq!(reason(VoiceInput { no_xp_role: true, ..base.clone() }), Some("no-xp-role"));
    assert_eq!(reason(VoiceInput { self_deaf: true, ..base.clone() }), Some("deafened"));
    assert_eq!(reason(VoiceInput { server_deaf: true, ..base.clone() }), Some("deafened"));
    assert_eq!(reason(VoiceInput { other_listeners: 0, ..base.clone() }), Some("alone"));
    let all = VoiceInput {
        channel_id: Some("AFK".into()),
        no_xp_channel_ids: vec!["AFK".into()],
        no_xp_role: true,
        self_deaf: true,
        other_listeners: 0,
        ..base.clone()
    };
    assert_eq!(reason(all), Some("afk"));
    assert_eq!(reason(VoiceInput { no_xp_role: true, self_deaf: true, other_listeners: 0, ..base.clone() }), Some("no-xp-role"));
    assert_eq!(reason(VoiceInput { self_deaf: true, other_listeners: 0, ..base }), Some("deafened"));
}

#[test]
fn count_other_listeners_excludes_bots_self_and_deafened() {
    let m = |id: &str, bot: bool, self_deaf: bool, server_deaf: bool| Listener { id: id.into(), bot, self_deaf, server_deaf };
    let members =
        vec![m("me", false, false, false), m("bot", true, false, false), m("deaf", false, true, false), m("sdeaf", false, false, true), m("a", false, false, false), m("b", false, false, false)];
    assert_eq!(count_other_listeners(&members, Some("me")), 2);
    assert_eq!(count_other_listeners(&members, None), 3);
    assert_eq!(count_other_listeners(&[], Some("me")), 0);
}

#[test]
fn voice_sessions_segments_and_rates_agree_with_the_js() {
    for c in arr(&golden()["sessions"]) {
        let got = parse_voice_session(Some(&c["raw"]), 1_700_000_100_000.0).map(|s| json!({ "start": bot_core::command::js_number(s.start), "channelId": s.channel_id }));
        assert_eq!(got.unwrap_or(Value::Null), c["out"], "parseVoiceSession({})", c["raw"]);
    }
    assert_eq!(parse_voice_session(None, 1.0), None);
    for c in arr(&golden()["segments"]) {
        let f = |k: &str| c[k].as_f64().unwrap_or_default();
        let s = compute_voice_segment(f("start"), f("now"), f("readyAt"), VOICE_SEGMENT_CAP_MINUTES);
        assert_eq!(json!({ "minutes": s.minutes, "nextStart": bot_core::command::js_number(s.next_start) }), c["out"], "{c}");
    }
    for c in arr(&golden()["voiceXp"]) {
        let f = |k: &str| c[k].as_f64().unwrap_or_default();
        assert_eq!(json!(voice_xp_for_minutes(f("minutes"), opt_f64(&c["rate"]), f("multiplier"), f("boost"))), c["xp"], "{c}");
    }
}

#[test]
fn voice_change_matters_skips_mute_camera_and_stream() {
    let g = "955009000000000001";
    let plain = voice_state(g, "1", Some("2"), false, false);
    let deaf = voice_state(g, "1", Some("2"), true, false);
    let sdeaf = voice_state(g, "1", Some("2"), false, true);
    assert!(!voice_change_matters("state", Some(&plain), Some(&plain)), "mute, camera, stream");
    assert!(voice_change_matters("state", Some(&plain), Some(&deaf)));
    assert!(voice_change_matters("state", Some(&sdeaf), Some(&plain)));
    assert!(!voice_change_matters("state", Some(&deaf), Some(&sdeaf)), "still deafened");
    for change in ["join", "leave", "move"] {
        assert!(voice_change_matters(change, Some(&plain), Some(&plain)));
    }
}

// ── 10. Role plans, 11. announcement routing ─────────────────────────────────

#[test]
fn plan_level_roles_agrees_with_the_js_and_never_overlaps() {
    for c in arr(&golden()["plans"]) {
        let rewards: Vec<(i64, String)> =
            arr(&c["rewards"]).iter().map(|r| (r["level"].as_i64().unwrap_or_default(), r["roleId"].as_str().unwrap_or_default().to_string())).collect();
        let held: Vec<String> = arr(&c["held"]).iter().filter_map(|h| h.as_str().map(String::from)).collect();
        let p = plan_level_roles(
            &rewards,
            c["level"].as_i64().unwrap_or_default(),
            c["stack"].as_bool().unwrap_or_default(),
            c["syncDown"].as_bool().unwrap_or_default(),
            |id| held.iter().any(|h| h == id),
        );
        assert_eq!(json!({ "add": p.add, "remove": p.remove }), c["out"], "{c}");
        assert!(!p.add.iter().any(|a| p.remove.contains(a)));
    }
}

#[test]
fn resolve_announce_target_the_whole_matrix() {
    for c in arr(&golden()["targets"]) {
        let t = resolve_announce_target(c["mode"].as_str(), c["source"].as_str().unwrap_or_default(), c["cfg"].as_str(), c["cur"].as_str());
        let got = match t {
            AnnounceTarget::None => json!({ "kind": "none" }),
            AnnounceTarget::Dm => json!({ "kind": "dm" }),
            AnnounceTarget::Channel { channel_id, fallback } => json!({ "kind": "channel", "channelId": channel_id, "fallback": fallback }),
        };
        assert_eq!(got, c["out"], "{c}");
    }
}

// ── 12. levelUpVars / buildLevelUpPayload ─────────────────────────────────────

fn render_ctx() -> RenderContext {
    let rc = &golden()["renderContext"];
    let u = &rc["userA"];
    let gu = &rc["guildA"];
    let s = |v: &Value| v.as_str().map(String::from);
    let user = PlaceholderUser {
        id: s(&u["id"]).unwrap_or_default(),
        username: s(&u["username"]),
        tag: s(&u["tag"]),
        discriminator: s(&u["discriminator"]),
        global_name: s(&u["globalName"]),
        bot: false,
        created_timestamp: u["createdTimestamp"].as_f64(),
        avatar_url: s(&u["displayAvatarURL"]),
    };
    let guild = PlaceholderGuild {
        id: s(&gu["id"]).unwrap_or_default(),
        name: s(&gu["name"]),
        owner_id: s(&gu["ownerId"]),
        member_count: gu["memberCount"].as_f64(),
        verification_level: Some(gu["verificationLevel"].to_string()),
        joined_timestamp: gu["joinedTimestamp"].as_f64(),
        created_timestamp: gu["createdTimestamp"].as_f64(),
        premium_subscription_count: Some(gu["premiumSubscriptionCount"].to_string()),
        premium_tier: Some(gu["premiumTier"].to_string()),
        icon_url: s(&gu["iconURL"]),
    };
    let m = &rc["memberA"];
    RenderContext {
        member: Some(PlaceholderMember {
            display_name: s(&m["displayName"]),
            joined_timestamp: m["joinedTimestamp"].as_f64(),
            avatar_url: s(&m["avatarURL"]),
            user: Some(user.clone()),
            guild: Some(guild.clone()),
        }),
        user: Some(user),
        guild: Some(guild),
        channel: Some(PlaceholderChannel { id: "500".into(), name: Some("general".into()), kind: Some(0) }),
        ..Default::default()
    }
}

fn strip_timestamps(v: Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(
            a.into_iter()
                .map(|mut e| {
                    if let Value::Object(m) = &mut e {
                        m.remove("timestamp");
                    }
                    e
                })
                .collect(),
        ),
        other => other,
    }
}

#[test]
fn level_up_vars_and_payloads_agree_with_the_js() {
    let vars: serde_json::Map<String, Value> = level_up_vars("111111111111111111", "A*li_", 7, 1234.0, None)
        .into_iter()
        .map(|(k, v)| (k, json!(match v { bot_util::placeholder_parser::VarValue::Text(t) => t, other => format!("{other:?}") })))
        .collect();
    assert_eq!(Value::Object(vars), golden()["vars"]);

    for c in arr(&golden()["payloads"]) {
        let template = c["template"].as_str().unwrap_or_default();
        let p = build_level_up_payload(LevelUpInput {
            template,
            use_embed: c["useEmbed"].as_bool().unwrap_or_default(),
            ctx: render_ctx(),
            user_id: "111111111111111111",
            name: "A*li_",
            guild_name: "My Server",
            level: 7,
            xp: 12345.0,
            rank: c["rank"].as_i64(),
            embed_color: c["embedColor"].as_i64(),
            dm: c["dm"].as_bool().unwrap_or_default(),
        });
        let label = format!("template {:?} embed={} dm={}", &template[..template.len().min(40)], c["useEmbed"], c["dm"]);
        match (p, c["out"].is_null()) {
            (None, true) => {}
            (Some(p), false) => {
                let j = p.to_json();
                let theirs = &c["out"];
                assert_eq!(j.get("content").cloned().unwrap_or(Value::Null), theirs["content"], "{label}: content");
                assert_eq!(strip_timestamps(j.get("embeds").cloned().unwrap_or(Value::Null)), theirs["embeds"], "{label}: embeds");
                assert_eq!(j.get("components").cloned().unwrap_or(Value::Null), theirs["components"], "{label}: components");
                assert_eq!(j["allowedMentions"], theirs["allowedMentions"], "{label}: allowedMentions");
            }
            (p, _) => panic!("{label}: rust {:?}, js {}", p.map(|x| x.to_json()), c["out"]),
        }
    }
    for c in arr(&golden()["mentions"]) {
        assert_eq!(json!(template_mentions_player(c["t"].as_str().unwrap_or_default())), c["out"], "{c}");
    }
    assert_eq!(rank_where("g", "u", 50), golden()["rankWhere"]);
}

#[test]
fn levelconfig_card_warns_only_about_discord_upload_links() {
    for c in arr(&golden()["expiring"]) {
        assert_eq!(json!(is_expiring_discord_link(c["url"].as_str().unwrap_or_default())), c["out"], "{c}");
    }
}

#[test]
fn levelconfig_pure_pieces() {
    // noxp mutator: prune, duplicates, cap, remove.
    let cur = vec!["1".to_string(), "2".to_string(), "gone".to_string()];
    let exists: std::collections::HashSet<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
    let patch = noxp_patch(&cur, "noXpRoleIds", "3", false, Some(&exists), 100, "role").expect("added");
    assert_eq!(patch.0["noXpRoleIds"], json!(["1", "2", "3"]));
    let err = noxp_patch(&cur, "noXpRoleIds", "1", false, None, 100, "role").expect_err("duplicate");
    assert_eq!(err.downcast_ref::<levelconfig::ConfigError>().map(|e| e.0.as_str()), Some("Already on the no-XP list."));
    let err = noxp_patch(&cur, "noXpChannelIds", "9", true, None, 100, "channel").expect_err("not listed");
    assert_eq!(err.to_string(), "That channel is not on the no-XP list.");
    let err = noxp_patch(&cur, "noXpRoleIds", "9", false, None, 3, "role").expect_err("cap");
    assert_eq!(err.to_string(), "You can have at most 3 no-XP roles.");
    assert_eq!(noxp_patch(&cur, "noXpRoleIds", "2", true, None, 3, "role").expect("removed").0["noXpRoleIds"], json!(["1", "gone"]));

    // Boost add: the last value wins, the cap counts only new roles.
    let p = levelconfig::boost_add_patch(Some(&json!([{ "roleId": "955009200000000001", "multiplier": 2 }])), "955009200000000001", 1.5).expect("saved");
    assert_eq!(p.0["xpBoostRoles"], json!([{ "roleId": "955009200000000001", "multiplier": 1.5 }]));
    let ten: Vec<Value> = (0..10).map(|i| json!({ "roleId": format!("9550092000000001{i:02}"), "multiplier": 2 })).collect();
    let err = levelconfig::boost_add_patch(Some(&Value::Array(ten)), "955009200000000999", 2.0).expect_err("cap");
    assert_eq!(err.to_string(), "You can have at most 10 XP boost roles.");

    // xpChangeLines.
    let roles = RoleSync { added: vec!["1".into()], removed: vec!["2".into(), "3".into()], failed: vec![("4".into(), "x".into())] };
    assert_eq!(
        xp_change_lines(3, 5, Some(&roles)),
        "\nLevel 3 → 5.\nRoles added: <@&1>.\nRoles removed: <@&2>, <@&3>.\nCould not change: <@&4> (check my role position and Manage Roles)."
    );
    assert_eq!(xp_change_lines(2, 2, None), "");

    // The card style merge keeps the JS key order and writes numbers like JSON.stringify.
    let before = bot_cards::default_rank_card_style();
    let merged = merged_style(&before, &[("accentColor", json!("#FF0000")), ("overlayOpacity", json!(0))]);
    assert_eq!(serde_json::to_string(&merged).unwrap_or_default(), r##"{"background":"#2B2D31","backgroundUrl":null,"accentColor":"#FF0000","textColor":"#FFFFFF","overlayOpacity":0}"##);

    // Leaderboard lines, footer and start page.
    assert_eq!(leaderboard::line(1, "5", Some("A*b"), 1150), "🥇 **A\\*b** — Level **5** · 1,150 XP");
    assert_eq!(leaderboard::line(4, "5", None, 99), "**#4** <@5> — Level **0** · 99 XP");
    assert_eq!(leaderboard::footer(200, 1234, Some(1500)), "Top 200 of 1,234 ranked members · You: #1,500");
    assert_eq!(leaderboard::footer(3, 3, None), "Top 3 of 3 ranked members");
    assert_eq!((leaderboard::start_page(None, 5), leaderboard::start_page(Some(20), 5), leaderboard::start_page(Some(0), 5)), (0, 4, 0));
    assert_eq!(cooldown_ttl(0), 1);
    assert_eq!(cooldown_ttl(75), 75);
}

#[tokio::test]
async fn status_fields_show_the_configuration() {
    let g = "955009000000000005";
    let t = TestApp::new().with_guilds([g]);
    let s = t
        .settings()
        .update_settings(
            g,
            SettingsPatch::new()
                .set("xpEnabled", true)
                .set("xpMultiplier", 1.5)
                .set("levelUpMode", "CHANNEL")
                .set("levelUpChannelId", "5")
                .set("noXpRoleIds", json!(["7"]))
                .set("xpBoostRoles", json!([{ "roleId": "955009200000000001", "multiplier": 2 }]))
                .set("levelUpMessage", "hi ```x```"),
        )
        .await
        .expect("memory store");
    let f = status_fields(&s, 3, false);
    let get = |name: &str| f.iter().find(|x| x.0 == name).map(|x| x.1.clone()).unwrap_or_default();
    assert_eq!(get("Module"), "✅ On");
    assert_eq!(get("Multiplier"), "×1.5");
    assert_eq!(get("Announcements"), "Custom channel → <#5> ⚠️ missing · text");
    assert_eq!(get("XP boost roles"), "<@&955009200000000001> ×2");
    assert_eq!(get("No-XP roles"), "<@&7>");
    assert_eq!(get("No-XP channels"), "None");
    assert_eq!(get("Rank card"), "background #2B2D31 · accent #5865F2 · text #FFFFFF");
    assert_eq!(get("Level-up message"), "```\nhi ˋˋˋxˋˋˋ\n```");
    assert!(get("Role rewards").starts_with("3 configured"));
}

// ── Cache keys, values and TTLs (against the keys the JS wrote) ───────────────

#[tokio::test]
async fn cache_keys_values_and_ttls_match_the_js() {
    let calls = arr(&golden()["cacheCalls"]);
    let g = "955009000000000001";
    let u = "955009100000000001";
    assert_eq!(calls[0]["key"], cooldown_key(g, u));
    assert_eq!(calls[0]["ttl"], cooldown_ttl(0));
    assert_eq!(calls[1]["ttl"], 75, "Math.trunc of the cooldown");
    assert_eq!(calls[2]["key"], voice_key(g, u));

    // Message XP claims the cooldown with the JS key and TTL (the SQL then fails offline).
    let t = TestApp::new().with_guilds([g]);
    t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", true).set("xpCooldown", 0)).await.expect("memory store");
    let s = t.settings().get_settings_safe(g).await;
    let m = member(g, u, &[], false);
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let r = message_xp(&t, s.clone(), gid, &m, None, None, || 0.0).await;
    assert!(r.is_err(), "the database is unreachable offline");
    assert_eq!(t.cache_backend.raw(&cooldown_key(g, u)).as_deref(), Some("1"));
    assert!(t.cache_backend.pttl(&cooldown_key(g, u)) <= 1000);
    // A second message inside the cooldown is skipped.
    let again = message_xp(&t, s.clone(), gid, &m, None, None, || 0.0).await.expect("skipped");
    assert!(matches!(again, MessageXp::Skipped("cooldown")));
    // Redis down: fail closed.
    t.cache_backend.set_available(false);
    let down = message_xp(&t, s, gid, &member(g, "955009100000000009", &[], false), None, None, || 0.0).await.expect("skipped");
    assert!(matches!(down, MessageXp::Skipped("cooldown")));
    t.cache_backend.set_available(true);

    // A voice session start writes the JS value with the JS TTL.
    let settings = t.settings().get_settings(g).await.expect("settings");
    svc::set_ready_at(&t, 1);
    let view = VoiceView { channel_id: Some("955009300000000001".into()), other_listeners: 1, ..Default::default() };
    assert!(reconcile_voice_member(&t, gid, m.clone(), VoiceSource::View(view), settings, 1_700_000_000_000, false).await);
    let raw: Value = serde_json::from_str(&t.cache_backend.raw(&voice_key(g, u)).unwrap_or_default()).unwrap_or(Value::Null);
    assert_eq!(raw, calls[3]["value"]);
    assert_eq!(serde_json::to_string(&raw).unwrap_or_default(), serde_json::to_string(&calls[3]["value"]).unwrap_or_default(), "same key order");
    let ttl = t.cache_backend.pttl(&voice_key(g, u));
    assert!(ttl > (VOICE_KEY_TTL_SECONDS - 5) * 1000 && ttl <= VOICE_KEY_TTL_SECONDS * 1000, "{ttl}");
}

#[tokio::test]
async fn voice_sessions_start_discard_on_a_fresh_join_and_end_without_xp_offline() {
    let g = "955009000000000002";
    let u = "955009100000000002";
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let t = TestApp::new().with_guilds([g]);
    t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", true).set("voiceXpEnabled", true)).await.expect("memory store");
    let s = t.settings().get_settings(g).await.expect("settings");
    let m = member(g, u, &[], false);
    let now = 1_700_000_000_000i64;
    svc::set_ready_at(&t, now - 3_600_000);
    let ok = VoiceView { channel_id: Some("1".into()), other_listeners: 1, ..Default::default() };
    // A stored session on a fresh join is stale: discarded without credit and restarted.
    t.cache_backend.put_raw(&voice_key(g, u), &json!({ "start": now - 20 * 60_000, "channelId": "1" }).to_string(), None);
    assert!(reconcile_voice_member(&t, gid, m.clone(), VoiceSource::View(ok.clone()), s.clone(), now, true).await);
    let raw: Value = serde_json::from_str(&t.cache_backend.raw(&voice_key(g, u)).unwrap_or_default()).unwrap_or(Value::Null);
    assert_eq!(raw["start"], json!(now), "restarted at now");
    // Alone: the session ends (the key is deleted; less than a minute pays nothing, so no SQL runs).
    let alone = VoiceView { channel_id: Some("1".into()), other_listeners: 0, ..Default::default() };
    assert!(!reconcile_voice_member(&t, gid, m.clone(), VoiceSource::View(alone), s.clone(), now + 30_000, false).await);
    assert_eq!(t.cache_backend.raw(&voice_key(g, u)), None);
    // A no-XP role makes the member ineligible.
    t.settings().update_settings(g, SettingsPatch::new().set("noXpRoleIds", json!(["955009200000000077"]))).await.expect("memory store");
    let s = t.settings().get_settings(g).await.expect("settings");
    let held = member(g, u, &["955009200000000077"], false);
    assert!(!reconcile_voice_member(&t, gid, held, VoiceSource::View(ok), s, now, false).await);
    assert_eq!(t.cache_backend.raw(&voice_key(g, u)), None);
}

#[tokio::test]
async fn voice_xp_off_drops_the_member_key_without_credit() {
    let g = "955009000000000003";
    let u = "955009100000000003";
    let t = TestApp::new().with_guilds([g]);
    t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", true).set("voiceXpEnabled", false)).await.expect("memory store");
    t.cache_backend.put_raw(&voice_key(g, u), r#"{"start":1,"channelId":"1"}"#, None);
    let ev = VoiceStateUpdate {
        guild_id: GuildId::new(g.parse().unwrap_or(1)),
        old_state: None,
        new_state: voice_state(g, u, Some("1"), false, false),
        member: member(g, u, &[], false),
        change: VoiceChange::Join,
    };
    let s = t.settings().get_settings_safe(g).await;
    assert!(voice_state_xp(&t, s, &ev).await.is_none());
    assert_eq!(t.cache_backend.raw(&voice_key(g, u)), None);
}

#[tokio::test]
async fn bulk_lock_and_deprecated_entry_points() {
    let t = TestApp::new();
    assert!(try_start_bulk(&t, "1"));
    assert!(!try_start_bulk(&t, "1"));
    assert!(is_bulk_running(&t, "1"));
    end_bulk(&t, "1");
    assert!(!is_bulk_running(&t, "1"));
    grant_xp(&t).await;
    handle_voice_xp(&t).await; // no-ops, a warning each at most once
}

#[tokio::test]
async fn rewards_provider_refuses_while_the_module_is_off() {
    let g = "955009000000000004";
    let t = TestApp::new().with_guilds([g]);
    t.install(registry());
    t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", false)).await.expect("memory store");
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let r = t.rewards().grant(&t, RewardKind::Xp, RewardArgs::new(gid, "955009100000000004", 10)).await;
    assert_eq!(r.reason.as_deref(), Some("module-disabled"));
    let r = t.rewards().take(&t, RewardKind::Xp, RewardArgs::new(gid, "955009100000000004", 10)).await;
    assert_eq!(r.reason.as_deref(), Some("module-disabled"));
    t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", true)).await.expect("memory store");
    let bot = member(g, "955009100000000005", &[], true);
    let r = t.rewards().grant(&t, RewardKind::Xp, RewardArgs::new(gid, "955009100000000005", 10).member(Some(bot))).await;
    assert_eq!(r.reason.as_deref(), Some("bot"));
    let r = t.rewards().grant(&t, RewardKind::Xp, RewardArgs::new(gid, "955009100000000006", 10)).await;
    assert_eq!(r.reason.as_deref(), Some("not-member"), "offline, the member cannot be fetched");
}

// ── Database ──────────────────────────────────────────────────────────────────

async fn db(guilds: &[&'static str]) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = match redis_url() {
        Some(r) => DbTestApp::with_redis(&url, &r).await,
        None => DbTestApp::new(&url).await,
    };
    let t = t.with_guilds(guilds.iter().copied());
    t.reset_guilds(guilds).await;
    Some(t)
}

async fn enable(t: &DbTestApp, g: &str, extra: SettingsPatch) {
    let mut patch = SettingsPatch::new().set("xpEnabled", true);
    for (k, v) in extra.iter() {
        patch = patch.set(k, v.clone());
    }
    t.settings().update_settings(g, patch).await.expect("settings saved");
}

fn row_json(r: &bot_db::UserLevel) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

#[tokio::test]
async fn db_every_raw_sql_statement_executes_against_the_migrated_schema() {
    let Some(t) = db(&[]).await else { return };
    for (name, q) in raw_sql() {
        let mut tx = t.pool.begin().await.expect("tx");
        bot_db::InsertBuilder::new(&bot_db::GUILD_SETTINGS_INFO)
            .value("guildId", json!("0"))
            .and_then(|b| b.on_conflict_do_nothing(&["guildId"]))
            .and_then(|b| b.build())
            .expect("settings insert")
            .execute(&mut *tx)
            .await
            .expect("settings row");
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
    }
    for sql in [apply_delta_sql(), subtract_sql(), set_zero_sql(), set_xp_sql(), stored_level_sql(), bulk_give_sql(), bulk_level_sql()] {
        assert!(!sql.contains(" now()") || sql.contains(bot_db::SQL_NOW), "only SQL_NOW");
    }
}

#[tokio::test]
async fn db_atomic_xp_statements() {
    const G: &str = "955019000000000001";
    let Some(t) = db(&[G]).await else { return };
    let u = "955019100000000001";
    let r = apply_xp_delta(&t, XpDelta { messages: 1.0, touch_last_xp: true, ..XpDelta::new(G, u, 150.0) }).await.expect("apply");
    assert_eq!((r.xp, r.old_level, r.new_level, r.created), (150, 0, 1, true));
    let r = apply_xp_delta(&t, XpDelta::new(G, u, 50.0)).await.expect("apply");
    assert_eq!((r.xp, r.created), (200, false));
    let row = find_user_level(t.db(), G, u).await.expect("load").expect("row");
    assert_eq!((row.message_count, row.level), (1, 0), "the stored level is not touched by applyXpDelta");
    assert!(row.last_xp_at.is_some());
    // The stored level: one writer per transition.
    assert!(set_stored_level(t.db(), G, u, 200, 1).await.expect("set"));
    assert!(!set_stored_level(t.db(), G, u, 200, 1).await.expect("set"), "already at that level");
    assert!(!set_stored_level(t.db(), G, u, 999, 2).await.expect("set"), "stale XP");
    // Clamped at XP_MAX.
    let r = apply_xp_delta(&t, XpDelta::new(G, u, 2e9)).await.expect("apply");
    assert_eq!(r.xp, i64::from(bot_config::XP_MAX));
    // Subtract floors at 0 and never inserts.
    let r = subtract_xp(&t, G, u, 2e9).await.expect("subtract").expect("row");
    assert_eq!((r.xp, r.old_xp), (0, Some(i64::from(bot_config::XP_MAX))));
    assert_eq!(subtract_xp(&t, G, "955019100000000099", 5.0).await.expect("subtract"), None);
    assert!(find_user_level(t.db(), G, "955019100000000099").await.expect("load").is_none(), "R300");
    // setXp: 0 never creates; a value upserts.
    assert_eq!(set_xp(&t, G, "955019100000000098", 0.0).await.expect("set"), None);
    let r = set_xp(&t, G, u, 1150.0).await.expect("set").expect("row");
    assert_eq!((r.xp, r.new_level), (1150, 5));
    let r = set_xp(&t, G, u, 0.0).await.expect("set").expect("row");
    assert_eq!((r.xp, r.new_level), (0, 0));
    // reset deletes.
    assert_eq!(reset_xp(t.db(), G, u).await.expect("reset"), Some(1));
    assert_eq!(reset_xp(t.db(), G, u).await.expect("reset"), None);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_bulk_give_and_the_shared_rank_tie_rule() {
    const G: &str = "955019000000000002";
    let Some(t) = db(&[G]).await else { return };
    let ids: Vec<String> = (1..=5).map(|i| format!("95501910000000000{i}")).collect();
    let mut with_dupes = ids.clone();
    with_dupes.push(ids[0].clone());
    let res = give_xp_to_members(&t, G, &with_dupes, 100.0).await.expect("give");
    assert_eq!(res.updated, 5);
    assert_eq!(res.level_ups.len(), 5, "0 -> 1 for everyone");
    let again = give_xp_to_members(&t, G, &ids[..2], 100.0).await.expect("give");
    assert!(again.level_ups.is_empty(), "200 XP is still level 1");
    // Ties order by user id; getRank agrees with the leaderboard order.
    let rows = ranked_rows(t.db(), G, Some(0), None, LEADERBOARD_MAX_ROWS).await.expect("rows");
    let order: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(order, vec![ids[0].as_str(), ids[1].as_str(), ids[2].as_str(), ids[3].as_str(), ids[4].as_str()]);
    for (i, (u, xp)) in rows.iter().enumerate() {
        assert_eq!(get_rank(t.db(), G, u, *xp).await.expect("rank"), i as i64 + 1);
        let js = run_node(JS, &["rank", G, u, &xp.to_string()]);
        assert_eq!(js, json!(i + 1), "the JS getRank agrees");
    }
    assert_eq!(ranked_count(t.db(), G).await.expect("count"), 5);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_js_and_rust_read_each_others_xp_rows() {
    const G: &str = "955019000000000003";
    let Some(t) = db(&[G]).await else { return };
    let u = "955019100000000031";
    // JS writes, Rust reads.
    let js = run_node(JS, &["apply", G, u, "1200", "3", "7", "true"]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["xp"], 1200);
    let row = find_user_level(t.db(), G, u).await.expect("load").expect("row");
    assert_eq!((row.xp, row.message_count, row.voice_minutes), (1200, 3, 7));
    let mine = apply_xp_delta(&t, XpDelta::new(G, u, 10.0)).await.expect("apply");
    assert_eq!((mine.xp, mine.old_level, mine.new_level), (1210, 0, 5));
    let js = run_node(JS, &["give", G, r#"["955019100000000032","955019100000000031"]"#, "100"]);
    assert_eq!(js["updated"], 2, "{js}");
    let js_set = run_node(JS, &["setxp", G, "955019100000000033", "4675"]);
    assert_eq!(js_set["newLevel"], 10, "{js_set}");
    assert_eq!(find_user_level(t.db(), G, "955019100000000033").await.expect("load").map(|r| r.xp), Some(4675));
    // Rust writes, JS reads: the row reads the same through Prisma and Rust.
    set_xp(&t, G, "955019100000000034", 999.0).await.expect("set");
    apply_xp_delta(&t, XpDelta { voice_minutes: 4.0, touch_last_xp: true, ..XpDelta::new(G, "955019100000000034", 1.0) }).await.expect("apply");
    let back = run_node(JS, &["load", G, "955019100000000034"]);
    let ours = row_json(&find_user_level(t.db(), G, "955019100000000034").await.expect("load").expect("row"));
    assert_eq!(ours, back);
    assert_eq!(back["xp"], 1000);
    assert_eq!(back["voiceMinutes"], 4);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_role_rewards_and_settings_round_trip() {
    const G: &str = "955019000000000004";
    let Some(t) = db(&[G]).await else { return };
    enable(&t, G, SettingsPatch::new()).await;
    levelconfig::upsert_reward(&t, G, 5, "955019200000000005").await.expect("reward");
    levelconfig::upsert_reward(&t, G, 5, "955019200000000006").await.expect("replaced");
    let js = run_node(JS, &["addreward", G, "10", "955019200000000010"]);
    assert!(js.get("error").is_none(), "{js}");
    let rows = level_roles(t.db(), G).await.expect("rows");
    assert_eq!(reward_pairs(&rows), vec![(5, "955019200000000006".to_string()), (10, "955019200000000010".to_string())]);
    let back = run_node(JS, &["rewards", G]);
    assert_eq!(serde_json::to_value(&rows).unwrap_or(Value::Null), back, "Prisma reads the same rows");

    // JS writes the boost list; Rust reads it. Rust writes (via the mutator); JS reads it.
    let js = run_node(JS, &["boost", G, r#"[{"roleId":"955019200000000001","multiplier":1.5},{"roleId":"955019200000000002","multiplier":3}]"#]);
    assert!(js.get("error").is_none(), "{js}");
    t.settings().invalidate_settings(G).await;
    let s = t.settings().get_settings(G).await.expect("settings");
    let boosts = normalize_boost_roles(s.xp_boost_roles.as_ref());
    assert_eq!(boosts.len(), 2);
    t.settings()
        .update_settings_with(G, |cur| levelconfig::boost_add_patch(cur.xp_boost_roles.as_ref(), "955019200000000003", 2.0))
        .await
        .expect("saved");
    t.settings()
        .update_settings(G, SettingsPatch::new().set("noXpRoleIds", json!(["955019200000000002", "955019200000000009"])))
        .await
        .expect("saved");
    let back = run_node(JS, &["settings", G]);
    assert_eq!(
        back["xpBoostRoles"],
        json!([{ "roleId": "955019200000000001", "multiplier": 1.5 }, { "roleId": "955019200000000002", "multiplier": 3 }, { "roleId": "955019200000000003", "multiplier": 2 }])
    );

    // roleDelete: the Rust cleanup, read back through the JS.
    let removed = role_deleted(&t, G, "955019200000000002", t.settings().get_settings_safe(G).await).await.expect("cleanup");
    assert_eq!(removed, 0);
    role_deleted(&t, G, "955019200000000006", t.settings().get_settings_safe(G).await).await.expect("cleanup");
    let back = run_node(JS, &["settings", G]);
    assert_eq!(back["noXpRoleIds"], json!(["955019200000000009"]));
    assert_eq!(back["xpBoostRoles"], json!([{ "roleId": "955019200000000001", "multiplier": 1.5 }, { "roleId": "955019200000000003", "multiplier": 2 }]));
    assert_eq!(reward_pairs(&level_roles(t.db(), G).await.expect("rows")), vec![(10, "955019200000000010".to_string())]);
    // And the JS cleanup, read back through Rust.
    let js = run_node(JS, &["roledelete", G, "955019200000000010"]);
    assert!(js.get("error").is_none(), "{js}");
    assert!(level_roles(t.db(), G).await.expect("rows").is_empty());
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_level_change_pipeline_one_owner_per_transition() {
    const G: &str = "955019000000000005";
    let Some(t) = db(&[G]).await else { return };
    enable(&t, G, SettingsPatch::new().set("levelRolesSyncOnDemotion", true)).await;
    let gid = GuildId::new(G.parse().unwrap_or(1));
    let u = "955019100000000051";
    let s = t.settings().get_settings_safe(G).await;
    let r = apply_xp_delta(&t, XpDelta::new(G, u, 4675.0)).await.expect("apply");
    assert_eq!((r.old_level, r.new_level), (0, 10));
    let m = member(G, u, &[], false);
    let out = handle_level_change(&t, LevelChange::of(gid, u, &r, s.clone(), "admin").member(Some(m.clone())).announce(false)).await;
    assert!(out.leveled_up, "one level-up for a multi-level jump");
    assert_eq!(out.roles, Some(RoleSync::default()), "no rewards configured");
    let again = handle_level_change(&t, LevelChange::of(gid, u, &r, s.clone(), "admin").member(Some(m.clone()))).await;
    assert!(!again.leveled_up, "a second writer of the same transition does nothing");
    assert_eq!(find_user_level(t.db(), G, u).await.expect("load").map(|x| x.level), Some(10));
    // A decrease re-syncs roles when demotion sync is on (offline: the role is not in the cache, so it fails).
    levelconfig::upsert_reward(&t, G, 5, "955019200000000055").await.expect("reward");
    let held = member(G, u, &["955019200000000055"], false);
    let down = subtract_xp(&t, G, u, 4000.0).await.expect("subtract").expect("row");
    let out = handle_level_change(&t, LevelChange::of(gid, u, &down, s, "admin").member(Some(held))).await;
    assert!(!out.leveled_up);
    let roles = out.roles.expect("re-synced");
    assert_eq!(roles.failed, vec![("955019200000000055".to_string(), "Role not found.".to_string())]);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_message_xp_awards_15_to_25_once_per_cooldown() {
    const G: &str = "955019000000000006";
    let Some(t) = db(&[G]).await else { return };
    enable(&t, G, SettingsPatch::new().set("xpCooldown", 60).set("noXpChannelIds", json!(["955019300000000099"]))).await;
    let gid = GuildId::new(G.parse().unwrap_or(1));
    let u = "955019100000000061";
    t.cache().del_one(&cooldown_key(G, u)).await;
    let m = member(G, u, &[], false);
    let s = t.settings().get_settings_safe(G).await;
    let exempt = ChannelRef { id: "955019300000000099".into(), ..Default::default() };
    assert!(matches!(message_xp(&t, s.clone(), gid, &m, Some(exempt), None, math_random).await.expect("ran"), MessageXp::Skipped("no-xp-channel")));
    let first = message_xp(&t, s.clone(), gid, &m, None, None, math_random).await.expect("ran");
    let MessageXp::Awarded { amount, change, level_change } = first else { panic!("awarded") };
    assert!((15..=25).contains(&amount));
    assert_eq!(change.xp, amount);
    assert!(level_change.is_none());
    assert!(matches!(message_xp(&t, s, gid, &m, None, None, math_random).await.expect("ran"), MessageXp::Skipped("cooldown")));
    let row = find_user_level(t.db(), G, u).await.expect("load").expect("row");
    assert_eq!((i64::from(row.xp), row.message_count), (amount, 1));
    t.cache().del_one(&cooldown_key(G, u)).await;
    t.reset_guilds(&[G]).await;
}

fn voice_guild(g: &str, channel: &str, users: &[&str]) -> Value {
    let mut guild = FakeGuild::new(g, "955019100000000999").channel(channel, 2, "voice", &[]);
    for u in users {
        guild = guild.member(u, &[]);
    }
    let mut json = guild.json();
    json["voice_states"] = Value::Array(
        users
            .iter()
            .map(|u| {
                json!({ "channel_id": channel, "user_id": u, "session_id": "s", "deaf": false, "mute": false, "self_deaf": false,
                    "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null })
            })
            .collect(),
    );
    json
}

#[tokio::test]
async fn db_voice_sessions_and_the_sweep_job_round_trip_with_the_js() {
    const G: &str = "955019000000000007";
    let Some(t) = db(&[G]).await else { return };
    if redis_url().is_none() {
        println!("skipped: TEST_REDIS_URL not set");
        return;
    }
    enable(&t, G, SettingsPatch::new().set("voiceXpEnabled", true).set("voiceXpRate", 5)).await;
    let (ch, a, b) = ("955019300000000071", "955019100000000071", "955019100000000072");
    t.cache().del(&[voice_key(G, a), voice_key(G, b)]).await;
    t.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(sweep_dedupe_key(G)), ..Default::default() }).await;

    // JS seeds the sessions and the sweep job; Rust reads them.
    let js = run_node(JS, &["seedvoice", G, ch, a, b]);
    assert!(js.get("error").is_none(), "{js}");
    let now = t.clock().now_ms() as f64;
    let session = parse_voice_session(t.cache().get(&voice_key(G, a)).await.as_ref(), now).expect("session");
    assert_eq!(session.channel_id.as_deref(), Some(ch));
    let job = t.app.scheduler().get_job(&sweep_dedupe_key(G)).await.expect("job");
    assert_eq!(job.r#type, "levels.voiceSweep");
    assert_eq!(job.payload, json!({}));
    assert_eq!(job.max_attempts, 5);

    // Rust seeds; the JS reads the session and the job the same way.
    t.cache().del(&[voice_key(G, a), voice_key(G, b)]).await;
    t.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(sweep_dedupe_key(G)), ..Default::default() }).await;
    t.app.set_discord(offline_discord_as(&[voice_guild(G, ch, &[a, b])]));
    svc::set_ready_at(&t, t.clock().now_ms() - 3_600_000);
    seed_voice_sessions_for(&t, GuildId::new(G.parse().unwrap_or(1)), t.settings().get_settings_safe(G).await).await;
    let back = run_node(JS, &["voiceget", G, a]);
    assert_eq!(back["parsed"]["channelId"], ch, "{back}");
    assert_eq!(back["raw"]["start"], back["parsed"]["start"]);
    let job = run_node(JS, &["job", &sweep_dedupe_key(G)]);
    assert_eq!(job["type"], "levels.voiceSweep", "{job}");
    assert_eq!(job["payload"], json!({}));

    // A session older than 10 minutes is paid on the sweep, which then reschedules itself.
    let start = t.clock().now_ms() - 11 * 60_000 - 5_000;
    t.cache().set(&voice_key(G, a), &voice_session_json(start as f64, ch), VOICE_KEY_TTL_SECONDS).await;
    let outcome = run_voice_sweep(&t, G).await.expect("swept");
    assert!(matches!(outcome, JobOutcome::Reschedule { payload: None, .. }));
    let row = find_user_level(t.db(), G, a).await.expect("load").expect("paid");
    assert_eq!((row.xp, row.voice_minutes), (55, 11));
    let rotated = parse_voice_session(t.cache().get(&voice_key(G, a)).await.as_ref(), t.clock().now_ms() as f64).expect("rotated");
    assert_eq!(rotated.start, (start + 11 * 60_000) as f64, "the sub-minute remainder is carried");

    // Leaving pays the rest and deletes the key.
    let s = t.settings().get_settings(G).await.expect("settings");
    let gone = VoiceView::default();
    t.cache().set(&voice_key(G, a), &voice_session_json((t.clock().now_ms() - 3 * 60_000) as f64, ch), VOICE_KEY_TTL_SECONDS).await;
    assert!(!reconcile_voice_member(&t, GuildId::new(G.parse().unwrap_or(1)), member(G, a, &[], false), VoiceSource::View(gone), s, t.clock().now_ms(), false).await);
    assert_eq!(t.cache().get(&voice_key(G, a)).await, None);
    assert_eq!(find_user_level(t.db(), G, a).await.expect("load").map(|r| r.voice_minutes), Some(14));
    // Voice XP off: the sweep ends.
    t.settings().update_settings(G, SettingsPatch::new().set("voiceXpEnabled", false)).await.expect("saved");
    assert_eq!(run_voice_sweep(&t, G).await.expect("swept"), JobOutcome::Done);
    t.cache().del(&[voice_key(G, a), voice_key(G, b)]).await;
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_rewards_provider_grant_and_take() {
    const G: &str = "955019000000000008";
    let Some(t) = db(&[G]).await else { return };
    t.install(registry());
    enable(&t, G, SettingsPatch::new()).await;
    let gid = GuildId::new(G.parse().unwrap_or(1));
    let u = "955019100000000081";
    let m = member(G, u, &[], false);
    let r = t.rewards().take(&t, RewardKind::Xp, RewardArgs::new(gid, u, 10)).await;
    assert_eq!(r.reason.as_deref(), Some("insufficient"));
    let r = t.rewards().grant(&t, RewardKind::Xp, RewardArgs::new(gid, u, 300).member(Some(m.clone()))).await;
    assert_eq!((r.ok, r.total), (true, Some(300)));
    assert_eq!(find_user_level(t.db(), G, u).await.expect("load").map(|x| x.level), Some(2), "the level change ran");
    let r = t.rewards().take(&t, RewardKind::Xp, RewardArgs::new(gid, u, 1000).member(Some(m))).await;
    assert_eq!((r.ok, r.total), (true, Some(0)), "XP clamps at 0");
    let r = t.rewards().take(&t, RewardKind::Xp, RewardArgs::new(gid, u, 1)).await;
    assert_eq!(r.reason.as_deref(), Some("insufficient"));
    t.reset_guilds(&[G]).await;
}

// ── The commands through the router ───────────────────────────────────────────

const ADMIN: &str = "955019100000000900";
const ADMIN_ROLE: &str = "955019200000000900";

/// A user option with its resolved user (as Discord sends it).
fn with_user(b: FakeInteraction, name: &str, id: &str) -> FakeInteraction {
    let mut b = b.option(name, option_type::USER, json!(id));
    b.json_mut()["data"]["resolved"]["users"][id] =
        json!({ "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "global_name": null, "bot": false });
    b
}

/// A role option with its resolved role.
fn with_role(b: FakeInteraction, name: &str, id: &str, position: u16) -> FakeInteraction {
    let mut b = b.option(name, option_type::ROLE, json!(id));
    b.json_mut()["data"]["resolved"]["roles"][id] = json!({
        "id": id, "name": format!("role{position}"), "color": 0, "hoist": false, "position": position, "permissions": "0",
        "managed": false, "mentionable": false, "flags": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    });
    b
}

fn admin(g: &str) -> FakeInteraction {
    FakeInteraction::slash("levelconfig").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD | P::ADMINISTRATOR)
}

fn fields(call: &bot_core::interaction::RecordedCall) -> Vec<(String, String)> {
    arr(&call.body["embeds"][0]["fields"])
        .iter()
        .map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into()))
        .collect()
}

#[tokio::test]
async fn db_a_rank_level_up_template_shows_the_real_position() {
    const G: &str = "955019000000000012";
    let Some(t) = db(&[G]).await else { return };
    enable(&t, G, SettingsPatch::new().set("levelUpMessage", "{player} is #{rank} with {xp} XP").set("levelUpUseEmbed", false)).await;
    let (a, b) = ("955019100000000121", "955019100000000122");
    set_xp(&t, G, a, 5000.0).await.expect("set");
    set_xp(&t, G, b, 1200.0).await.expect("set");
    let s = t.settings().get_settings(G).await.expect("settings");
    let m = member(G, b, &[], false);
    let gid = GuildId::new(G.parse().unwrap_or(1));
    let announce = Announce { guild_id: gid, member: &m, level: 5, xp: 1200, settings: &s, source: "message", source_channel_id: None };
    let p = level_up_payload_for(&t, &announce, false).await.expect("payload");
    assert_eq!(p.content.as_deref(), Some(format!("<@{b}> is #2 with 1,200 XP").as_str()));
    assert_eq!(p.to_json()["allowedMentions"], json!({ "users": [b] }), "only the levelled member is pinged");
    let dm = level_up_payload_for(&t, &announce, true).await.expect("payload");
    assert_eq!(dm.components.len(), 1, "the DM carries the disabled Sent from button");
    // Admin changes never announce, and CURRENT without a source channel stays silent (offline: nothing is sent).
    assert!(!announce_level_up(&t, Announce { source: "admin", ..announce.clone() }).await);
    assert!(!announce_level_up(&t, announce).await);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_levelconfig_settings_subcommands_reply_from_the_saved_row() {
    const G: &str = "955019000000000009";
    let Some(t) = db(&[G]).await else { return };
    t.install(registry());
    t.settings().ensure_guild_settings(G).await.expect("row");

    route_interaction(&t, admin(G).subcommand("enable").build()).await;
    let c = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!c[0].ephemeral());
    assert_eq!(c[1].embed_title(), Some("✅  Levels Enabled"));
    assert_eq!(c[1].embed_description(), Some("Members now earn **15–25 XP** per message (once every 60s). Configure with `/levelconfig`."));

    let run = |b: FakeInteraction| {
        let t = &t;
        async move {
            t.responses.clear();
            route_interaction(t, b.build()).await;
            t.responses.calls().last().cloned().expect("answered")
        }
    };
    let c = run(admin(G).subcommand("range").option("min", option_type::INTEGER, json!(30)).option("max", option_type::INTEGER, json!(10))).await;
    assert_eq!(c.embed_title(), Some("❌  Invalid Range"));
    let c = run(admin(G).subcommand("range").option("min", option_type::INTEGER, json!(10)).option("max", option_type::INTEGER, json!(30))).await;
    assert_eq!(c.embed_description(), Some("Members earn **10–30 XP** per message (before multipliers). MEE6 uses 15–25."));
    let c = run(admin(G).subcommand("multiplier").option("value", option_type::NUMBER, json!(1.256))).await;
    assert_eq!(c.embed_description(), Some("Server-wide XP multiplier: **×1.26**."));
    let c = run(admin(G).subcommand("voicexp").option("enabled", option_type::BOOLEAN, json!(true)).option("rate", option_type::INTEGER, json!(7))).await;
    assert_eq!(
        c.embed_description(),
        Some("Voice XP **on** at **7 XP/minute**. No XP while deafened, alone, in the AFK channel or in a no-XP channel.")
    );
    let c = run(admin(G).subcommand("message").option("text", option_type::STRING, json!("  GG {player}! {bogus}  "))).await;
    assert_eq!(c.embed_title(), Some("✅  Level-up Message Updated"));
    assert!(c.embed_description().unwrap_or_default().starts_with("Preview (sample values):\nGG <@955019100000000900>! {bogus}"));
    assert_eq!(fields(&c)[0], ("⚠️ Unknown variables".to_string(), "`{bogus}`\nThey will appear exactly as typed.".to_string()));
    let c = run(admin(G).group("noxp", "role").option("role", option_type::ROLE, json!("955019200000000091"))).await;
    assert_eq!(c.embed_description(), Some("<@&955019200000000091> added to the no-XP roles."));
    let c = run(admin(G).group("noxp", "role").option("role", option_type::ROLE, json!("955019200000000091"))).await;
    assert_eq!((c.embed_title(), c.embed_description()), (Some("❌  Not Saved"), Some("Already on the no-XP list.")));
    let c = run(admin(G).group("boost", "add").option("role", option_type::ROLE, json!("955019200000000092")).option("multiplier", option_type::NUMBER, json!(1.5)))
        .await;
    assert_eq!(c.embed_title(), Some("✅  XP Boost Saved"));
    let c = run(admin(G).group("boost", "list")).await;
    assert_eq!(c.embed_description(), Some("<@&955019200000000092> — ×1.5 ⚠️ deleted role"));
    let c = run(admin(G).subcommand("status")).await;
    assert_eq!(c.embed_title(), Some("⭐ Levels Configuration"));
    assert!(fields(&c).contains(&("Multiplier".to_string(), "×1.26".to_string())));
    let c = run(admin(G).subcommand("announce").option("mode", option_type::STRING, json!("DM")).option("embed", option_type::BOOLEAN, json!(true))).await;
    assert_eq!(c.embed_description(), Some("Mode: **Private message**. Format: **embed**."));
    let c = run(admin(G).subcommand("announce").option("mode", option_type::STRING, json!("CHANNEL"))).await;
    assert_eq!(c.embed_title(), Some("❌  Channel Required"));
    let c = run(admin(G).subcommand("disable")).await;
    assert_eq!(c.embed_title(), Some("✅  Levels Disabled"));

    // The JS reads what the commands saved.
    let back = run_node(JS, &["settings", G]);
    assert_eq!((back["xpMin"].clone(), back["xpMax"].clone(), back["xpMultiplier"].clone()), (json!(10), json!(30), json!(1.26)));
    assert_eq!((back["voiceXpRate"].clone(), back["levelUpMode"].clone(), back["levelUpUseEmbed"].clone()), (json!(7), json!("DM"), json!(true)));
    assert_eq!(back["levelUpMessage"], "GG {player}! {bogus}");
    assert_eq!(back["noXpRoleIds"], json!(["955019200000000091"]));
    assert_eq!(back["xpBoostRoles"], json!([{ "roleId": "955019200000000092", "multiplier": 1.5 }]));
    assert_eq!(back["xpEnabled"], false);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_levelconfig_xp_group_rank_and_leaderboard() {
    const G: &str = "955019000000000010";
    let Some(t) = db(&[G]).await else { return };
    t.install(registry());
    enable(&t, G, SettingsPatch::new()).await;
    let u = "955019100000000101";
    let xp = |sub: &str| admin(G).group("xp", sub);
    let user_opt = |b: FakeInteraction, id: &str| with_user(b, "user", id);
    let run = |b: FakeInteraction| {
        let t = &t;
        async move {
            t.responses.clear();
            route_interaction(t, b.build()).await;
            t.responses.calls()
        }
    };
    let c = run(user_opt(xp("view"), u)).await;
    assert!(c[0].ephemeral(), "the xp group answers privately");
    assert_eq!(c[1].embed_title(), Some("ℹ️  No Data"));
    let c = run(user_opt(xp("remove"), u).option("amount", option_type::INTEGER, json!(5))).await;
    assert_eq!(c[1].embed_title(), Some("ℹ️  Nothing to Remove"));
    let c = run(user_opt(xp("set"), u).option("amount", option_type::INTEGER, json!(0))).await;
    assert_eq!(c[1].embed_description(), Some(format!("<@{u}> already has 0 XP.").as_str()));
    let c = run(user_opt(xp("set"), u).option("amount", option_type::INTEGER, json!(1150))).await;
    assert_eq!(c[1].embed_title(), Some("❌  Not Saved"), "offline the member cannot be fetched");
    set_xp(&t, G, u, 1150.0).await.expect("set");
    set_xp(&t, G, "955019100000000102", 2000.0).await.expect("set");
    let c = run(user_opt(xp("remove"), u).option("amount", option_type::INTEGER, json!(100))).await;
    assert_eq!(c[1].embed_title(), Some("✅  XP Removed"));
    assert_eq!(c[1].embed_description(), Some(format!("Removed **100 XP** from <@{u}>. Total: **1,050 XP** (level 4).\nLevel 0 → 4.").as_str()));
    let c = run(user_opt(xp("view"), u)).await;
    assert_eq!(c[1].embed_title(), Some(format!("XP — user{u}").as_str()));
    let f = fields(&c[1]);
    assert_eq!(&f[..4], &[
        ("Level".to_string(), "4".to_string()),
        ("Total XP".to_string(), "1,050".to_string()),
        ("Rank".to_string(), "#2".to_string()),
        ("Progress".to_string(), "280 / 380".to_string()),
    ]);
    let c = run(user_opt(xp("reset"), u)).await;
    assert_eq!(c[1].embed_description(), Some(format!("Wiped XP and level for <@{u}> (was level 4).").as_str()));

    // /rank and /leaderboard.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("rank").guild(G).user(u).build()).await;
    let c = t.responses.calls();
    assert_eq!(c[1].embed_title(), Some("ℹ️  No XP Yet"));
    assert_eq!(c[1].embed_description(), Some("member hasn't earned any XP in this server yet."));
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("rank").guild(G).user("955019100000000102").build()).await;
    let c = t.responses.calls();
    assert_eq!(c.len(), 2, "{c:?}");
    assert!(c[1].body.to_string().contains("rank.png"), "the card is attached: {}", c[1].body);
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("leaderboard").guild(G).user(u).build()).await;
    let c = t.responses.calls();
    let last = c.last().expect("answered");
    assert_eq!(last.embed_description(), Some("🥇 <@955019100000000102> — Level **6** · 2,000 XP"));
    assert!(last.body.to_string().contains("Top 1 of 1 ranked members"), "{}", last.body);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_levelconfig_role_rewards_with_a_cached_guild() {
    const G: &str = "955019000000000011";
    let Some(t) = db(&[G]).await else { return };
    t.install(registry());
    enable(&t, G, SettingsPatch::new()).await;
    let guild = FakeGuild::new(G, "955019100000000999")
        .role("955019200000000111", 3, P::MANAGE_ROLES)
        .member(TEST_BOT_ID, &["955019200000000111"])
        .role("955019200000000112", 1, P::empty())
        .role("955019200000000113", 5, P::empty())
        .role(ADMIN_ROLE, 4, P::ADMINISTRATOR)
        .member(ADMIN, &[ADMIN_ROLE]);
    t.app.set_discord(offline_discord_as(&[guild.json()]));
    let add = |level: i64, role: &str, position: u16| {
        with_role(admin(G).group("role", "add").option("level", option_type::INTEGER, json!(level)), "role", role, position)
            .option("sync", option_type::BOOLEAN, json!(false))
    };
    let run = |b: FakeInteraction| {
        let t = &t;
        async move {
            t.responses.clear();
            route_interaction(t, b.build()).await;
            t.responses.calls().last().cloned().expect("answered")
        }
    };
    let c = run(add(900, "955019200000000112", 1)).await;
    assert_eq!(c.embed_title(), Some("❌  Level Too High"));
    assert_eq!(
        c.embed_description(),
        Some("Level 900 needs more than 1,000,000,000 XP, so nobody can reach it. The highest reachable level is 838.")
    );
    let c = run(add(5, "955019200000000113", 5)).await;
    assert_eq!((c.embed_title(), c.embed_description()), (Some("❌  Can't Use This Role"), Some("That role is above my highest role.")));
    let c = run(add(5, "955019200000000112", 1)).await;
    assert_eq!(c.embed_description(), Some("Members who reach level **5** get <@&955019200000000112>."));
    let c = run(add(7, "955019200000000112", 1)).await;
    assert_eq!(c.embed_description(), Some("<@&955019200000000112> is already the reward for level 5. Remove that first."));
    let c = run(admin(G).group("role", "list")).await;
    assert_eq!(c.embed_title(), Some("🎁 Level Role Rewards"));
    assert_eq!(c.embed_description(), Some("**Level 5** → <@&955019200000000112>"));
    // Autocomplete lists the reward levels by prefix.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::autocomplete("levelconfig").guild(G).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::ADMINISTRATOR).group("role", "remove").focused("level", "5").build())
        .await;
    let choices = t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(json!(choices), json!([{ "name": "Level 5 → role955019200000000112", "value": 5 }]), "{choices:?}");
    let c = run(admin(G).group("role", "remove").option("level", option_type::INTEGER, json!(5))).await;
    assert_eq!(c.embed_title(), Some("✅  Role Reward Removed"));
    let c = run(admin(G).group("role", "remove").option("level", option_type::INTEGER, json!(5))).await;
    assert_eq!(c.embed_description(), Some("No role reward is set for level 5."));
    // A role sync with no rewards answers at once and leaves no bulk lock behind.
    let c = run(admin(G).group("role", "sync")).await;
    assert_eq!(c.embed_title(), Some("ℹ️  No Role Rewards"));
    assert!(!is_bulk_running(&t, G));
    let _ = UserId::new(1);
    t.reset_guilds(&[G]).await;
}

// ── More behaviour ────────────────────────────────────────────────────────────

#[tokio::test]
async fn db_concurrent_xp_writers_never_lose_xp_and_one_writer_owns_each_transition() {
    const G: &str = "955019000000000013";
    let Some(t) = db(&[G]).await else { return };
    t.settings().ensure_guild_settings(G).await.expect("row");
    let u = "955019100000000131";
    let writes: Vec<_> = (0..20)
        .map(|_| {
            let app = t.app.clone();
            tokio::spawn(async move { apply_xp_delta(&app, XpDelta::new(G, u, 25.0)).await })
        })
        .collect();
    for w in writes {
        w.await.expect("joined").expect("applied");
    }
    let row = find_user_level(t.db(), G, u).await.expect("load").expect("row");
    assert_eq!(row.xp, 500, "no write is lost");
    let owners: Vec<_> = (0..8)
        .map(|_| {
            let pool = t.pool.clone();
            tokio::spawn(async move { set_stored_level(&pool, G, u, 500, 3).await })
        })
        .collect();
    let mut won = 0;
    for o in owners {
        if o.await.expect("joined").expect("ran") {
            won += 1;
        }
    }
    assert_eq!(won, 1, "exactly one writer owns the transition");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn sync_level_roles_adds_before_removes_and_reports_failures() {
    let g = "955009000000000006";
    let (bot_role, low, high) = ("955009200000000061", "955009200000000062", "955009200000000063");
    let guild = FakeGuild::new(g, "955009100000000999")
        .role(bot_role, 5, P::MANAGE_ROLES)
        .member(TEST_BOT_ID, &[bot_role])
        .role(low, 1, P::empty())
        .role(high, 2, P::empty())
        .member("955009100000000061", &[low]);
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[guild]);
    let s = t.settings().update_settings(g, SettingsPatch::new().set("levelRolesStack", false)).await.expect("memory store");
    let m = member(g, "955009100000000061", &[low], false);
    let rewards = vec![(1, low.to_string()), (5, high.to_string()), (9, "955009200000000069".to_string())];
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let r = sync_level_roles(&t, gid, &m, 5, Some(&s), SyncOptions { rewards: Some(rewards.clone()), ..Default::default() }).await;
    // Offline every REST call fails: the order of the failures is the order of the calls (add first, then remove).
    let order: Vec<&str> = r.failed.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(order, vec![high, low]);
    assert!(r.added.is_empty() && r.removed.is_empty());
    // A role that is not in the guild is refused before any call; demotion sync removes unearned roles.
    let holder = member(g, "955009100000000061", &["955009200000000069"], false);
    let r = sync_level_roles(&t, gid, &holder, 0, Some(&s), SyncOptions { sync_down: true, rewards: Some(rewards), ..Default::default() }).await;
    assert_eq!(r.failed, vec![("955009200000000069".to_string(), "Role not found.".to_string())]);
}

#[tokio::test]
async fn voice_others_are_rechecked_only_with_one_or_two_listeners() {
    let g = "955009000000000007";
    let ch = "955009300000000071";
    let (a, b, c) = ("955009100000000071", "955009100000000072", "955009100000000073");
    for (users, rechecked) in [(vec![a, b], true), (vec![a, b, c], false)] {
        let t = TestApp::new().with_guilds([g]);
        t.app.set_discord(offline_discord_as(&[voice_guild(g, ch, &users)]));
        t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", true).set("voiceXpEnabled", true)).await.expect("memory store");
        svc::set_ready_at(&t, 1);
        let ev = VoiceStateUpdate {
            guild_id: GuildId::new(g.parse().unwrap_or(1)),
            old_state: None,
            new_state: voice_state(g, a, Some(ch), false, false),
            member: member(g, a, &[], false),
            change: VoiceChange::Join,
        };
        let task = voice_state_xp(&t, t.settings().get_settings_safe(g).await, &ev).await;
        assert_eq!(task.is_some(), rechecked, "{} listeners", users.len());
        if let Some(task) = task {
            task.await.expect("re-check ran");
            assert!(t.cache_backend.raw(&voice_key(g, b)).is_some(), "the partner's session started");
        } else {
            assert!(t.cache_backend.raw(&voice_key(g, b)).is_none(), "busier channels are skipped");
        }
        assert!(t.cache_backend.raw(&voice_key(g, a)).is_some());
        assert_eq!(t.job_store.all().len(), 1, "the join seeds the sweep");
    }
}

fn recorded_confirm_id(t: &DbTestApp) -> Option<String> {
    t.responses.calls().iter().rev().find_map(|c| {
        arr(&c.body["components"])
            .iter()
            .flat_map(|row| arr(&row["components"]))
            .find_map(|b| b["custom_id"].as_str().filter(|id| id.starts_with("confirm:") && id.ends_with(":yes")).map(String::from))
    })
}

#[tokio::test]
async fn db_xp_give_to_a_role_confirms_gives_in_bulk_and_releases_the_lock() {
    const G: &str = "955019000000000014";
    let Some(t) = db(&[G]).await else { return };
    t.install(registry());
    enable(&t, G, SettingsPatch::new()).await;
    let role = "955019200000000141";
    let guild = FakeGuild::new(G, "955019100000000999")
        .role(ADMIN_ROLE, 4, P::ADMINISTRATOR)
        .member(ADMIN, &[ADMIN_ROLE])
        .role(role, 1, P::empty())
        .member("955019100000000141", &[role])
        .member("955019100000000142", &[role])
        .member(TEST_BOT_ID, &[role]);
    t.app.set_discord(offline_discord_as(&[guild.json()]));
    let give = || {
        let mut b = admin(G).group("xp", "give").option("target", option_type::MENTIONABLE, json!(role)).option("amount", option_type::INTEGER, json!(100));
        b.json_mut()["data"]["resolved"]["roles"][role] = json!({
            "id": role, "name": "r", "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false, "mentionable": false, "flags": 0,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
        });
        b.build()
    };
    for confirm in [false, true] {
        t.responses.clear();
        let app = t.app.clone();
        let interaction = give();
        let route = tokio::spawn(async move { route_interaction(&app, interaction).await });
        let mut waited = 0;
        while t.app.collectors().is_empty() && waited < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            waited += 1;
        }
        let yes = recorded_confirm_id(&t).expect("the confirm prompt was shown");
        assert!(is_bulk_running(&t, G), "the bulk lock is held while the prompt is open");
        let id = if confirm { yes } else { yes.replace(":yes", ":no") };
        route_interaction(&t, FakeInteraction::button(&id).guild(G).user(ADMIN).message_id("1000000000000000100").build()).await;
        route.await.expect("routed");
        let last = t.responses.calls().last().cloned().expect("answered");
        if confirm {
            assert_eq!(last.embed_title(), Some("✅  XP Given"));
            assert_eq!(last.embed_description(), Some("Gave **100 XP** to **2** members of <@&955019200000000141>. 2 levelled up."), "bots are left out");
        } else {
            assert_eq!((last.embed_title(), last.embed_description()), (Some("ℹ️  Cancelled"), Some("No XP was given.")));
            assert!(find_user_level(t.db(), G, "955019100000000141").await.expect("load").is_none());
        }
        let mut waited = 0;
        while is_bulk_running(&t, G) && waited < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            waited += 1;
        }
        assert!(!is_bulk_running(&t, G), "the bulk lock is released");
    }
    assert_eq!(find_user_level(t.db(), G, "955019100000000142").await.expect("load").map(|r| (r.xp, r.level)), Some((100, 1)));
    assert!(find_user_level(t.db(), G, TEST_BOT_ID).await.expect("load").is_none());
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_role_add_backfills_in_the_background_and_noxp_channel_leaves_the_announcement_channel() {
    const G: &str = "955019000000000015";
    let Some(t) = db(&[G]).await else { return };
    t.install(registry());
    enable(&t, G, SettingsPatch::new().set("levelUpChannelId", "955019300000000151")).await;
    let (bot_role, reward) = ("955019200000000151", "955019200000000152");
    let guild = FakeGuild::new(G, "955019100000000999")
        .role(bot_role, 3, P::MANAGE_ROLES)
        .member(TEST_BOT_ID, &[bot_role])
        .role(ADMIN_ROLE, 4, P::ADMINISTRATOR)
        .member(ADMIN, &[ADMIN_ROLE])
        .role(reward, 1, P::empty())
        .member("955019100000000151", &[]);
    t.app.set_discord(offline_discord_as(&[guild.json()]));
    set_xp(&t, G, "955019100000000151", 500.0).await.expect("set");
    set_xp(&t, G, "955019100000000152", 50.0).await.expect("set");
    set_xp(&t, G, "955019100000000153", 300.0).await.expect("set");
    t.responses.clear();
    let add = with_role(admin(G).group("role", "add").option("level", option_type::INTEGER, json!(1)), "role", reward, 1);
    route_interaction(&t, add.build()).await;
    let c = t.responses.calls();
    assert_eq!(
        c[1].embed_description(),
        Some("Members who reach level **1** get <@&955019200000000152>.\nGiving it to members already at level 1+ in the background…")
    );
    let mut waited = 0;
    while is_bulk_running(&t, G) && waited < 400 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        waited += 1;
    }
    assert!(!is_bulk_running(&t, G));
    let report = t.responses.calls().into_iter().find(|c| c.method == "followUp").expect("the summary follow-up");
    assert_eq!(report.embed_title(), Some("ℹ️  Role Sync Finished"));
    assert_eq!(
        report.embed_description(),
        Some("Role sync for level 1: 0 updated, 1 failed, 1 no longer in the server."),
        "member 151 is cached (the offline role add fails), 153 is gone; 152 is below level 1"
    );
    assert!(!report.ephemeral());

    // R002: `noxp channel` never touches the announcement channel.
    t.responses.clear();
    route_interaction(&t, admin(G).group("noxp", "channel").option("channel", option_type::CHANNEL, json!("955019300000000159")).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title().map(String::from)).as_deref(), Some("✅  No-XP Channels"));
    let s = t.settings().get_settings(G).await.expect("settings");
    assert_eq!(s.level_up_channel_id.as_deref(), Some("955019300000000151"));
    assert_eq!(s.no_xp_channel_ids, vec!["955019300000000159".to_string()]);
    t.reset_guilds(&[G]).await;
}

// ── Review fixes ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn ready_at_without_a_ready_time_is_the_current_now() {
    // `guild.client.readyTimestamp ?? now` is evaluated afresh: before the client is ready, every call gets its own now.
    let t = TestApp::new();
    assert_eq!(ready_at(&t, 1_000), 1_000);
    assert_eq!(ready_at(&t, 5_000), 5_000, "the first now is not remembered");
    set_ready_at(&t, 2_000);
    assert_eq!(ready_at(&t, 9_000), 2_000);
}

#[tokio::test]
async fn announce_level_up_fetches_an_uncached_custom_channel() {
    // announceLevelUp: `guild.channels.cache.get(id) ?? await guild.channels.fetch(id)`.
    let g = "955009000000000031";
    let ch = "955009300000000311";
    let user = "955009100000000311";
    let guild = FakeGuild::new(g, "955009100000000999")
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS)
        .member(TEST_BOT_ID, &[])
        .member(user, &[]);
    let rest = bot_core::testing::FakeRest::new();
    rest.on(
        "GET",
        &format!("/channels/{ch}"),
        200,
        json!({ "id": ch, "guild_id": g, "type": 0, "name": "levels", "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": null, "flags": 0 }),
    );
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(std::slice::from_ref(&guild), &rest);
    let s = t
        .settings()
        .update_settings(g, SettingsPatch::new().set("xpEnabled", true).set("levelUpMode", "CHANNEL").set("levelUpChannelId", ch).set("levelUpUseEmbed", false))
        .await
        .expect("memory store");
    let m = member(g, user, &[], false);
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let sent = announce_level_up(&t, Announce { guild_id: gid, member: &m, level: 2, xp: 300, settings: &s, source: "message", source_channel_id: None }).await;
    assert!(sent, "the fetched channel is usable");
    let posts = rest.calls_to("POST", &format!("/channels/{ch}/messages"));
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["content"], json!(format!("GG <@{user}>, you just advanced to level 2!")));
}

#[tokio::test]
async fn fetch_members_asks_the_gateway_for_uncached_members() {
    // leaderboard fetchMissingNames / runBulkRoleSync: `guild.members.fetch({ user: ids })`, one gateway request per
    // chunk of ids, never one REST call per member.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let g = "955009000000000032";
    let cached = "955009100000000321";
    let guild = FakeGuild::new(g, "955009100000000999").member(TEST_BOT_ID, &[]).member(cached, &[]);
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(std::slice::from_ref(&guild), &rest);
    let requests = Arc::new(AtomicUsize::new(0));
    let (app, count) = (t.app.clone(), requests.clone());
    bot_core::guild_members::set_chunk_sender(
        &t,
        Some(Arc::new(move |gid, ids, nonce| {
            count.fetch_add(1, Ordering::SeqCst);
            let members: Vec<Value> = ids
                .iter()
                .filter(|id| id.get() != 955009100000000329) // a member who left
                .map(|id| json!({ "user": { "id": id.to_string(), "username": format!("u{id}"), "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 }))
                .collect();
            let event: serenity::all::GuildMembersChunkEvent =
                serde_json::from_value(json!({ "guild_id": gid.to_string(), "members": members, "chunk_index": 0, "chunk_count": 1, "nonce": nonce, "not_found": [] }))
                    .expect("chunk json");
            bot_core::guild_members::deliver_members_chunk(&app.ext::<bot_core::guild_members::MemberChunks>(), &event);
            Box::pin(async { true })
        })),
    );
    let ids: Vec<String> = [cached, "955009100000000322", "955009100000000323", "955009100000000329"].iter().map(|s| s.to_string()).collect();
    let got = fetch_members(&t, GuildId::new(g.parse().unwrap_or(1)), &ids, 5_000).await;
    let mut names: Vec<&str> = got.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, vec![cached, "955009100000000322", "955009100000000323"]);
    assert_eq!(requests.load(Ordering::SeqCst), 1, "one gateway request for the uncached ids");
    assert!(rest.calls_to("GET", "/guilds/*/members/*").is_empty(), "no REST call per member");
}
