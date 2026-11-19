//! Offline tests of feat-reactionroles: the golden checks (commands, plugin, customIds, cache keys, payloads), the pure
//! helpers against JS-produced fixtures, and the ported JS tests that run on fake Discord objects
//! (`test/reaction-roles.test.js` 1-20, `test/role-templates.test.js`).

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::testing::{TestApp, load_golden};
use common::{BOT, FakeOps};
use feat_reactionroles::reaction_role_service::{self as rr, EmojiInfo, EmojiInput, EmojiLookup, MessageRef, PanelLite, PanelView, Plan, Source};
use feat_reactionroles::role_template_setup::{self as setup, PanelStore, RoleTemplateError};
use feat_reactionroles::role_templates::role_templates;
use serde_json::{Value, json};

fn golden() -> &'static Value {
    static G: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    G.get_or_init(|| load_golden("feat-reactionroles/golden.json"))
}

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_reactionroles::register(&mut reg);
    reg
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect()).unwrap_or_default()
}

fn opt_str(v: &Value) -> Option<&str> {
    v.as_str()
}

// ── Golden: command, plugin ───────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["reactionrole"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "reaction-roles");
    let mine: Vec<String> = bot_core::testing::persistent_handler_problems(&reg).into_iter().filter(|p| p.contains("\"rr")).collect();
    assert!(mine.is_empty(), "{mine:?}");
}

#[test]
fn template_choices_match_the_js() {
    let g = golden();
    let want: Vec<(String, String)> = g["templateChoices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap().to_string(), c["value"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(feat_reactionroles::commands::reactionrole::template_choices(), want);
}

// ── Golden: emoji ─────────────────────────────────────────────────────────────

#[test]
fn emoji_helpers_match_the_js() {
    let mut bad = Vec::new();
    for c in golden()["emoji"].as_array().unwrap() {
        let input = c["input"].as_str().unwrap();
        let got = json!({
            "isUnicode": rr::is_unicode_emoji(input),
            "component": rr::component_emoji(Some(input)).map(|e| e.to_json()),
            "key": rr::emoji_key_of_stored(Some(input)),
            "normalized": rr::normalize_emoji_key(input),
        });
        let want = json!({ "isUnicode": c["isUnicode"], "component": c["component"], "key": c["key"], "normalized": c["normalized"] });
        if got != want {
            bad.push(format!("{input:?}: got {got} want {want}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    // test 1
    assert_eq!(rr::normalize_emoji_key("❤️"), rr::normalize_emoji_key("❤"));
    assert_eq!(rr::emoji_key_of_stored(None), None);
    assert_eq!(rr::emoji_key_of_stored(Some("")), None);
}

#[test]
fn unicode_emoji_sweep_matches_v8() {
    let sweep = &golden()["emojiSweep"];
    let inputs = strs(&sweep["inputs"]);
    let want: Vec<bool> = sweep["isUnicode"].as_array().unwrap().iter().map(|b| b.as_bool().unwrap()).collect();
    let bad: Vec<String> = inputs
        .iter()
        .zip(&want)
        .filter(|(s, w)| rr::is_unicode_emoji(s) != **w)
        .map(|(s, w)| format!("{:?} (U+{:X}) js={w}", s, s.chars().next().map(|c| c as u32).unwrap_or(0)))
        .collect();
    assert!(bad.is_empty(), "{} of {} differ:\n{}", bad.len(), inputs.len(), bad.join("\n"));
}

struct GoldenEmojis {
    guild: Vec<EmojiInfo>,
    client: Vec<EmojiInfo>,
}

impl EmojiLookup for GoldenEmojis {
    fn guild_emojis(&self) -> Vec<EmojiInfo> {
        self.guild.clone()
    }
    fn client_emoji(&self, id: &str) -> Option<EmojiInfo> {
        self.client.iter().find(|e| e.id == id).cloned()
    }
}

fn emoji_infos(v: &Value) -> Vec<EmojiInfo> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| EmojiInfo {
            id: e["id"].as_str().unwrap().into(),
            name: e["name"].as_str().unwrap().into(),
            animated: e["animated"].as_bool().unwrap(),
            available: e["available"].as_bool().unwrap(),
            limited_for_bot: e["limitedForBot"].as_bool().unwrap(),
        })
        .collect()
}

#[test]
fn parse_emoji_input_matches_the_js() {
    let g = golden();
    let lookup = GoldenEmojis { guild: emoji_infos(&g["emojiWorld"]["guild"]), client: emoji_infos(&g["emojiWorld"]["client"]) };
    for c in g["parseEmoji"].as_array().unwrap() {
        let got = match rr::parse_emoji_input(opt_str(&c["input"]), &lookup) {
            EmojiInput::Ok { emoji, key } => json!({ "ok": true, "emoji": emoji, "key": key }),
            EmojiInput::Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(got, c["result"], "input {}", c["input"]);
    }
}

// ── Golden: links, names, errors ──────────────────────────────────────────────

#[test]
fn links_names_and_errors_match_the_js() {
    let g = golden();
    for c in g["messageRefs"].as_array().unwrap() {
        let got = match rr::parse_message_reference(c["input"].as_str().unwrap(), c["guildId"].as_str().unwrap(), c["channelId"].as_str().unwrap()) {
            MessageRef::Ok { channel_id, message_id } => json!({ "ok": true, "channelId": channel_id, "messageId": message_id }),
            MessageRef::Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(got, c["result"], "{}", c["input"]);
    }
    for c in g["attachTitles"].as_array().unwrap() {
        let got = rr::default_attach_title(c["content"].as_str().unwrap(), opt_str(&c["embedTitle"]), c["channel"].as_str().unwrap());
        assert_eq!(got, c["result"].as_str().unwrap(), "{}", c["content"]);
    }
    for c in g["panelChoiceNames"].as_array().unwrap() {
        let got = rr::panel_choice_name(c["title"].as_str().unwrap(), c["layout"].as_str().unwrap(), c["itemCount"].as_i64().unwrap());
        assert_eq!(got, c["result"].as_str().unwrap());
        assert!(bot_util::js::utf16_len(&got) <= 100);
    }
    for c in g["itemChoiceNames"].as_array().unwrap() {
        let got = rr::item_choice_name(c["label"].as_str().unwrap(), opt_str(&c["emoji"]), opt_str(&c["roleName"]));
        assert_eq!(got, c["result"].as_str().unwrap());
    }
    for c in g["discordErrors"].as_array().unwrap() {
        assert_eq!(rr::describe_discord_error(c["code"].as_i64()), c["text"].as_str().unwrap());
    }
}

#[test]
fn resolve_item_finds_by_id_role_and_label() {
    // test 5b
    let item = |id: &str, role: &str, label: &str| bot_db::ReactionRoleItem {
        id: id.into(),
        group_id: "g".into(),
        custom_id: format!("rr:g:{role}"),
        role_id: role.into(),
        label: label.into(),
        emoji: None,
        style: 1,
        position: 0,
        description: None,
    };
    let items = vec![item("i1", "400000000000000001", "News"), item("i2", "400000000000000002", "Events")];
    assert_eq!(rr::resolve_item(&items, "i2").unwrap().role_id, "400000000000000002");
    assert_eq!(rr::resolve_item(&items, "400000000000000001").unwrap().id, "i1");
    assert_eq!(rr::resolve_item(&items, "<@&400000000000000002>").unwrap().id, "i2");
    assert_eq!(rr::resolve_item(&items, " news ").unwrap().id, "i1");
    assert!(rr::resolve_item(&items, "nothing").is_none());
}

// ── Golden: planner ───────────────────────────────────────────────────────────

fn source_of(s: &str) -> Source {
    match s {
        "button" => Source::Button,
        "select" => Source::Select,
        "react" => Source::React,
        _ => Source::Unreact,
    }
}

#[test]
fn planner_matches_the_js_exhaustively() {
    let panel = vec!["A".to_string(), "B".to_string(), "C".to_string()];
    let cases = golden()["plans"].as_array().unwrap();
    assert!(cases.len() > 1000);
    for c in cases {
        let p: Plan = rr::plan_role_change(
            c["type"].as_str().unwrap(),
            source_of(c["source"].as_str().unwrap()),
            &panel,
            &strs(&c["held"]),
            &strs(&c["target"]),
            c["max"].as_i64().map(|m| m as i32),
        );
        let got = json!({ "toAdd": p.to_add, "toRemove": p.to_remove, "skipped": p.skipped, "reason": p.reason.map(|r| r.as_str()) });
        let want = json!({ "toAdd": c["toAdd"], "toRemove": c["toRemove"], "skipped": c["skipped"], "reason": c["reason"] });
        assert_eq!(got, want, "case {c}");
    }
}

// ── Golden: payloads, customIds, cache key ────────────────────────────────────

#[test]
fn panel_payloads_match_the_js() {
    for (i, c) in golden()["payloads"].as_array().unwrap().iter().enumerate() {
        let view: PanelView = serde_json::from_value(c["group"].clone()).unwrap();
        let got = rr::build_panel_payload(&view, c["color"].as_i64()).to_json();
        assert_eq!(got, c["payload"], "payload {i}: {}", c["group"]["title"]);
    }
}

#[test]
fn select_rows_footers_custom_ids_and_index_key_match_the_js() {
    let g = golden();
    for c in g["selectRows"].as_array().unwrap() {
        let view: PanelView = serde_json::from_value(c["group"].clone()).unwrap();
        let selected = c["selected"].as_array().map(|_| strs(&c["selected"]));
        let row = rr::build_select_row(&view, selected.as_deref(), opt_str(&c["placeholder"]));
        let got = row.map(|r| rr::Row::Select(r).to_json()).unwrap_or(Value::Null);
        assert_eq!(got, c["row"]);
    }
    for c in g["footers"].as_array().unwrap() {
        let got = rr::panel_footer(c["type"].as_str().unwrap(), c["layout"].as_str().unwrap(), c["maxRoles"].as_i64().map(|m| m as i32));
        assert_eq!(got, c["text"].as_str().unwrap());
    }
    for c in g["customIds"].as_array().unwrap() {
        let (gid, rid) = (c["groupId"].as_str().unwrap(), c["roleId"].as_str().unwrap());
        let d = rr::draft_item(gid, &rr::NewItem { role_id: rid.into(), label: "L".into(), ..Default::default() }, 4).unwrap();
        assert_eq!(serde_json::to_value(&d).unwrap(), c["item"]);
        assert_eq!(rr::select_custom_id(gid), c["select"].as_str().unwrap());
        // the handlers parse what the JS built
        let parsed = bot_config::custom_ids::parse_custom_id(&d.custom_id);
        assert_eq!((parsed.prefix, parsed.params), ("rr", vec![gid, rid]));
    }
    assert_eq!(rr::index_key("900000000000000001"), g["indexKey"].as_str().unwrap());
    assert_eq!(rr::rate_key("1", "2"), "rr:rate:1:2");
}

#[test]
fn list_pages_match_the_js() {
    let g = &golden()["listPages"];
    let rows: Vec<rr::PanelListRow> = serde_json::from_value(g["panels"].clone()).unwrap();
    let settings = bot_core::settings::MemorySettingsStore::new_row("900000000000000001", json!({ "embedColor": 0x123456 }).as_object().unwrap().clone()).unwrap();
    let settings = Arc::new(settings);
    let pages = feat_reactionroles::commands::reactionrole::build_list_pages(&rows, "900000000000000001", 25, Some(&settings));
    let want = g["pages"].as_array().unwrap();
    assert_eq!(pages.len(), want.len());
    for (p, w) in pages.iter().zip(want) {
        let j = serde_json::to_value(p).unwrap();
        assert_eq!(j["title"], w["title"]);
        assert_eq!(j["color"], w["color"]);
        let names: Vec<(&Value, &Value)> = j["fields"].as_array().unwrap().iter().map(|f| (&f["name"], &f["value"])).collect();
        let wnames: Vec<(&Value, &Value)> = w["fields"].as_array().unwrap().iter().map(|f| (&f["name"], &f["value"])).collect();
        assert_eq!(names, wnames);
    }
    assert!(feat_reactionroles::commands::reactionrole::build_list_pages(&[], "1", 0, None).is_empty());
}

// ── Golden: templates ─────────────────────────────────────────────────────────

#[test]
fn templates_match_the_js() {
    let g = golden();
    for key in ["verify", "notifications"] {
        let t = rr::rr_template(key).unwrap();
        let w = &g["rrTemplates"][key];
        assert_eq!(t.kind, w["type"].as_str().unwrap());
        assert_eq!(t.layout, w["layout"]);
        assert_eq!(t.title, w["title"]);
        for l in ["BUTTONS", "SELECT", "REACTIONS"] {
            assert_eq!(t.description_for(l).unwrap(), w["description"][l].as_str().unwrap());
        }
        assert_eq!(json!({ "label": t.item.label, "emoji": t.item.emoji, "style": t.item.style }), w["item"]);
    }
    let got: Vec<Value> = role_templates()
        .iter()
        .map(|t| {
            json!({
                "key": t.key, "label": t.label, "note": t.note, "linksZones": t.links_zones,
                "panel": { "type": t.panel.kind, "layout": t.panel.layout, "title": t.panel.title, "description": t.panel.description, "placeholder": t.panel.placeholder },
                "roles": t.roles.iter().map(|r| json!({ "name": r.name, "label": r.label, "emoji": r.emoji, "color": r.color, "description": r.description, "zone": r.zone })).collect::<Vec<_>>(),
            })
        })
        .collect();
    assert_eq!(Value::Array(got), g["roleTemplates"]);
    for t in role_templates() {
        for r in &t.roles {
            if let Some(e) = r.emoji {
                assert!(rr::is_unicode_emoji(e), "{}: {e}", t.key);
            }
        }
    }
    assert!(feat_reactionroles::role_templates::get_role_template("verify").is_none());
    assert!(feat_reactionroles::role_templates::get_role_template("__proto__").is_none());
}

#[test]
fn template_helpers_match_the_js() {
    let g = golden();
    for c in g["describeSetup"].as_array().unwrap() {
        assert_eq!(setup::describe_setup(c["created"].as_u64().unwrap() as usize, c["reused"].as_u64().unwrap() as usize), c["text"].as_str().unwrap());
    }
    for c in g["describeReused"].as_array().unwrap() {
        let roles: Vec<setup::SetupRole> = c["roles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| setup::SetupRole {
                role_id: "1".into(),
                name: r["name"].as_str().unwrap().into(),
                label: String::new(),
                description: None,
                emoji: None,
                zone: None,
                created: r["created"].as_bool().unwrap(),
            })
            .collect();
        assert_eq!(setup::describe_reused(&roles).as_deref(), opt_str(&c["text"]));
    }
    for c in g["pastTemplates"].as_array().unwrap() {
        let zones: Vec<Option<String>> = c["zones"].as_array().unwrap().iter().map(|z| z.as_str().map(str::to_string)).collect();
        assert_eq!(setup::match_past_template(&zones), opt_str(&c["key"]), "{}", c["zones"]);
    }
    for c in g["orderItems"].as_array().unwrap() {
        let roles = strs(&c["roles"]);
        let got = rr::order_items(&roles, &strs(&c["order"]), |r| r.as_str());
        assert_eq!(got, strs(&c["result"]));
    }
}

// ── In-process state (tests 11, 12) ───────────────────────────────────────────

const G1: &str = "955019000000000070";

fn lite(id: &str, message_id: &str, layout: &str) -> PanelLite {
    PanelLite {
        id: id.into(),
        channel_id: "c".into(),
        message_id: message_id.into(),
        title: "T".into(),
        kind: "NORMAL".into(),
        layout: layout.into(),
        max_roles: None,
        attached: false,
        items: Vec::new(),
    }
}

#[tokio::test]
async fn reaction_suppression_expires() {
    let t = TestApp::new();
    rr::suppress_reaction_removal(&t, "m1", "u1", "k");
    assert!(rr::consume_reaction_suppression(&t, "m1", "u1", "k"));
    assert!(!rr::consume_reaction_suppression(&t, "m1", "u1", "k"));
    assert!(!rr::consume_reaction_suppression(&t, "m1", "u2", "k"));
    rr::suppress_reaction_removal(&t, "m2", "u1", "k");
    t.clock.advance(rr::SUPPRESS_TTL_MS + 1);
    assert!(!rr::consume_reaction_suppression(&t, "m2", "u1", "k"));
    rr::suppress_reaction_removal(&t, "m3", "u1", "k");
    t.clock.advance(rr::SUPPRESS_TTL_MS - 1);
    assert!(rr::consume_reaction_suppression(&t, "m3", "u1", "k"));
}

#[tokio::test]
async fn hook_filters_follow_the_panel_index() {
    let t = TestApp::new();
    assert!(!rr::reaction_hook_filter(&t, None, Some("m")));
    assert!(!rr::reaction_hook_filter(&t, Some(G1), None));
    assert!(rr::reaction_hook_filter(&t, Some(G1), Some("m1")), "unknown memo -> run loads it");
    assert!(rr::message_delete_filter(&t, Some(G1), Some("m1")));
    rr::set_panel_index(&t, G1, Vec::new());
    assert!(!rr::reaction_hook_filter(&t, Some(G1), Some("m1")));
    assert!(!rr::message_delete_filter(&t, Some(G1), Some("m1")));
    rr::set_panel_index(&t, G1, vec![lite("p1", "rm", "REACTIONS"), lite("p2", "bm", "BUTTONS")]);
    assert!(rr::reaction_hook_filter(&t, Some(G1), Some("rm")));
    assert!(!rr::reaction_hook_filter(&t, Some(G1), Some("bm")));
    assert!(!rr::reaction_hook_filter(&t, Some(G1), Some("other")));
    assert!(rr::message_delete_filter(&t, Some(G1), Some("bm")));
    assert!(!rr::message_delete_filter(&t, Some(G1), Some("other")));
    t.clock.advance(rr::INDEX_MEMO_TTL_MS + 1);
    assert!(rr::reaction_hook_filter(&t, Some(G1), Some("other")), "a stale memo makes run reload it");
    assert!(!rr::peek_panel_index(&t, G1).unwrap().fresh);
    rr::reset_panel_index(&t);
    assert!(rr::peek_panel_index(&t, G1).is_none());
}

#[test]
fn cached_index_values_are_read_the_js_way() {
    let ok = json!([{ "id": "p", "channelId": "c", "messageId": "m", "title": "T", "type": "NORMAL", "layout": "REACTIONS", "maxRoles": null,
        "attached": false, "items": [{ "roleId": "r", "emoji": "❤️", "key": "❤" }] }]);
    let parsed = rr::parse_cached_index(&ok).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), ok, "written back JSON-equal, key order included");
    assert_eq!(serde_json::to_string(&parsed).unwrap(), serde_json::to_string(&ok).unwrap());
    assert!(rr::parse_cached_index(&json!([{ "id": 1, "messageId": "m", "items": [] }])).is_none());
    assert!(rr::parse_cached_index(&json!([{ "id": "p", "messageId": "m" }])).is_none());
    assert!(rr::parse_cached_index(&json!({ "id": "p" })).is_none());
    assert!(rr::parse_cached_index(&json!("text")).is_none());
}

// ── Role changes on fake Discord objects (tests 17-20) ────────────────────────

fn ops_app(guild: &str) -> (TestApp, FakeOps) {
    let t = TestApp::new().with_guilds([guild]);
    let ops = FakeOps::new();
    rr::set_discord_ops(&t, Arc::new(ops.clone()));
    (t, ops)
}

#[tokio::test]
async fn apply_role_change_adds_first_and_stops_on_a_failed_add() {
    let ops = FakeOps::new();
    ops.member("u1", &["R2"]);
    let plan = Plan { to_add: vec!["R1".into()], to_remove: vec!["R2".into()], ..Default::default() };
    let res = rr::apply_role_change(&ops, "g", "u1", &["R2".to_string()], &plan, "why").await;
    assert_eq!(ops.role_calls(), vec![("add".into(), "R1".into()), ("remove".into(), "R2".into())]);
    assert_eq!((res.added, res.removed, res.failed), (vec!["R1".to_string()], vec!["R2".to_string()], None));
    assert!(ops.w().calls.iter().all(|c| c.3 == "why"));

    let stuck = FakeOps::new();
    stuck.member("u2", &["R2"]);
    stuck.w().fail_role.insert("R1".into(), 50013);
    let failed = rr::apply_role_change(&stuck, "g", "u2", &["R2".to_string()], &plan, "x").await;
    assert_eq!(failed.failed, Some(rr::RoleFailure { role_id: "R1".into(), op: "add", code: Some(50013) }));
    assert!(stuck.role_calls().is_empty(), "a failed swap keeps the old role (R162)");

    let idem = FakeOps::new();
    let same = rr::apply_role_change(&idem, "g", "u3", &["R1".to_string()], &plan, "x").await;
    assert!(idem.role_calls().is_empty());
    assert!(same.added.is_empty() && same.removed.is_empty());
}

#[test]
fn check_roles_usable_splits_missing_and_blocked() {
    let ops = FakeOps::new();
    ops.role("R1", "ok", 1).role("R2", "high", 60);
    let ids: Vec<String> = ["R1", "R2", "R3"].iter().map(|s| s.to_string()).collect();
    let u = rr::check_roles_usable(&ops, "g", &ids);
    assert_eq!((u.missing, u.blocked, u.no_manage_roles), (vec!["R3".to_string()], vec!["R2".to_string()], false));
    ops.w().manage_roles = false;
    let none = rr::check_roles_usable(&ops, "g", &ids[..1]);
    assert_eq!((none.missing, none.blocked, none.no_manage_roles), (Vec::<String>::new(), vec!["R1".to_string()], true));
}

const EMOJIS: [&str; 4] = ["🔔", "🎉", "🎮", "⭐"];

/// `reactionWorld(n, ...)`: a REACTIONS panel of four roles in a primed index, no database.
fn reaction_world(n: u64, kind: &str, max_roles: Option<i64>) -> (TestApp, FakeOps, String, String, Vec<String>) {
    let guild = format!("9550190000000{n:05}");
    let (t, ops) = ops_app(&guild);
    let channel = format!("3000000000000{n:05}");
    let message = format!("5000000000009{n:05}");
    ops.channel(&channel, "roles").message(&guild, &channel, &message);
    let roles: Vec<String> = (1..=4).map(|i| format!("4000000000000{:05}", n * 10 + i)).collect();
    for (i, r) in roles.iter().enumerate() {
        ops.role(r, &format!("Role {}", i + 1), 1);
    }
    let items = roles
        .iter()
        .zip(EMOJIS)
        .map(|(r, e)| rr::IndexItem { role_id: r.clone(), emoji: Some(e.into()), key: rr::emoji_key_of_stored(Some(e)) })
        .collect();
    let panel = PanelLite {
        id: format!("panel{n}"),
        channel_id: channel.clone(),
        message_id: message.clone(),
        title: "Panel".into(),
        kind: kind.into(),
        layout: "REACTIONS".into(),
        max_roles,
        attached: false,
        items,
    };
    rr::set_panel_index(&t, &guild, vec![panel]);
    (t, ops, guild, message, roles)
}

async fn react(t: &TestApp, ops: &FakeOps, guild: &str, message: &str, user: &str, emoji: &str, source: Source) {
    let channel = ops.w().messages.get(message).map(|m| m.channel_id.clone()).unwrap_or_default();
    ops.add_reaction(message, emoji, false);
    let roles: Option<Vec<String>> = ops.w().members.get(user).map(|s| s.iter().cloned().collect());
    let input = rr::ReactionInput { guild_id: guild.into(), channel_id: channel, message_id: message.into(), user_id: user.into(), emoji_key: emoji.into() };
    rr::process_reaction(t, input, source, move || async move { roles }).await;
}

#[tokio::test]
async fn reaction_hook_normal_react_adds_and_unreact_removes() {
    let (t, ops, g, m, roles) = reaction_world(11, "NORMAL", None);
    ops.member("u1", &[]);
    react(&t, &ops, &g, &m, "u1", "🔔", Source::React).await;
    assert_eq!(ops.role_calls(), vec![("add".into(), roles[0].clone())]);
    assert!(ops.w().calls[0].3.starts_with("Reaction role: Panel"));
    react(&t, &ops, &g, &m, "u1", "🔔", Source::Unreact).await;
    assert_eq!(ops.role_calls().len(), 2);
    assert_eq!(ops.role_calls()[1], ("remove".into(), roles[0].clone()));
    // a foreign emoji and a foreign message are left alone
    react(&t, &ops, &g, &m, "u1", "🍕", Source::React).await;
    react(&t, &ops, &g, "500000000000000001", "u1", "🔔", Source::React).await;
    assert_eq!(ops.role_calls().len(), 2);
    // the user left
    react(&t, &ops, &g, &m, "nobody", "🔔", Source::React).await;
    assert_eq!(ops.role_calls().len(), 2);
}

#[tokio::test]
async fn reaction_hook_unique_swaps_and_ignores_its_own_removal() {
    let (t, ops, g, m, roles) = reaction_world(12, "UNIQUE", None);
    ops.member("u2", &[&roles[1]]);
    ops.add_reaction(&m, "🎉", false);
    react(&t, &ops, &g, &m, "u2", "🔔", Source::React).await;
    assert_eq!(ops.role_calls(), vec![("add".into(), roles[0].clone()), ("remove".into(), roles[1].clone())], "adds before removes");
    assert_eq!(ops.w().removed_user, vec![(m.clone(), "🎉".to_string(), "u2".to_string())], "the swapped-out reaction is taken back");
    let before = ops.role_calls().len();
    react(&t, &ops, &g, &m, "u2", "🎉", Source::Unreact).await; // the bot's own removal: no role logic
    assert_eq!(ops.role_calls().len(), before);
    react(&t, &ops, &g, &m, "u2", "🔔", Source::Unreact).await;
    assert_eq!(ops.role_calls().len(), before + 1);
}

#[tokio::test]
async fn reaction_hook_failed_unique_add_keeps_the_old_role() {
    let (t, ops, g, m, roles) = reaction_world(13, "UNIQUE", None);
    ops.member("u3", &[&roles[1]]);
    ops.w().fail_role.insert(roles[0].clone(), 50013);
    react(&t, &ops, &g, &m, "u3", "🔔", Source::React).await;
    assert!(ops.role_calls().is_empty());
    assert!(ops.member_roles_now("u3").contains(&roles[1]));
}

#[tokio::test]
async fn reaction_hook_role_cap_takes_the_reaction_back() {
    let (t, ops, g, m, roles) = reaction_world(14, "NORMAL", Some(1));
    ops.member("u4", &[&roles[1]]);
    react(&t, &ops, &g, &m, "u4", "🔔", Source::React).await;
    assert!(ops.role_calls().is_empty());
    assert_eq!(ops.w().removed_user, vec![(m.clone(), "🔔".to_string(), "u4".to_string())]);
    assert!(rr::consume_reaction_suppression(&t, &m, "u4", "🔔"), "the removal is marked as ours");
    // without Manage Messages the reaction stays and nothing else happens
    let (t2, ops2, g2, m2, roles2) = reaction_world(15, "NORMAL", Some(1));
    let ch = ops2.w().messages[&m2].channel_id.clone();
    ops2.w().no_manage_messages.insert(ch);
    ops2.member("u5", &[&roles2[1]]);
    react(&t2, &ops2, &g2, &m2, "u5", "🔔", Source::React).await;
    assert!(ops2.w().removed_user.is_empty());
    assert!(ops2.role_calls().is_empty());
}

#[tokio::test]
async fn reaction_hook_verify_and_reverse() {
    let (t, ops, g, m, _roles) = reaction_world(16, "VERIFY", None);
    ops.member("u6", &[]);
    react(&t, &ops, &g, &m, "u6", "🔔", Source::React).await;
    react(&t, &ops, &g, &m, "u6", "🔔", Source::Unreact).await;
    assert_eq!(ops.role_calls().iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["add"], "unreacting never removes a VERIFY role");
    let (t2, ops2, g2, m2, roles2) = reaction_world(17, "REVERSE", None);
    ops2.member("u7", &[&roles2[0]]);
    react(&t2, &ops2, &g2, &m2, "u7", "🔔", Source::React).await;
    react(&t2, &ops2, &g2, &m2, "u7", "🔔", Source::Unreact).await;
    assert_eq!(ops2.role_calls().iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["remove", "add"]);
}

#[tokio::test]
async fn reaction_hook_unmanageable_and_unverifiable_roles_change_nothing() {
    let (t, ops, g, m, roles) = reaction_world(18, "NORMAL", None);
    ops.w().roles.iter_mut().find(|r| r.id == roles[0]).unwrap().editable = false;
    ops.member("u8", &[]);
    react(&t, &ops, &g, &m, "u8", "🔔", Source::React).await;
    assert!(ops.role_calls().is_empty());
    // the role is missing from the cache and REST cannot tell: not pruned, not assigned
    let (t2, ops2, g2, m2, roles2) = reaction_world(19, "NORMAL", None);
    ops2.w().roles.retain(|r| r.id != roles2[0]);
    ops2.w().role_fetch_error = true;
    ops2.member("u9", &[]);
    react(&t2, &ops2, &g2, &m2, "u9", "🔔", Source::React).await;
    assert!(ops2.role_calls().is_empty());
    // the index is unknown and cannot be loaded (no database): fail closed
    rr::reset_panel_index(&t2);
    ops2.w().role_fetch_error = false;
    ops2.w().roles.push(rr::RoleInfo { id: roles2[0].clone(), name: "x".into(), position: 1, managed: false, editable: true });
    react(&t2, &ops2, &g2, &m2, "u9", "🔔", Source::React).await;
    assert!(ops2.role_calls().is_empty());
}

#[tokio::test]
async fn clear_reaction_hooks_re_add_the_bot_reactions() {
    let (t, ops, g, m, _roles) = reaction_world(22, "NORMAL", None);
    let ch = ops.w().messages[&m].channel_id.clone();
    rr::restore_all_reactions(&t, &g, &ch, &m).await;
    assert_eq!(ops.w().reacted.iter().map(|r| r.1.clone()).collect::<Vec<_>>(), EMOJIS.to_vec(), "every item emoji is back, in order");
    rr::restore_all_reactions(&t, &g, &ch, &m).await;
    assert_eq!(ops.w().reacted.len(), 4, "throttled and already present");
    let (t2, ops2, g2, m2, _r) = reaction_world(23, "NORMAL", None);
    let ch2 = ops2.w().messages[&m2].channel_id.clone();
    rr::restore_emoji_reaction(&t2, &g2, &ch2, &m2, "🎉").await;
    rr::restore_emoji_reaction(&t2, &g2, &ch2, &m2, "🍕").await;
    assert_eq!(ops2.w().reacted.iter().map(|r| r.1.clone()).collect::<Vec<_>>(), vec!["🎉"]);
}

#[tokio::test]
async fn reaction_helpers_ensure_remove_and_remove_own() {
    let (t, ops) = ops_app("955019000000000003");
    let g = "955019000000000003";
    ops.channel("300000000000000040", "roles").message(g, "300000000000000040", "500000000000000040");
    let msg = || ops.w().messages["500000000000000040"].clone();
    let items = |es: &[&str]| es.iter().map(|e| rr::IndexItem { role_id: "r".into(), emoji: Some(e.to_string()), key: None }).collect::<Vec<_>>();
    let res = rr::ensure_panel_reactions(&t, &msg(), &items(&["🔔", ":x:", "🎉"])).await;
    assert_eq!(res.added, 2);
    assert_eq!(ops.w().reacted.iter().map(|r| r.1.clone()).collect::<Vec<_>>(), vec!["🔔", "🎉"]);
    ops.w().react_error = Some(10014);
    let failed = rr::ensure_panel_reactions(&t, &msg(), &items(&["⭐"])).await;
    assert_eq!(failed.failed, vec![rr::ReactFailure { emoji: "⭐".into(), code: Some(10014), reason: "I can't use that emoji".into() }]);
    ops.w().react_error = None;
    assert_eq!(rr::remove_panel_reaction(&t, g, &msg(), "🔔").await, rr::RemoveResult { ok: true, all: true });
    assert_eq!(ops.w().removed_all, vec![("500000000000000040".to_string(), "🔔".to_string())]);
    ops.w().no_manage_messages.insert("300000000000000040".into());
    assert_eq!(rr::remove_panel_reaction(&t, g, &msg(), "🎉").await, rr::RemoveResult { ok: true, all: false });
    assert_eq!(ops.w().removed_user.last().unwrap().2, BOT);
    assert_eq!(rr::remove_panel_reaction(&t, g, &msg(), "🍕").await, rr::RemoveResult { ok: true, all: false });
    for r in ops.w().messages.get_mut("500000000000000040").unwrap().reactions.iter_mut() {
        r.me = true;
    }
    rr::remove_own_reactions(&t, &msg(), &items(&["🎉"])).await;
    assert_eq!(ops.w().removed_user.len(), 2);
}

// ── Role templates (test/role-templates.test.js) ──────────────────────────────

const TG: &str = "900000000000000001";

fn template_app(existing: &[(&str, i64)]) -> (TestApp, FakeOps) {
    let (t, ops) = ops_app(TG);
    ops.role(TG, "@everyone", 0);
    for (i, (name, pos)) in existing.iter().enumerate() {
        ops.role(&format!("91000000000000{:04}", i + 1), name, *pos);
    }
    (t, ops)
}

fn put_settings(t: &TestApp, fields: Value) {
    let row = bot_core::settings::MemorySettingsStore::new_row(TG, fields.as_object().unwrap().clone()).unwrap();
    t.settings_store.put(row);
}

fn tz_links(t: &TestApp) -> Vec<(String, String)> {
    let row = t.settings_store.row(TG).unwrap();
    bot_util::timezones::normalize_timezone_roles(row.timezone_roles.as_ref().unwrap_or(&Value::Null)).into_iter().map(|l| (l.role_id, l.zone)).collect()
}

fn created_names(ops: &FakeOps) -> Vec<String> {
    ops.w().created_roles.iter().map(|b| b["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn setup_creates_missing_roles_reuses_by_name_and_links_every_one() {
    let (t, ops) = template_app(&[("cst", 3), ("Members", 2)]);
    put_settings(&t, json!({ "timezoneRoles": [{ "roleId": "999999999999999999", "zone": "UTC" }] }));
    let out = setup::setup_role_template(&t, TG, "timezones-us", None, "test").await.unwrap();
    assert_eq!(out.roles.len(), 10);
    assert_eq!((out.created, out.reused), (9, 1));
    assert_eq!(created_names(&ops), vec!["SST", "HST", "AKST", "PST", "MST (AZ)", "MST", "EST", "AST (PR)", "ChST"]);
    for b in ops.w().created_roles.iter() {
        assert_eq!((&b["permissions"], &b["mentionable"], &b["hoist"]), (&json!("0"), &json!(false), &json!(false)));
        assert!(b.get("colors").is_none());
    }
    let cst = out.roles.iter().find(|r| r.name == "CST").unwrap();
    assert!(!cst.created);
    assert_eq!(ops.cached_role_name(&cst.role_id), "cst");
    // The deleted role's old link is dropped; each template role is linked to its zone.
    let want: Vec<(String, String)> = out.roles.iter().map(|r| (r.role_id.clone(), r.zone.clone().unwrap())).collect();
    assert_eq!(tz_links(&t), want);
    assert_eq!(setup::describe_setup(out.created, out.reused), "Created 9 roles and reused 1 that already existed.");
    let again = setup::setup_role_template(&t, TG, "timezones-us", None, "x").await.unwrap();
    assert_eq!((again.created, again.reused), (0, 10));
    assert_eq!(ops.w().created_roles.len(), 9);
    assert_eq!(tz_links(&t).len(), 10);
}

trait RoleName {
    fn cached_role_name(&self, id: &str) -> String;
}

impl RoleName for FakeOps {
    fn cached_role_name(&self, id: &str) -> String {
        self.w().roles.iter().find(|r| r.id == id).map(|r| r.name.clone()).unwrap_or_default()
    }
}

async fn refuses(t: &TestApp, ops: &FakeOps, key: &str, pattern: &str) {
    let err = setup::setup_role_template(t, TG, key, None, "r").await.unwrap_err();
    let e = err.downcast_ref::<RoleTemplateError>().unwrap_or_else(|| panic!("not a RoleTemplateError: {err}"));
    assert!(e.0.contains(pattern), "{} does not contain {pattern}", e.0);
    assert!(ops.w().created_roles.is_empty(), "{pattern}");
}

#[tokio::test]
async fn setup_refuses_before_creating_anything() {
    let (t, ops) = template_app(&[]);
    put_settings(&t, json!({}));
    ops.w().manage_roles = false;
    refuses(&t, &ops, "timezones-us", "Manage Roles").await;
    let (t, ops) = template_app(&[("EST", 60)]);
    put_settings(&t, json!({}));
    refuses(&t, &ops, "timezones-us", "The existing **EST** role can't be used: That role is above my highest role.").await;
    let (t, ops) = template_app(&[("PST", 4)]);
    put_settings(&t, json!({}));
    let pst = ops.w().roles.iter().find(|r| r.name == "PST").unwrap().id.clone();
    ops.w().dangerous.insert(pst);
    refuses(&t, &ops, "timezones-us", "dangerous").await;
    let names: Vec<(String, i64)> = (0..23).map(|i| (format!("zone{i}"), 1)).collect();
    let refs: Vec<(&str, i64)> = names.iter().map(|(n, p)| (n.as_str(), *p)).collect();
    let (t, ops) = template_app(&refs);
    let links: Vec<Value> = ops.w().roles.iter().filter(|r| r.name.starts_with("zone")).map(|r| json!({ "roleId": r.id, "zone": "UTC" })).collect();
    put_settings(&t, json!({ "timezoneRoles": links }));
    refuses(&t, &ops, "timezones-us", "most is 25").await;
    let (t, ops) = template_app(&[]);
    put_settings(&t, json!({}));
    refuses(&t, &ops, "nope", "does not exist").await;
    // near the role cap, and a role above the bot for a role set
    let many: Vec<(String, i64)> = (0..245).map(|i| (format!("role{i}"), 1)).collect();
    let refs: Vec<(&str, i64)> = many.iter().map(|(n, p)| (n.as_str(), *p)).collect();
    let (t, ops) = template_app(&refs);
    refuses(&t, &ops, "colors", "limit of 250").await;
    let (t, ops) = template_app(&[("They/Them", 60)]);
    refuses(&t, &ops, "pronouns", "They/Them").await;
}

#[tokio::test]
async fn a_discord_refusal_names_the_roles_already_created() {
    let (t, ops) = template_app(&[]);
    put_settings(&t, json!({}));
    ops.w().create_fail = Some("PST".into());
    let err = setup::setup_role_template(&t, TG, "timezones-us", None, "r").await.unwrap_err();
    let msg = err.downcast_ref::<RoleTemplateError>().unwrap().0.clone();
    assert!(msg.contains("**PST**") && msg.contains("Already created: SST, HST, AKST"), "{msg}");
    assert!(tz_links(&t).is_empty(), "nothing is linked after a failure");
}

#[test]
fn find_role_by_name_ignores_case_everyone_and_integrations() {
    let roles = vec![
        rr::RoleInfo { id: TG.into(), name: "@everyone".into(), position: 0, managed: false, editable: false },
        rr::RoleInfo { id: "1".into(), name: "CST".into(), position: 2, managed: false, editable: true },
        rr::RoleInfo { id: "2".into(), name: "cst".into(), position: 7, managed: false, editable: true },
        rr::RoleInfo { id: "3".into(), name: "EST".into(), position: 1, managed: true, editable: false },
    ];
    assert_eq!(setup::find_role_by_name(&roles, TG, "Cst").unwrap().position, 7);
    assert!(setup::find_role_by_name(&roles, TG, "EST").is_none());
    assert!(setup::find_role_by_name(&roles, TG, "@everyone").is_none());
}

#[tokio::test]
async fn a_role_set_creates_colored_roles_and_writes_no_settings() {
    let (t, ops) = template_app(&[("red", 3)]);
    put_settings(&t, json!({}));
    let out = setup::setup_role_template(&t, TG, "colors", None, "test").await.unwrap();
    let colors = feat_reactionroles::role_templates::get_role_template("colors").unwrap().roles;
    assert_eq!((out.created, out.reused), (colors.len() - 1, 1));
    assert_eq!(created_names(&ops), colors[1..].iter().map(|r| r.name.to_string()).collect::<Vec<_>>());
    for b in ops.w().created_roles.iter() {
        let want = colors.iter().find(|r| r.name == b["name"]).unwrap().color;
        assert_eq!(b["colors"]["primary_color"], json!(want));
    }
    assert_eq!(out.roles.iter().map(|r| r.emoji.clone()).collect::<Vec<_>>(), colors.iter().map(|r| r.emoji.map(str::to_string)).collect::<Vec<_>>());
    assert!(t.settings_store.row(TG).unwrap().timezone_roles.is_none());
    let reused = setup::describe_reused(&out.roles).unwrap();
    assert!(reused.starts_with("Existing role now on the panel: **Red**. ") && reused.contains("check it doesn't unlock"));
}

// ── upgradeTimezonePanels with a fake panel store ─────────────────────────────

#[derive(Default)]
struct FakeStore {
    panel: Mutex<Option<rr::Panel>>,
    list_calls: Mutex<usize>,
    inserts: Mutex<Vec<(Vec<rr::NewItem>, Vec<String>)>>,
    synced: Mutex<Vec<String>>,
    invalidated: Mutex<usize>,
}

#[async_trait]
impl PanelStore for FakeStore {
    async fn list(&self, _app: &bot_core::App, _g: &str) -> anyhow::Result<Vec<rr::Panel>> {
        *self.list_calls.lock().unwrap() += 1;
        Ok(self.panel.lock().unwrap().clone().into_iter().collect())
    }
    async fn insert_items(&self, _app: &bot_core::App, panel: &rr::Panel, items: &[rr::NewItem], order: &[String]) -> anyhow::Result<()> {
        self.inserts.lock().unwrap().push((items.to_vec(), order.to_vec()));
        let mut all: Vec<bot_db::ReactionRoleItem> = panel.items.clone();
        for it in items {
            all.push(item_row(&it.role_id, &it.label));
        }
        let mut sorted = rr::order_items(&all, order, |i| i.role_id.as_str());
        for (i, it) in sorted.iter_mut().enumerate() {
            it.position = i as i32;
        }
        self.panel.lock().unwrap().as_mut().unwrap().items = sorted;
        Ok(())
    }
    async fn invalidate(&self, _app: &bot_core::App, _g: &str) {
        *self.invalidated.lock().unwrap() += 1;
    }
    async fn sync(&self, _app: &bot_core::App, _g: &str, id: &str, _c: Option<i64>) -> rr::SyncResult {
        self.synced.lock().unwrap().push(id.into());
        rr::SyncResult { status: "ok", code: None, reason: None }
    }
}

fn item_row(role: &str, label: &str) -> bot_db::ReactionRoleItem {
    bot_db::ReactionRoleItem {
        id: format!("item-{role}"),
        group_id: "panel-1".into(),
        custom_id: format!("rr:panel-1:{role}"),
        role_id: role.into(),
        label: label.into(),
        emoji: None,
        style: 2,
        position: 0,
        description: None,
    }
}

fn panel_row(layout: &str, items: Vec<bot_db::ReactionRoleItem>) -> rr::Panel {
    let group: bot_db::ReactionRoleGroup = serde_json::from_value(json!({
        "id": "panel-1", "guildId": TG, "channelId": "300000000000000001", "messageId": "500000000000000001", "title": "TZ",
        "description": null, "maxRoles": null, "type": "UNIQUE", "layout": layout, "plainText": false, "messageContent": null,
        "attached": false, "placeholder": null, "messageMissingAt": null, "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z",
    }))
    .unwrap();
    rr::Panel { group, items }
}

fn past(key: &str) -> Vec<&'static str> {
    bot_util::timezones::TIMEZONE_TEMPLATE_PAST_ZONES.iter().find(|(k, _)| *k == key).unwrap().1[0].to_vec()
}

fn name_of_zone(zone: &str) -> &'static str {
    bot_util::timezones::timezone_role_template("timezones-world").unwrap().roles.iter().find(|r| r.zone == zone).unwrap().name
}

/// `pastPanelGuild(zones, ...)`: a guild holding the roles of a panel posted from a past template, linked to their zones.
fn past_panel_guild(zones: &[&str], layout: &str, extra: &[(&str, i64)], extra_links: Vec<Value>) -> (TestApp, FakeOps, FakeStore, Vec<String>) {
    let (t, ops) = template_app(extra);
    let mut ids = Vec::new();
    for (i, z) in zones.iter().enumerate() {
        let id = format!("92000000000000{:04}", i + 1);
        ops.role(&id, name_of_zone(z), 1);
        ids.push(id);
    }
    let mut links: Vec<Value> = ids.iter().zip(zones).map(|(id, z)| json!({ "roleId": id, "zone": z })).collect();
    links.extend(extra_links);
    put_settings(&t, json!({ "timezoneRoles": links }));
    let items: Vec<bot_db::ReactionRoleItem> = ids.iter().zip(zones).map(|(id, z)| item_row(id, name_of_zone(z))).collect();
    let store = FakeStore { panel: Mutex::new(Some(panel_row(layout, items))), ..Default::default() };
    (t, ops, store, ids)
}

#[tokio::test]
async fn upgrade_adds_the_new_roles_in_order_and_a_second_run_does_nothing() {
    let (t, ops, store, _) = past_panel_guild(&past("timezones-us"), "SELECT", &[], vec![json!({ "roleId": "999999999999999999", "zone": "UTC" })]);
    assert_eq!(setup::upgrade_timezone_panels_with(&t, TG, &store).await, 1);
    assert_eq!(created_names(&ops), vec!["SST", "MST (AZ)", "AST (PR)", "ChST"]);
    let us = bot_util::timezones::timezone_role_template("timezones-us").unwrap();
    let links = tz_links(&t);
    assert_eq!(links.len(), 10, "the deleted role link is dropped");
    let zone_of = |id: &str| links.iter().find(|l| l.0 == id).map(|l| l.1.clone()).unwrap();
    let inserts = store.inserts.lock().unwrap().clone();
    assert_eq!(inserts[0].1.iter().map(|id| zone_of(id)).collect::<Vec<_>>(), us.roles.iter().map(|r| r.zone.to_string()).collect::<Vec<_>>());
    let chst = inserts[0].0.iter().find(|i| i.label == "Chamorro (ChST)").unwrap();
    assert_eq!((chst.style, chst.emoji.clone(), chst.description.clone()), (Some(2), None, Some("Guam, Northern Mariana Islands".to_string())));
    assert_eq!((store.synced.lock().unwrap().clone(), *store.invalidated.lock().unwrap()), (vec!["panel-1".to_string()], 1));
    assert_eq!(setup::upgrade_timezone_panels_with(&t, TG, &store).await, 0);
    assert_eq!(ops.w().created_roles.len(), 4);
    assert_eq!(store.inserts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn upgrade_world_panel_reuses_an_unlinked_role() {
    let (t, ops, store, _) = past_panel_guild(&past("timezones-world"), "BUTTONS", &[("chst", 5)], Vec::new());
    assert_eq!(setup::upgrade_timezone_panels_with(&t, TG, &store).await, 1);
    assert_eq!(created_names(&ops), vec!["MST (AZ)", "CST (MX)", "AST (PR)", "PKT"]);
    assert_eq!(tz_links(&t).len(), 24);
    assert!(store.inserts.lock().unwrap()[0].0.iter().all(|i| i.description.is_none()), "buttons have no descriptions");
    assert_eq!(store.panel.lock().unwrap().as_ref().unwrap().items.len(), 24);
}

#[tokio::test]
async fn upgrade_leaves_changed_panels_and_conflicts_alone() {
    async fn untouched(t: &TestApp, ops: &FakeOps, store: &FakeStore, listed: bool) {
        let before = tz_links(t);
        assert_eq!(setup::upgrade_timezone_panels_with(t, TG, store).await, 0);
        assert!(ops.w().created_roles.is_empty());
        assert!(store.inserts.lock().unwrap().is_empty());
        assert_eq!(tz_links(t), before);
        assert_eq!(*store.list_calls.lock().unwrap(), usize::from(listed));
    }
    let us = past("timezones-us");
    let (t, o, s, _) = past_panel_guild(&us[1..], "SELECT", &[], Vec::new());
    untouched(&t, &o, &s, false).await;
    let world = past("timezones-world");
    let (t, o, s, _) = past_panel_guild(&world[1..], "SELECT", &[], Vec::new());
    untouched(&t, &o, &s, true).await;
    let mut swapped: Vec<&str> = us[..5].to_vec();
    swapped.push("Europe/London");
    let (t, o, s, _) = past_panel_guild(&swapped, "SELECT", &[], Vec::new());
    untouched(&t, &o, &s, true).await;
    let (t, o, s, _) = past_panel_guild(&us, "SELECT", &[("ChST", 5)], Vec::new());
    let chst = o.w().roles.iter().find(|r| r.name == "ChST").unwrap().id.clone();
    let mut links: Vec<Value> = t.settings_store.row(TG).unwrap().timezone_roles.unwrap().as_array().unwrap().clone();
    links.push(json!({ "roleId": chst, "zone": "Asia/Tokyo" }));
    put_settings(&t, json!({ "timezoneRoles": links }));
    untouched(&t, &o, &s, true).await;
    let (t, o, s, _) = past_panel_guild(&us, "SELECT", &[("SST", 60)], Vec::new());
    untouched(&t, &o, &s, true).await;
    let (t, o, s, _) = past_panel_guild(&us, "SELECT", &[], Vec::new());
    let mut links: Vec<Value> = t.settings_store.row(TG).unwrap().timezone_roles.unwrap().as_array().unwrap().clone();
    for i in 0..(25 - 7) {
        let id = format!("93000000000000{i:04}");
        o.role(&id, &format!("z{i}"), 1);
        links.push(json!({ "roleId": id, "zone": "UTC" }));
    }
    put_settings(&t, json!({ "timezoneRoles": links }));
    untouched(&t, &o, &s, true).await;
    let (t, o, s, _) = past_panel_guild(&us, "SELECT", &[], Vec::new());
    o.w().manage_roles = false;
    untouched(&t, &o, &s, true).await;
}

#[tokio::test]
async fn upgrade_a_refusal_leaves_the_panel_and_the_next_run_finishes() {
    let (t, ops, store, _) = past_panel_guild(&past("timezones-us"), "SELECT", &[], Vec::new());
    ops.w().create_fail = Some("AST (PR)".into());
    let before = tz_links(&t);
    assert_eq!(setup::upgrade_timezone_panels_with(&t, TG, &store).await, 0);
    assert_eq!(tz_links(&t), before);
    assert!(store.inserts.lock().unwrap().is_empty());
    ops.w().create_fail = None;
    assert_eq!(setup::upgrade_timezone_panels_with(&t, TG, &store).await, 1);
    assert_eq!(created_names(&ops), vec!["SST", "MST (AZ)", "AST (PR)", "ChST"], "SST and MST (AZ) were reused");
}

// ── Static checks (test 16) ───────────────────────────────────────────────────

#[test]
fn static_checks_on_the_sources() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(p).unwrap() {
            let e = e.unwrap().path();
            if e.is_dir() { walk(&e, out) } else { out.push(e) }
        }
    }
    walk(&dir, &mut files);
    let mut seen = HashSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let name = f.display().to_string();
        assert!(!text.contains("println!"), "{name}: println");
        assert!(!text.contains(":settings\""), "{name}: touches the settings cache key");
        assert!(!text.contains("\"GuildSettings\""), "{name}: writes GuildSettings");
        assert!(!text.contains("todo!(") && !text.contains("unimplemented!("), "{name}: stub");
        assert!(!text.contains("edit_member") && !text.contains(".roles(") , "{name}: array-form role call");
        seen.insert(name);
    }
    assert!(seen.len() >= 10);
}

// ── The production Discord operations, on a cached guild ──────────────────────

#[tokio::test]
async fn serenity_operations_read_the_cache_like_discord_js() {
    use bot_core::testing::{FakeGuild, TEST_BOT_ID};
    use feat_reactionroles::reaction_role_service::{RrDiscord, SerenityRr};
    use serenity::all::Permissions as P;
    let g = "955019000000000090";
    let guild = FakeGuild::new(g, "600000000000000099")
        .role("820000000000000001", 10, P::MANAGE_ROLES | P::VIEW_CHANNEL)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role("820000000000000002", 5, P::empty())
        .role("820000000000000003", 15, P::empty())
        .role_with("820000000000000004", 3, P::empty(), |r| r["managed"] = json!(true))
        .channel("810000000000000001", 0, "open", &[])
        .channel("810000000000000002", 0, "locked", &[(g, false, P::empty(), P::MANAGE_MESSAGES)]);
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[guild]);
    let ops = SerenityRr { app: t.app.clone() };
    assert_eq!(ops.bot_id().as_deref(), Some(TEST_BOT_ID));
    assert!(ops.guild_available(g));
    assert!(ops.bot_can_manage_roles(g));
    assert!(ops.cached_role(g, "820000000000000002").unwrap().editable, "below the bot");
    assert!(!ops.cached_role(g, "820000000000000003").unwrap().editable, "above the bot");
    assert!(!ops.cached_role(g, "820000000000000004").unwrap().editable, "managed");
    assert!(ops.cached_role(g, "820000000000000099").is_none());
    let roles = ops.guild_roles(g);
    assert_eq!(roles.len(), 5, "@everyone counts towards the role cap");
    assert!(roles.iter().any(|r| r.id == g));
    assert!(ops.check_assignable(g, "820000000000000002", None).ok);
    assert_eq!(ops.check_assignable(g, "820000000000000003", None).reason.as_deref(), Some("That role is above my highest role."));
    let ids = ["820000000000000002".to_string(), "820000000000000003".to_string(), "820000000000000099".to_string()];
    let usable = rr::check_roles_usable(&ops, g, &ids);
    assert_eq!((usable.missing, usable.blocked), (vec![ids[2].clone()], vec![ids[1].clone()]));
    assert!(!ops.bot_has_manage_messages(g, "810000000000000002"), "an overwrite denies it");
    assert!(!ops.bot_has_manage_messages(g, "810000000000000001"), "the bot's roles do not grant it");
    assert!(!ops.bot_has_manage_messages(g, "810000000000000099"), "an uncached channel");
    assert_eq!(ops.cached_channel(g, "810000000000000001").unwrap().name, "open");
    // offline every REST call fails without a code, so nothing is ever treated as deleted
    assert_eq!(ops.fetch_role_exists(g, "820000000000000099").await.unwrap_err().code, None);
    assert_eq!(rr::role_verdict(&ops, g, "820000000000000099").await, rr::RoleVerdict::Unknown);
    assert_eq!(ops.member_roles(g, TEST_BOT_ID).await.unwrap_err().code, None);
    // the bot without Manage Roles
    let g2 = "955019000000000091";
    let guild2 = FakeGuild::new(g2, "600000000000000099")
        .role("820000000000000011", 10, P::VIEW_CHANNEL)
        .member(TEST_BOT_ID, &["820000000000000011"])
        .role("820000000000000012", 2, P::empty());
    let t2 = TestApp::new().with_guilds([g2]).with_fake_guilds(&[guild2]);
    let ops2 = SerenityRr { app: t2.app.clone() };
    assert!(!ops2.bot_can_manage_roles(g2));
    assert!(!ops2.cached_role(g2, "820000000000000012").unwrap().editable);
    assert!(rr::check_roles_usable(&ops2, g2, &["820000000000000012".to_string()]).no_manage_roles);
}

#[test]
fn constant_tables_are_iterable_for_the_dashboard() {
    assert_eq!(rr::MAX_ITEMS.iter().map(|(_, n)| *n).max(), Some(25));
    assert_eq!(rr::STYLE_MAP.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec!["Primary", "Secondary", "Success", "Danger"]);
    assert_eq!(rr::TYPE_LABELS.iter().map(|(k, _)| *k).collect::<Vec<_>>(), rr::RR_TYPES.to_vec());
    assert_eq!(rr::LAYOUT_LABELS.iter().map(|(k, _)| *k).collect::<Vec<_>>(), rr::RR_LAYOUTS.to_vec());
    assert_eq!(rr::style_map("Danger"), Some(4));
    assert_eq!(rr::max_items("REACTIONS"), Some(20));
    assert_eq!(rr::placeholder_for("REVERSE"), Some("Choose a role to remove"));
}

// ── Review fixes ──────────────────────────────────────────────────────────────

/// `getPanelIndex`: the in-flight load clears itself when it settles (`promise.then(clear, clear)`), even when the
/// caller that started it was dropped half-way (a hook timeout). Before, the settled load stayed in flight and every
/// later call joined it instead of loading again.
#[tokio::test]
async fn an_abandoned_index_load_does_not_stay_in_flight() {
    use futures::FutureExt;
    let t = TestApp::new();
    let g = "955019000000000095";
    // Started and dropped while it waits on PostgreSQL (offline: the connection is refused, so the load fails closed).
    assert!(rr::get_panel_index(&t, g).now_or_never().is_none(), "the load is pending on the database");
    assert!(rr::get_panel_index(&t, g).await.is_none(), "joins the load, which fails closed");
    // The index is now in the cache: the next call loads again and finds it.
    let panel = PanelLite {
        id: "p1".into(),
        channel_id: "810000000000000001".into(),
        message_id: "830000000000000001".into(),
        title: "Roles".into(),
        kind: "NORMAL".into(),
        layout: "REACTIONS".into(),
        max_roles: None,
        attached: false,
        items: Vec::new(),
    };
    t.cache().set(&rr::index_key(g), &vec![panel], 300).await;
    let index = rr::get_panel_index(&t, g).await.expect("a fresh load reads the cache");
    assert!(index.contains_key("830000000000000001"));
}

/// `guild.roles.fetch(id, { force: true })` and `guild.channels.fetch(id)` cache what they fetched (so an existing role
/// that was not cached is usable at once), and a channel of another guild is GuildChannelUnowned: an error without a
/// numeric code, so the panel is 'failed', never flagged as missing.
#[tokio::test]
async fn rest_fetches_cache_their_result_and_refuse_foreign_channels() {
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use feat_reactionroles::reaction_role_service::{RrDiscord, SerenityRr};
    use serenity::all::Permissions as P;
    let g = "955019000000000096";
    let bot_role = "820000000000000031";
    let fresh = "820000000000000032";
    let guild = FakeGuild::new(g, "600000000000000099").role(bot_role, 10, P::MANAGE_ROLES | P::VIEW_CHANNEL).member(TEST_BOT_ID, &[bot_role]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(&[guild], &rest);
    let role = |id: &str, pos: u16| {
        json!({ "id": id, "name": format!("role{id}"), "color": 0, "hoist": false, "position": pos, "permissions": "0", "managed": false,
                "mentionable": false, "flags": 0,
                "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null } })
    };
    rest.on("GET", "/guilds/*/roles", 200, json!([role(g, 0), role(bot_role, 10), role(fresh, 2)]));
    let ops = SerenityRr { app: t.app.clone() };
    assert!(ops.cached_role(g, fresh).is_none(), "not cached yet");
    let v = rr::verify_roles(&ops, g, &[fresh.to_string()]).await;
    assert!(v.gone.is_empty() && v.unverified.is_empty(), "the fetch cached the role, so it is usable: {v:?}");
    assert!(ops.cached_role(g, fresh).unwrap().editable);

    let own = "810000000000000031";
    let foreign = "810000000000000032";
    let channel = |id: &str, guild: &str| json!({ "id": id, "guild_id": guild, "type": 0, "name": format!("c{id}"), "position": 0, "permission_overwrites": [], "nsfw": false });
    rest.on("GET", &format!("/channels/{own}"), 200, channel(own, g));
    rest.on("GET", &format!("/channels/{foreign}"), 200, channel(foreign, "955019000000000097"));
    assert_eq!(ops.fetch_channel(g, own).await.unwrap().name, format!("c{own}"));
    assert!(ops.cached_channel(g, own).is_some(), "a fetched channel is cached");
    assert_eq!(ops.fetch_channel(g, foreign).await.unwrap_err().code, None, "GuildChannelUnowned has no code");
    match rr::fetch_panel_message(&t, g, "p1", foreign, "830000000000000001").await {
        rr::Fetched::Failed { code, reason } => assert_eq!((code, reason.as_str()), (None, "Discord error unknown")),
        other => panic!("expected failed, got {other:?}"),
    }
}
