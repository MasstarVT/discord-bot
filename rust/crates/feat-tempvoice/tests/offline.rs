//! Offline tests: golden checks against JS-produced fixtures, the ported pure tests of `test/temp-voice.test.js` and
//! `test/voice-roles.test.js`, the voice-roles flows against a fake REST and an offline serenity cache, and the
//! component handlers' stale-id answers through the router.

mod common;

use std::sync::atomic::Ordering;

use bot_core::router::route_interaction;
use bot_core::settings::MemorySettingsStore;
use bot_core::testing::{FakeInteraction, TestApp, load_golden};
use common::*;
use feat_tempvoice::temp_voice_service::{self as tv, ClaimInput, ClaimStatus, Msg, MsgParams, NameContext, NameFormatCheck, OkParams, TargetInput};
use feat_tempvoice::voice_role_service::{self as vr, ApplyOptions, ApplyStatus};
use serde_json::{Map, Value, json};
use serenity::all::Permissions;

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_tempvoice::register(&mut reg);
    reg
}

fn golden() -> Value {
    load_golden("feat-tempvoice/golden.json")
}

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["voice-hub", "voicerole", "voice"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "temp-voice");
    bot_core::testing::assert_plugin_matches_golden(&reg, "voice-roles");
}

#[test]
fn constants_keys_and_raw_sql() {
    let g = golden();
    assert_eq!(tv::OWNER_PERMS, 17_827_088);
    assert_eq!(tv::BOT_VOICE_PERMS, 17_910_800);
    assert_eq!(tv::BOT_TEXT_PERMS.to_string(), g["botTextPerms"].as_str().unwrap());
    assert_eq!(tv::cleanup_key("123456789012345678"), g["keys"]["cleanup"]);
    assert_eq!(tv::rename_key("123456789012345678"), g["keys"]["rename"]);
    assert_eq!(tv::MAX_HUB_ROLE_LIST, 10);
    assert_eq!(tv::HUB_CHANNEL_NAME, "➕ Join to Create");
    assert_eq!(tv::REQUIRED_BOT_PERMS, ["ManageChannels", "ManageRoles", "MoveMembers", "ViewChannel", "Connect"]);
    assert_eq!(tv::RAW_SQL.len(), 2);
    for (name, sql, _) in tv::RAW_SQL {
        assert!(!sql.to_lowercase().contains("now()"), "{name}");
        assert!(sql.contains("FOR UPDATE"), "{name}");
    }
    assert_eq!(vr::ADD_REASON, g["reasons"]["add"]);
    assert_eq!(vr::REMOVE_REASON, g["reasons"]["remove"]);
}

// ── 1-9: pure helpers ─────────────────────────────────────────────────────────

#[test]
fn t01_lowest_free_index() {
    assert_eq!(tv::lowest_free_index(&[]), 1);
    assert_eq!(tv::lowest_free_index(&[1, 2, 4]), 3);
    assert_eq!(tv::lowest_free_index(&[2, 3]), 1);
    assert_eq!(tv::lowest_free_index(&[1, 1, 2]), 3);
    assert_eq!(tv::lowest_free_index(&[0, -1]), 1);
}

#[test]
fn t02_normalize_channel_name() {
    assert_eq!(tv::normalize_channel_name(Some("  a\n b  ")).as_deref(), Some("a b"));
    assert_eq!(tv::normalize_channel_name(Some("   ")), None);
    assert_eq!(tv::normalize_channel_name(None), None);
    assert_eq!(tv::normalize_channel_name(Some(&"x".repeat(150))).unwrap().len(), 100);
    let cut = tv::normalize_channel_name(Some(&format!("{}😀yyy", "x".repeat(99)))).unwrap();
    assert_eq!(cut, format!("{}😀", "x".repeat(99)));
    assert_eq!(tv::normalize_channel_name(Some(&"😀".repeat(150))).unwrap().chars().count(), 100);
    for case in golden()["normalize"].as_array().unwrap() {
        let input = case["input"].as_str();
        assert_eq!(tv::normalize_channel_name(input).map(Value::from).unwrap_or(Value::Null), case["out"], "{input:?}");
    }
}

#[test]
fn t03_text_channel_name() {
    assert_eq!(tv::text_channel_name("#1 - Bob's Channel", 1), "1-bobs-channel");
    assert_eq!(tv::text_channel_name("🔥🔥", 3), "voice-3");
    assert_eq!(tv::text_channel_name("Ünïcode  Name!!", 2), "ünïcode-name");
    assert_eq!(tv::text_channel_name("  --a--  ", 4), "a");
    assert!(tv::text_channel_name(&"a".repeat(300), 1).len() <= 100);
    for case in golden()["textNames"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(tv::text_channel_name(input, 7), case["out"].as_str().unwrap(), "{input:?}");
    }
}

#[test]
fn t04_parse_limit_input() {
    assert_eq!(tv::parse_limit_input("5"), Some(5));
    assert_eq!(tv::parse_limit_input(" 0 "), Some(0));
    assert_eq!(tv::parse_limit_input("99"), Some(99));
    for bad in ["-1", "1.5", "099", "", "abc"] {
        assert_eq!(tv::parse_limit_input(bad), None, "{bad}");
    }
}

#[test]
fn t05_effective_bitrate() {
    assert_eq!(tv::effective_bitrate(None, Some(96000)), None);
    assert_eq!(tv::effective_bitrate(Some(128000), Some(96000)), Some(96000));
    assert_eq!(tv::effective_bitrate(Some(64000), Some(96000)), Some(64000));
    assert_eq!(tv::effective_bitrate(Some(1000), Some(96000)), Some(8000));
    assert_eq!(tv::effective_bitrate(Some(64000), None), Some(64000));
}

#[test]
fn t06_check_rename_window() {
    let now = 1e9;
    let w = tv::check_rename_window(&[], now);
    assert!(w.ok && w.recent.is_empty());
    assert!(tv::check_rename_window(&[now - 100_000.0], now).ok);
    let full = tv::check_rename_window(&[now - 500_000.0, now - 100_000.0], now);
    assert!(!full.ok);
    assert_eq!(full.retry_at, Some(now + 100_000.0));
    assert!(tv::check_rename_window(&[now - 700_000.0, now - 100_000.0], now).ok, "the old stamp is outside the window");
    assert!(tv::check_rename_window(&[now + 5_000.0, now + 9_000.0], now).ok, "future stamps are ignored");
    assert_eq!(tv::check_rename_window(&[f64::NAN, now - 10.0], now).recent.len(), 1);
}

#[test]
fn t07_claim_status() {
    let now = 1e9;
    let base = ClaimInput { actor_connected: true, now, ..Default::default() };
    assert_eq!(tv::claim_status(&ClaimInput { is_owner: true, ..base.clone() }), ClaimStatus::AlreadyOwner);
    assert_eq!(tv::claim_status(&ClaimInput { actor_connected: false, ..base.clone() }), ClaimStatus::NotConnected);
    assert_eq!(tv::claim_status(&ClaimInput { owner_connected: true, ..base.clone() }), ClaimStatus::OwnerPresent);
    let left = Some(now - 60_000.0);
    assert_eq!(tv::claim_status(&ClaimInput { owner_left_at: left, lock_minutes: 2.0, ..base.clone() }), ClaimStatus::Locked { claimable_at: now + 60_000.0 });
    assert_eq!(tv::claim_status(&ClaimInput { owner_left_at: left, lock_minutes: 2.0, is_mod: true, ..base.clone() }), ClaimStatus::Ok);
    assert_eq!(tv::claim_status(&base), ClaimStatus::Ok);
    assert_eq!(tv::claim_status(&ClaimInput { lock_minutes: 1.0, ..base }), ClaimStatus::Locked { claimable_at: now + 60_000.0 });
}

#[test]
fn t08_target_problem() {
    let base = |action: &'static str| TargetInput { action, actor_id: "a", target_id: "t", owner_id: "o", ..Default::default() };
    let t = |i: TargetInput<'static>| tv::target_problem(&i);
    assert_eq!(t(TargetInput { target_id: "a", owner_id: "a", ..base("kick") }), Some(Msg::TargetSelf));
    assert_eq!(t(TargetInput { target_id: "o", ..base("kick") }), Some(Msg::TargetOwner));
    assert_eq!(t(TargetInput { target_protected: true, target_connected: true, ..base("kick") }), Some(Msg::TargetProtected));
    assert_eq!(t(TargetInput { target_connected: false, ..base("kick") }), Some(Msg::TargetNotConnected));
    assert_eq!(t(TargetInput { target_connected: true, ..base("kick") }), None);
    assert_eq!(t(TargetInput { target_connected: true, target_is_bot: true, ..base("kick") }), None, "bots may be kicked");
    assert_eq!(t(TargetInput { target_id: "a", ..base("ban") }), Some(Msg::TargetSelf));
    assert_eq!(t(TargetInput { target_id: "o", ..base("ban") }), Some(Msg::TargetOwner));
    assert_eq!(t(TargetInput { target_protected: true, is_banned: true, ..base("ban") }), Some(Msg::TargetProtected));
    assert_eq!(t(TargetInput { is_banned: true, ..base("ban") }), Some(Msg::AlreadyBanned));
    assert_eq!(t(TargetInput { ban_count: tv::MAX_BANS_PER_CHANNEL, ..base("ban") }), Some(Msg::BanLimit));
    assert_eq!(t(TargetInput { ban_count: tv::MAX_BANS_PER_CHANNEL - 1, ..base("ban") }), None);
    assert_eq!(t(TargetInput { target_is_bot: true, ..base("ban") }), None, "bots may be banned");
    assert_eq!(t(TargetInput { is_banned: false, ..base("unban") }), Some(Msg::NotBanned));
    assert_eq!(t(TargetInput { is_banned: true, ..base("unban") }), None);
    assert_eq!(t(TargetInput { target_id: "o", ..base("transfer") }), Some(Msg::TargetAlreadyOwner));
    assert_eq!(t(TargetInput { target_is_bot: true, target_connected: true, ..base("transfer") }), Some(Msg::TargetBot));
    assert_eq!(t(TargetInput { target_connected: false, ..base("transfer") }), Some(Msg::TargetNotConnected));
    assert_eq!(t(TargetInput { target_connected: true, ..base("transfer") }), None);
    assert_eq!(t(base("lock")), None);
}

#[test]
fn t09_can_control() {
    assert!(tv::can_control("claim", false, false));
    assert!(!tv::can_control("lock", false, false));
    assert!(tv::can_control("lock", true, false));
    assert!(tv::can_control("lock", false, true));
    assert!(!tv::can_control("nonsense", true, true));
}

// ── 10-12: overwrites and reconcile decisions ─────────────────────────────────

fn ow_json(list: &[feat_tempvoice::discord_port::Overwrite]) -> Value {
    Value::Array(list.iter().map(|o| json!({ "id": o.id, "type": o.kind, "allow": o.allow.to_string(), "deny": o.deny.to_string() })).collect())
}

fn template() -> Vec<feat_tempvoice::discord_port::Overwrite> {
    use feat_tempvoice::discord_port::Overwrite;
    vec![Overwrite { id: "9".into(), kind: 0, allow: 0, deny: 1 << 21 }, Overwrite { id: "400".into(), kind: 0, allow: (1 << 28) | (1 << 10), deny: 0 }]
}

#[test]
fn t10_t11_overwrites_match_the_js() {
    let g = golden();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let voice = tv::compute_voice_overwrites(&tv::VoiceOverwriteInput {
        everyone_id: "9".into(),
        bot_id: "200".into(),
        owner_id: "300".into(),
        template: template(),
        locked: true,
        hidden: true,
        banned_user_ids: s(&["500", "300"]),
        exempt_role_ids: s(&["400", "9"]),
        grantable: None,
    });
    assert_eq!(ow_json(&voice), g["overwrites"]["voice"]);
    let masked = tv::compute_voice_overwrites(&tv::VoiceOverwriteInput {
        everyone_id: "9".into(),
        bot_id: "200".into(),
        owner_id: "300".into(),
        template: template(),
        locked: true,
        exempt_role_ids: s(&["400"]),
        grantable: Some((1 << 10) | (1 << 20) | (1 << 4) | (1 << 24) | (1 << 28)),
        ..Default::default()
    });
    assert_eq!(ow_json(&masked), g["overwrites"]["voiceMasked"]);
    let private = tv::compute_text_overwrites(&tv::TextOverwriteInput {
        everyone_id: "9".into(),
        bot_id: "200".into(),
        owner_id: "300".into(),
        template: template(),
        members_only: true,
        member_ids: s(&["300", "600"]),
        exempt_role_ids: s(&["400"]),
        ..Default::default()
    });
    assert_eq!(ow_json(&private), g["overwrites"]["textPrivate"]);
    let public = tv::compute_text_overwrites(&tv::TextOverwriteInput {
        everyone_id: "9".into(),
        bot_id: "200".into(),
        owner_id: "300".into(),
        template: template(),
        members_only: false,
        banned_user_ids: s(&["500", "300", "200"]),
        grantable: Some(1 << 10),
        ..Default::default()
    });
    assert_eq!(ow_json(&public), g["overwrites"]["textPublic"]);

    // JS test 10: the owner is never banned and the output is deterministic.
    let input = tv::VoiceOverwriteInput {
        everyone_id: "100".into(),
        bot_id: "200".into(),
        owner_id: "300".into(),
        locked: true,
        banned_user_ids: s(&["300", "200", "500"]),
        exempt_role_ids: s(&["400"]),
        ..Default::default()
    };
    let a = tv::compute_voice_overwrites(&input);
    assert_eq!(a, tv::compute_voice_overwrites(&input));
    assert_eq!(a.iter().find(|o| o.id == "300").unwrap().deny & (1 << 20), 0);
    assert!(a.iter().all(|o| o.allow != 0 || o.deny != 0));
    let everyone_exempt = tv::compute_voice_overwrites(&tv::VoiceOverwriteInput { exempt_role_ids: s(&["100"]), ..input });
    assert_eq!(everyone_exempt.iter().find(|o| o.id == "100").unwrap().deny & (1 << 20), 1 << 20);
}

#[test]
fn t12_reconcile_decision() {
    use tv::ReconcileAction as A;
    let now = 1e9;
    let min = 60_000.0;
    assert_eq!(tv::reconcile_decision(false, 0, 0, None, now), A::Purge);
    assert_eq!(tv::reconcile_decision(true, 2, 0, None, now), A::Occupied);
    assert_eq!(tv::reconcile_decision(true, 0, 0, None, now), A::Delete);
    assert_eq!(tv::reconcile_decision(true, 0, 5, Some(now - 10.0 * min), now), A::Delete);
    assert_eq!(tv::reconcile_decision(true, 0, 5, Some(now - min), now), A::Schedule { run_at: now + 4.0 * min, set_empty_since: false });
    assert_eq!(tv::reconcile_decision(true, 0, 5, None, now), A::Schedule { run_at: now + 5.0 * min, set_empty_since: true });
}

// ── 13: names ─────────────────────────────────────────────────────────────────

fn bob() -> NameContext {
    use bot_util::placeholder_parser::{PlaceholderGuild, PlaceholderMember, PlaceholderUser};
    let guild = PlaceholderGuild { id: "9".into(), name: Some("Srv".into()), member_count: Some(3.0), ..Default::default() };
    let user = PlaceholderUser { id: "1".into(), username: Some("bob".into()), tag: Some("bob".into()), avatar_url: Some(String::new()), ..Default::default() };
    NameContext {
        member: Some(PlaceholderMember { display_name: Some("Bob".into()), user: Some(user), guild: Some(guild.clone()), ..Default::default() }),
        guild: Some(guild),
    }
}

#[test]
fn t13_render_and_validate_name_formats_match_the_js() {
    let ctx = bob();
    assert_eq!(tv::render_channel_name(tv::DEFAULT_NAME_FORMAT, &ctx, 3), "#3 - Bob's Channel");
    assert_eq!(tv::render_channel_name("{user.name} room", &ctx, 1), "bob room");
    assert_eq!(tv::render_channel_name("{user}", &ctx, 3), "#3 - Bob's Channel");
    let g = golden();
    for case in g["render"].as_array().unwrap() {
        let f = case["format"].as_str().unwrap();
        assert_eq!(tv::render_channel_name(f, &ctx, 3), case["out"].as_str().unwrap(), "render {f:?}");
    }
    for case in g["validate"].as_array().unwrap() {
        let f = case["format"].as_str().unwrap();
        let ours = match tv::validate_name_format(Some(f), &ctx) {
            NameFormatCheck::Ok { preview, warnings } => json!({ "ok": true, "preview": preview, "warnings": warnings }),
            NameFormatCheck::Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(ours, case["out"], "validate {f:?}");
    }
}

// ── 14: builders ──────────────────────────────────────────────────────────────

fn row_components(rows: &[serenity::builder::CreateActionRow]) -> Vec<Value> {
    rows.iter().map(|r| serde_json::to_value(r).unwrap()).collect()
}

#[test]
fn t14_panel_picker_and_modal_match_the_js() {
    let g = golden();
    let ch = "123456789012345678";
    let settings = MemorySettingsStore::new_row("9", Map::from_iter([("embedColor".to_string(), json!(1193046))])).unwrap();
    let embed = embed_json(&tv::control_panel_embed("111", Some(&settings)));
    let want = &g["panel"]["embed"];
    for k in ["title", "description", "color", "footer"] {
        assert_eq!(embed[k], want[k], "panel embed {k}");
    }
    let rows = row_components(&tv::control_panel_rows(ch));
    for (r, wr) in rows.iter().zip(g["panel"]["components"].as_array().unwrap()) {
        assert_eq!(r["type"], wr["type"]);
        for (b, wb) in r["components"].as_array().unwrap().iter().zip(wr["components"].as_array().unwrap()) {
            for k in ["custom_id", "label", "style", "type"] {
                assert_eq!(b[k], wb[k], "button {k}");
            }
            assert_eq!(b["emoji"]["name"], wb["emoji"]["name"]);
        }
    }
    let banned = ["5".to_string(), "6".to_string()];
    for (key, action, list) in [("kick", "kick", &[][..]), ("ban", "ban", &[][..]), ("ban2", "ban", &banned[..]), ("transfer", "transfer", &[][..])] {
        let (content, rows) = tv::member_picker_parts(action, ch, list);
        assert_eq!(content, g["pickers"][key]["content"].as_str().unwrap(), "{key}");
        let ours = row_components(&rows);
        let theirs = g["pickers"][key]["components"].as_array().unwrap();
        assert_eq!(ours.len(), theirs.len());
        for (r, wr) in ours.iter().zip(theirs) {
            let (s, ws) = (&r["components"][0], &wr["components"][0]);
            for k in ["type", "custom_id", "placeholder", "min_values", "max_values"] {
                assert_eq!(s[k], ws[k], "{key} select {k}");
            }
        }
    }
    for (key, action, current) in [("limit", "limit", "5"), ("limitNull", "limit", ""), ("rename", "rename", "Old name"), ("renameEmpty", "rename", "")] {
        let ours = serde_json::to_value(tv::build_value_modal(action, ch, current)).unwrap();
        let theirs = &g["modals"][key];
        assert_eq!(ours["custom_id"], theirs["custom_id"], "{key}");
        assert_eq!(ours["title"], theirs["title"], "{key}");
        let (i, wi) = (&ours["components"][0]["components"][0], &theirs["components"][0]["components"][0]);
        for k in ["type", "custom_id", "style", "required", "label", "min_length", "max_length"] {
            assert_eq!(i[k], wi[k], "{key} input {k}");
        }
        assert_eq!(i.get("value").filter(|v| !v.is_null()), wi.get("value"), "{key} value");
    }
}

// ── 16: messages ──────────────────────────────────────────────────────────────

#[test]
fn t16_messages_match_the_js() {
    let g = golden();
    let p = MsgParams {
        target: Some("222".into()),
        owner: Some("333".into()),
        n: Some(7),
        at: Some(1_700_000_000_000.0),
        retry_at: Some(1_700_000_600_000.0),
        names: vec!["Manage Channels".into(), "Manage Roles".into()],
        channel_id: Some("444".into()),
        code: Some(50013),
    };
    for m in Msg::ALL {
        let want = &g["msg"][m.key()]["params"];
        let ours = embed_json(&m.embed(&p));
        assert_eq!(ours["title"], want["title"], "{}", m.key());
        assert_eq!(ours["description"], want["description"], "{}", m.key());
        assert_eq!(ours["color"], want["color"], "{}", m.key());
        let none = &g["msg"][m.key()]["none"];
        if !matches!(m, Msg::ClaimLocked | Msg::RenameRate) {
            // undefined dates render as NaN in the JS
            assert_eq!(embed_json(&m.plain())["description"], none["description"], "{} without params", m.key());
        }
    }
    assert_eq!(embed_json(&Msg::NoChangeLimit.embed(&MsgParams { n: Some(0), ..Default::default() }))["description"], g["msg"]["NO_CHANGE_LIMIT"]["zero"]["description"]);
    assert_eq!(embed_json(&Msg::DiscordError.plain())["description"], g["msg"]["DISCORD_ERROR"]["unknown"]["description"]);
    let op = OkParams { name: Some("My *Room*".into()), target: Some("222".into()), n: Some(4), new_name: Some("New_name".into()) };
    for a in ["lock", "unlock", "hide", "reveal", "limit", "rename", "kick", "ban", "unban", "transfer", "claim", "clean", "nope"] {
        let ours = embed_json(&tv::ok_embed(a, &op));
        assert_eq!(ours["title"], g["ok"][a]["title"], "{a}");
        assert_eq!(ours["description"], g["ok"][a]["description"], "{a}");
        assert_eq!(ours["color"], g["ok"][a]["color"], "{a}");
    }
    assert_eq!(embed_json(&tv::ok_embed("limit", &OkParams { name: Some("x".into()), n: Some(0), ..Default::default() }))["description"], g["ok"]["limit0"]["description"]);
    assert_eq!(embed_json(&tv::ok_embed("clean", &OkParams { n: Some(0), ..Default::default() }))["title"], g["ok"]["clean0"]["title"]);
    assert_eq!(embed_json(&tv::ok_embed("lock", &OkParams::default()))["description"], g["ok"]["noname"]["description"]);
    for (code, text) in g["describe"].as_object().unwrap() {
        let c = code.parse::<u64>().ok();
        assert_eq!(tv::describe_discord_error(c), text.as_str().unwrap(), "{code}");
    }
    assert_eq!(embed_json(&Msg::NotInTemp.plain())["title"], "❌  Not In A Temporary Channel");
    assert_eq!(embed_json(&Msg::NoChangeLock.plain())["title"], "⚠️  No Change");
}

const G: &str = "955021000000000001";
const OWNER: &str = "840000000000000099";

fn hub(json_extra: Value) -> bot_db::TempVoiceHub {
    let mut base = json!({
        "id": "h1", "guildId": "9", "channelId": "700", "categoryId": null, "nameFormat": "A `b` c", "userLimit": 0, "bitrate": null,
        "keepAliveMinutes": 0, "ownershipLockMinutes": 0, "syncPermissions": true, "createTextChannel": false, "textChannelMembersOnly": true,
        "ignoredRoleIds": [], "moderatorRoleIds": [], "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z",
    });
    for (k, v) in json_extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

#[tokio::test]
async fn t16_hub_summary_matches_the_js() {
    let t = TestApp::new();
    install_guilds(&t, &[TestGuild::new("9", OWNER, admin()).channel("800", 4, "Cat", None, &[]).channel("700", 2, "hub", Some("800"), &[])]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, gid("9")).unwrap();
    for case in golden()["summaries"].as_array().unwrap() {
        let h = hub(case["hub"].clone());
        assert_eq!(tv::hub_summary(&h, None), case["top"].as_str().unwrap());
        assert_eq!(tv::hub_summary(&h, Some(&guild)), case["parented"].as_str().unwrap());
    }
}

#[tokio::test]
async fn t16_exempt_roles_mods_protected_and_hub_problems() {
    let t = TestApp::new();
    let r1 = "830000000000000001";
    let r2 = "830000000000000002";
    let r3 = "830000000000000003";
    let m1 = "830000000000000011";
    let guild = TestGuild::new(G, OWNER, admin())
        .role(r1, 1, Permissions::empty())
        .role(r2, 2, Permissions::empty())
        .role(r3, 3, Permissions::empty())
        .role(m1, 4, Permissions::empty())
        .role("830000000000000020", 5, Permissions::BAN_MEMBERS)
        .member("840000000000000001", &[])
        .member("840000000000000002", &[r2])
        .member("840000000000000003", &[r1])
        .member("840000000000000004", &["830000000000000020"])
        .member("840000000000000005", &[m1])
        .channel("850000000000000005", 4, "Cat", None, &[])
        .channel("850000000000000010", 2, "hub", Some("850000000000000005"), &[]);
    install_guilds(&t, &[guild]);
    let cached = bot_core::interaction_helpers::cached_guild(&t, gid(G)).unwrap();
    let h = hub(json!({ "ignoredRoleIds": [r1, "830000000000000099"], "moderatorRoleIds": [r2, G] }));
    let settings = MemorySettingsStore::new_row(G, Map::from_iter([("modRoleIds".to_string(), json!([r3, r1]))])).unwrap();
    let mut ex = tv::exempt_role_ids(Some(&h), Some(&settings), Some(&cached));
    ex.sort();
    assert_eq!(ex, vec![r1, r2, r3]);
    let mut ex = tv::exempt_role_ids(Some(&h), None, Some(&cached));
    ex.sort();
    assert_eq!(ex, vec![r1, r2]);

    let h2 = hub(json!({ "moderatorRoleIds": [r2], "ignoredRoleIds": [r1] }));
    let m = |u: &str| member(&t, G, u);
    let g = gid(G);
    assert!(!tv::is_temp_voice_mod(&t, g, &m("840000000000000001"), Some(&h2), None));
    assert!(tv::is_temp_voice_mod(&t, g, &m("840000000000000002"), Some(&h2), None));
    assert!(!tv::is_temp_voice_mod(&t, g, &m("840000000000000003"), Some(&h2), None), "an ignored role is protected but not a moderator");
    assert!(tv::is_temp_voice_mod(&t, g, &m("840000000000000004"), Some(&h2), None), "MODERATOR level counts");
    let mod_settings = MemorySettingsStore::new_row(G, Map::from_iter([("modRoleIds".to_string(), json!([m1]))])).unwrap();
    assert!(tv::is_temp_voice_mod(&t, g, &m("840000000000000005"), Some(&h2), Some(&mod_settings)));
    assert!(tv::is_protected_target(&t, g, &m("840000000000000003"), Some(&h2), None));
    assert!(tv::is_protected_target(&t, g, &m("840000000000000002"), Some(&h2), None));
    assert!(!tv::is_protected_target(&t, g, &m("840000000000000001"), Some(&h2), None));

    let gone = hub(json!({ "channelId": "850000000000000099" }));
    let problems = tv::hub_problems(&t, g, &gone);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].starts_with("Hub channel deleted"));
    assert!(tv::hub_problems(&t, g, &hub(json!({ "channelId": "850000000000000010" }))).is_empty());
}

#[tokio::test]
async fn t14_filters_without_a_memo_let_the_task_decide() {
    let t = TestApp::new();
    let g = gid(G);
    assert!(!tv::voice_filter(&t, g, true, None, Some(cid("850000000000000010"))));
    assert!(tv::voice_filter(&t, g, false, None, Some(cid("850000000000000010"))));
    assert!(tv::channel_delete_filter(&t, g, cid("850000000000000010")));
}

#[tokio::test]
async fn t16_component_handlers_answer_without_io_for_stale_ids_and_wrong_context() {
    let t = TestApp::new();
    install_guilds(&t, &[TestGuild::new(G, OWNER, admin())]);
    t.install(registry());
    let send = |i: FakeInteraction| route_interaction(&t, i.guild(G).user("840000000000000001").build());
    send(FakeInteraction::button("voice:explode:123456789012345678")).await;
    send(FakeInteraction::button("voice:lock:notasnowflake")).await;
    send(FakeInteraction::user_select("voice-select:lock:123456789012345678", &["840000000000000002"])).await;
    send(FakeInteraction::modal("voice-modal:kick:123456789012345678", &[("voice-value", "x")])).await;
    send(FakeInteraction::modal("voice-modal:limit:123456789012345678", &[("voice-value", "abc")])).await;
    let calls = t.responses.calls();
    assert_eq!(calls.len(), 5, "{:?}", t.responses.names());
    assert_eq!(calls[0].content(), Some("This button is no longer active."));
    assert_eq!(calls[1].content(), Some("This button is no longer active."));
    assert_eq!(calls[2].content(), Some("This menu is no longer active."));
    assert_eq!(calls[3].content(), Some("This form has expired. Please run the command again."));
    assert_eq!(calls[4].embed_title(), Some("❌  Invalid Limit"));
    assert!(calls.iter().all(|c| c.ephemeral()));

    // A guild that is not cached: "Server Only".
    let t2 = TestApp::new().with_guilds([G]);
    t2.install(registry());
    route_interaction(&t2, FakeInteraction::button("voice:lock:123456789012345678").guild(G).build()).await;
    let c = t2.responses.calls();
    assert_eq!(c.last().and_then(|c| c.embed_title()), Some("❌  Server Only"));
}

#[tokio::test]
async fn voice_command_outside_a_temp_channel() {
    let t = TestApp::new();
    install_guilds(&t, &[TestGuild::new(G, OWNER, admin()).member("840000000000000001", &[])]);
    t.install(registry());
    route_interaction(&t, FakeInteraction::slash("voice").guild(G).user("840000000000000001").subcommand("lock").build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    let c = &t.responses.calls()[1];
    assert_eq!(c.embed_title(), Some("❌  Not In A Temporary Channel"));
}

// ── Voice roles (test/voice-roles.test.js) ────────────────────────────────────

const CAT: &str = "860000000000000001";
const VA: &str = "860000000000000002";
const VB: &str = "860000000000000003";
const VC: &str = "860000000000000004";
const ST: &str = "860000000000000005";
const AFK: &str = "860000000000000006";
const RA: &str = "870000000000000001";
const RB: &str = "870000000000000002";
const RC: &str = "870000000000000003";
const RANY: &str = "870000000000000004";
const RCAT: &str = "870000000000000005";
const RHIGH: &str = "870000000000000006";
const RMANAGED: &str = "870000000000000007";
const RADMIN: &str = "870000000000000008";
const RGONE: &str = "870000000000000009";
const U1: &str = "880000000000000001";
const U2: &str = "880000000000000002";
const U3: &str = "880000000000000003";
const U5: &str = "880000000000000005";
const B1: &str = "880000000000000009";

fn vr_guild(bot_perms: Permissions, members: &[(&str, &[&str])]) -> TestGuild {
    let mut g = TestGuild::new(G, OWNER, bot_perms)
        .role(RA, 1, Permissions::empty())
        .role(RB, 2, Permissions::empty())
        .role(RC, 3, Permissions::empty())
        .role(RANY, 4, Permissions::empty())
        .role(RCAT, 5, Permissions::empty())
        .role(RHIGH, 60, Permissions::empty())
        .role(RADMIN, 7, Permissions::ADMINISTRATOR)
        .channel(CAT, 4, "cat", None, &[])
        .channel(VA, 2, "va", Some(CAT), &[])
        .channel(VB, 2, "vb", Some(CAT), &[])
        .channel(VC, 2, "vc", None, &[])
        .channel(ST, 13, "stage", None, &[])
        .channel(AFK, 2, "afk", None, &[])
        .afk(AFK)
        .bot_member(B1);
    g.base = g.base.role_with(RMANAGED, 8, Permissions::empty(), |r| r["managed"] = json!(true));
    for (id, roles) in members {
        g = g.member(id, roles);
    }
    g
}

fn vr_settings(links: Value, any: Value) -> bot_core::settings::Settings {
    std::sync::Arc::new(
        MemorySettingsStore::new_row(G, Map::from_iter([("voiceRoleLinks".to_string(), links), ("voiceRoleAnyRoleIds".to_string(), any)])).unwrap(),
    )
}

fn links() -> Value {
    json!([{ "channelId": VA, "roleId": RA }, { "channelId": VB, "roleId": RB }])
}

/// The member's roles as Discord has them: the initial roles plus the role calls made.
fn truth(rest: &FakeRest, initial: &[&str], user: &str) -> Vec<String> {
    let mut roles: Vec<String> = initial.iter().map(|s| s.to_string()).collect();
    for c in rest.calls.lock().unwrap().iter() {
        if c.data["user"] != user {
            continue;
        }
        let role = c.data["role"].as_str().unwrap_or_default().to_string();
        match c.op.as_str() {
            "add_role" if !roles.contains(&role) => roles.push(role),
            "remove_role" => roles.retain(|r| *r != role),
            _ => {}
        }
    }
    roles.sort();
    roles
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn vr_settings_shapes_match_the_js() {
    let g = golden();
    let vs = &g["voiceRoles"]["settings"];
    let s = MemorySettingsStore::new_row(
        G,
        Map::from_iter([("voiceRoleLinks".to_string(), vs["voiceRoleLinks"].clone()), ("voiceRoleAnyRoleIds".to_string(), json!(["a1", "a1", "r1"]))]),
    )
    .unwrap();
    assert_eq!(serde_json::to_value(vr::links_of(Some(&s))).unwrap(), g["voiceRoles"]["links"]);
    assert_eq!(json!(vr::any_roles_of(Some(&s))), g["voiceRoles"]["any"]);
    assert_eq!(json!(vr::managed_role_ids(Some(&s))), g["voiceRoles"]["managed"]);
    assert!(vr::has_voice_roles(Some(&s)));
    assert!(!vr::has_voice_roles(None));
    let spot = |c: &str, p: Option<&str>| vr::VoiceSpot { channel_id: c.into(), parent_id: p.map(str::to_string) };
    let d = &g["voiceRoles"]["desired"];
    assert_eq!(json!(vr::desired_role_ids(Some(&s), Some("afk"), Some(&spot("c1", None)))), d["c1"]);
    assert_eq!(json!(vr::desired_role_ids(Some(&s), Some("afk"), Some(&spot("x", Some("cat"))))), d["inCat"]);
    assert_eq!(json!(vr::desired_role_ids(Some(&s), Some("afk"), None)), d["none"]);
    assert_eq!(json!(vr::desired_role_ids(Some(&s), Some("afk"), Some(&spot("afk", None)))), d["afk"]);
    let mut off = s.clone();
    off.voice_role_ignore_afk = false;
    assert_eq!(json!(vr::desired_role_ids(Some(&off), Some("afk"), Some(&spot("afk", None)))), d["afkOff"]);
    let patch_json = |p: Option<bot_core::settings::SettingsPatch>| p.map(|p| Value::Object(p.0)).unwrap_or(Value::Null);
    assert_eq!(patch_json(vr::patch_drop_channel(&s, "c1")), g["voiceRoles"]["dropChannel"]);
    assert_eq!(patch_json(vr::patch_drop_channel(&s, "zzz")), g["voiceRoles"]["dropChannelNone"]);
    // The JS any-list keeps its raw duplicates.
    assert_eq!(patch_json(vr::patch_drop_role(&s, "r1")), g["voiceRoles"]["dropRole"]);
    assert_eq!(patch_json(vr::patch_drop_role(&s, "zzz")), g["voiceRoles"]["dropRoleNone"]);
}

#[tokio::test]
async fn vr_desired_roles_channel_category_stage_afk() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[])]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, gid(G)).unwrap();
    let s = vr_settings(
        json!([{ "channelId": VA, "roleId": RA }, { "channelId": VB, "roleId": RB }, { "channelId": CAT, "roleId": RCAT }, { "channelId": ST, "roleId": RC }]),
        json!([RANY]),
    );
    let at = |c: &str| vr::VoiceSpot { channel_id: c.into(), parent_id: guild.channels.get(&cid(c)).and_then(|ch| ch.parent_id).map(|p| p.to_string()) };
    let afk = vr::afk_channel_of(&guild);
    let want = |c: Option<&str>| sorted(vr::desired_role_ids(Some(&s), afk.as_deref(), c.map(at).as_ref()));
    assert_eq!(want(Some(VA)), sorted(vec![RA.into(), RANY.into(), RCAT.into()]));
    assert_eq!(want(Some(VB)), sorted(vec![RANY.into(), RB.into(), RCAT.into()]));
    assert_eq!(want(Some(VC)), vec![RANY.to_string()]);
    assert_eq!(want(Some(ST)), sorted(vec![RANY.into(), RC.into()]));
    assert!(want(None).is_empty());
    assert!(want(Some(AFK)).is_empty(), "the AFK channel gets nothing by default");
    let mut off = (*s).clone();
    off.voice_role_ignore_afk = false;
    assert_eq!(vr::desired_role_ids(Some(&off), afk.as_deref(), Some(&at(AFK))), vec![RANY.to_string()]);
    assert_eq!(vr::desired_role_ids(Some(&s), None, Some(&at(AFK))), vec![RANY.to_string()], "no AFK channel: nothing is special");
}

#[tokio::test]
async fn vr_join_move_leave_follow_the_voice_state() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[(U1, &[RC])])]);
    let rest = FakeRest::install(&t);
    let s = vr_settings(links(), json!([RANY]));
    let g = gid(G);
    let apply = || async { vr::apply_member(&t, g, &member(&t, G, U1), Some(s.clone()), ApplyOptions::default()).await };

    rest.set_voice(g, uid(U1), Some(cid(VA)));
    let res = apply().await;
    assert_eq!(res.status, ApplyStatus::Ok);
    assert_eq!(sorted(res.added), sorted(vec![RA.into(), RANY.into()]));
    assert_eq!(rest.ops(), vec!["add_role", "add_role"], "single-role calls");
    assert!(rest.calls_of("add_role").iter().all(|c| c["reason"] == vr::ADD_REASON));
    assert_eq!(truth(&rest, &[RC], U1), sorted(vec![RA.into(), RANY.into(), RC.into()]));

    rest.set_voice(g, uid(U1), Some(cid(VB)));
    let before = rest.ops().len();
    let res = apply().await;
    assert_eq!(res.added, vec![RB.to_string()]);
    assert_eq!(res.removed, vec![RA.to_string()]);
    assert_eq!(rest.ops()[before..].to_vec(), vec!["add_role", "remove_role"], "adds first, then removes");
    assert_eq!(rest.calls_of("remove_role")[0]["reason"], vr::REMOVE_REASON);

    let before = rest.ops().len();
    apply().await;
    assert_eq!(rest.ops().len(), before, "nothing differs: no calls");

    rest.set_voice(g, uid(U1), None);
    let res = apply().await;
    assert_eq!(sorted(res.removed), sorted(vec![RANY.into(), RB.into()]));
    assert_eq!(truth(&rest, &[RC], U1), vec![RC.to_string()], "unmanaged roles stay");

    rest.set_voice(g, uid(U1), Some(cid(VA)));
    apply().await;
    rest.set_voice(g, uid(U1), Some(cid(AFK)));
    apply().await;
    assert_eq!(truth(&rest, &[RC], U1), vec![RC.to_string()], "the AFK channel counts as leaving");
}

#[tokio::test]
async fn vr_fast_hops_with_a_lagging_cache() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[(U1, &[])])]);
    let rest = FakeRest::install(&t);
    rest.role_echo.store(false, Ordering::Relaxed);
    let s = vr_settings(links(), json!([]));
    let g = gid(G);
    let apply = || async { vr::apply_member(&t, g, &member(&t, G, U1), Some(s.clone()), ApplyOptions::default()).await };
    rest.set_voice(g, uid(U1), Some(cid(VA)));
    apply().await;
    rest.set_voice(g, uid(U1), Some(cid(VB)));
    apply().await;
    assert_eq!(truth(&rest, &[], U1), vec![RB.to_string()]);
    rest.set_voice(g, uid(U1), Some(cid(VA)));
    apply().await;
    assert_eq!(truth(&rest, &[], U1), vec![RA.to_string()], "the roles from the earlier hops were taken into account");
    assert!(member(&t, G, U1).roles.is_empty(), "the cache really never changed");
    rest.set_voice(g, uid(U1), None);
    apply().await;
    assert!(truth(&rest, &[], U1).is_empty());
}

#[tokio::test]
async fn vr_unassignable_roles_are_skipped_bots_and_deleted_roles_ignored() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[(U1, &[]), (U2, &[RADMIN, RHIGH, RA])])]);
    let rest = FakeRest::install(&t);
    let s = vr_settings(
        json!([
            { "channelId": VA, "roleId": RA }, { "channelId": VA, "roleId": RHIGH }, { "channelId": VA, "roleId": RMANAGED },
            { "channelId": VA, "roleId": RADMIN }, { "channelId": VA, "roleId": RGONE }
        ]),
        json!([]),
    );
    let g = gid(G);
    rest.set_voice(g, uid(U1), Some(cid(VA)));
    let res = vr::apply_member(&t, g, &member(&t, G, U1), Some(s.clone()), ApplyOptions::default()).await;
    assert_eq!(truth(&rest, &[], U1), vec![RA.to_string()], "only the safe role is given");
    assert_eq!(res.skipped, 3);
    assert_eq!(res.status, ApplyStatus::Ok);

    let out = vr::apply_member(&t, g, &member(&t, G, U2), Some(s.clone()), ApplyOptions::default()).await;
    assert_eq!(sorted(out.removed), sorted(vec![RA.into(), RADMIN.into()]));
    assert_eq!(truth(&rest, &[RADMIN, RHIGH, RA], U2), vec![RHIGH.to_string()], "a role above the bot is left alone");

    rest.set_voice(g, uid(B1), Some(cid(VA)));
    let before = rest.ops().len();
    assert_eq!(vr::apply_member(&t, g, &member(&t, G, B1), Some(s), ApplyOptions::default()).await.status, ApplyStatus::Skipped);
    assert_eq!(rest.ops().len(), before);
}

#[tokio::test]
async fn vr_without_manage_roles_nothing_is_called() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::VIEW_CHANNEL, &[(U1, &[])])]);
    let rest = FakeRest::install(&t);
    rest.set_voice(gid(G), uid(U1), Some(cid(VA)));
    let res = vr::apply_member(&t, gid(G), &member(&t, G, U1), Some(vr_settings(links(), json!([]))), ApplyOptions::default()).await;
    assert_eq!(res.status, ApplyStatus::Blocked);
    assert!(rest.ops().is_empty());
}

#[tokio::test]
async fn vr_api_errors() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[(U1, &[]), (U2, &[]), (U3, &[])])]);
    let rest = FakeRest::install(&t);
    let s = vr_settings(json!([{ "channelId": VA, "roleId": RA }, { "channelId": VA, "roleId": RB }]), json!([]));
    let g = gid(G);
    for u in [U1, U2, U3] {
        rest.set_voice(g, uid(u), Some(cid(VA)));
    }
    rest.fail_op("add_role", 10007);
    assert_eq!(vr::apply_member(&t, g, &member(&t, G, U1), Some(s.clone()), ApplyOptions::default()).await.status, ApplyStatus::Left);
    assert_eq!(rest.calls_of("add_role").len(), 1, "stopped at the first call");
    rest.fail_op("add_role", 50013);
    let res = vr::apply_member(&t, g, &member(&t, G, U2), Some(s.clone()), ApplyOptions::default()).await;
    assert_eq!(res.status, ApplyStatus::Ok);
    assert_eq!(res.skipped, 2);
    rest.fail_op("add_role", 500);
    assert_eq!(vr::apply_member(&t, g, &member(&t, G, U3), Some(s.clone()), ApplyOptions::default()).await.status, ApplyStatus::Failed);
    // A failed call is not remembered: the next change tries again.
    rest.clear_fail("add_role");
    rest.clear();
    vr::apply_member(&t, g, &member(&t, G, U3), Some(s), ApplyOptions::default()).await;
    assert_eq!(truth(&rest, &[], U3), sorted(vec![RA.into(), RB.into()]));
}

#[tokio::test]
async fn vr_reconcile_and_release() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[(U1, &[]), (U2, &[]), (U3, &[RA, RC]), (U5, &[RC])])]);
    let rest = FakeRest::install(&t);
    let s = vr_settings(json!([{ "channelId": VA, "roleId": RA }]), json!([RANY]));
    let g = gid(G);
    rest.set_voice(g, uid(U1), Some(cid(VA)));
    rest.set_voice(g, uid(U2), Some(cid(VC)));
    rest.set_voice(g, uid(B1), Some(cid(VA)));
    let res = vr::reconcile_guild(&t, g, Some(s.clone()), vr::RECONCILE_MAX).await;
    assert!(!res.busy);
    assert_eq!(res.checked, 3, "two in voice and one holder; bots and members without managed roles are not looked at");
    assert_eq!(res.added, 3);
    assert_eq!(res.removed, 1);
    assert_eq!(res.changed, 3);
    assert_eq!(truth(&rest, &[], U1), sorted(vec![RA.into(), RANY.into()]));
    assert_eq!(truth(&rest, &[], U2), vec![RANY.to_string()]);
    assert_eq!(truth(&rest, &[RA, RC], U3), vec![RC.to_string()]);
    let bounded = vr::reconcile_guild(&t, g, Some(s.clone()), 1).await;
    assert!(bounded.truncated);
    assert_eq!(bounded.checked, 1);

    // A dropped link's role is taken from holders not entitled to it any more.
    let after = vr_settings(json!([]), json!([RANY]));
    let rel = vr::release_dropped_roles(&t, g, Some(after), &[RA.to_string()], vr::RECONCILE_MAX).await;
    assert_eq!(rel.removed, 1);
    assert_eq!(truth(&rest, &[], U1), vec![RANY.to_string()]);
}

#[tokio::test]
async fn vr_hooks_drop_links_even_while_the_module_is_off() {
    let t = TestApp::new();
    install_guilds(&t, &[vr_guild(Permissions::MANAGE_ROLES, &[])]);
    t.install(registry());
    t.settings_store.put(
        MemorySettingsStore::new_row(
            G,
            Map::from_iter([
                ("voiceRolesEnabled".to_string(), json!(false)),
                ("voiceRoleLinks".to_string(), json!([{ "channelId": VA, "roleId": RA }, { "channelId": VB, "roleId": RB }, { "channelId": CAT, "roleId": RCAT }])),
                ("voiceRoleAnyRoleIds".to_string(), json!([RANY, RB])),
            ]),
        )
        .unwrap(),
    );
    let ch = |id: &str| channel(&t, G, id).unwrap();
    use bot_core::prelude::{ChannelDelete, RoleDelete};
    t.run_hooks(ChannelDelete { guild_id: gid(G), channel: ch(VA) }).await;
    let row = t.settings_store.row(G).unwrap();
    let ids: Vec<String> = vr::links_of(Some(&row)).into_iter().map(|l| l.channel_id).collect();
    assert_eq!(ids, vec![VB.to_string(), CAT.to_string()]);
    t.run_hooks(ChannelDelete { guild_id: gid(G), channel: ch(CAT) }).await;
    t.run_hooks(RoleDelete { guild_id: gid(G), role_id: serenity::all::RoleId::new(RB.parse().unwrap()), role: None }).await;
    let row = t.settings_store.row(G).unwrap();
    assert!(vr::links_of(Some(&row)).is_empty());
    assert_eq!(row.voice_role_any_role_ids, vec![RANY.to_string()]);
    feat_tempvoice::plugins::voice_roles::idle(&t).await;
}

#[tokio::test]
async fn vr_filters() {
    use bot_core::prelude::VoiceChange;
    use feat_tempvoice::plugins::voice_roles as p;
    let t = TestApp::new();
    let g = gid(G);
    assert!(!p::voice_filter(&t, g, VoiceChange::State, false));
    assert!(!p::voice_filter(&t, g, VoiceChange::Join, true));
    assert!(p::voice_filter(&t, g, VoiceChange::Join, false), "no memo: the hook runs and decides");
    assert!(p::channel_delete_filter(&t, g, VA));
    assert!(p::role_delete_filter(&t, g, RA));
}
