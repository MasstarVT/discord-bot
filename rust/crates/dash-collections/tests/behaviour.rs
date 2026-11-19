//! The collection, catalog and channel-access cases of test/dashboard*.test.js, test/sec-dashboard.test.js and
//! test/review-dashboard*.test.js, the drift guards against the feature crates, the in-memory doubles of the seams,
//! and a database test: the rows the JS toolchain seeded, read through bot-db, are exactly the rows the item builders
//! were checked against.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bot_config::modules::{MODULE_NAMES, MODULES};
use dash_collections::collections::{automations as auto, embeds, levels, reaction_roles, small, ticket_panels};
use dash_collections::testing::{FakeDiscord, MemorySettings, MemoryStore, MemoryTasks, RegistryBridge};
use dash_collections::*;
use indexmap::IndexSet;
use serde_json::{Map, Value, json};

const NOW: f64 = 1_780_315_200_000.0; // 2026-06-01T12:00:00.000Z
const G: &str = "955040000000000001";
const ALICE: &str = "800000000000000001";

fn golden(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).expect("valid JSON")
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("an object")
}

fn validate(key: &str, data: Value, mode: Mode, action: Option<&str>, current: Option<Value>) -> Result<ValidInput, StoreError> {
    validate_record_input(key, &data, mode, action, current.as_ref(), NOW)
}

fn field_err(r: Result<ValidInput, StoreError>) -> (String, String) {
    match r {
        Err(StoreError::Field(f)) => (f.path, f.message),
        other => panic!("expected a FieldError, got {other:?}"),
    }
}

// ── Catalog (test/dashboard.test.js) ─────────────────────────────────────────────────────────────────────────────

#[test]
fn every_module_appears_in_exactly_one_category_in_the_contract_order() {
    let ids: Vec<&str> = catalog::CATEGORIES.iter().map(|c| c.id).collect();
    assert_eq!(ids, ["safety", "community", "engagement", "tools"]);
    let mut listed: Vec<&str> = catalog::CATEGORIES.iter().flat_map(|c| c.modules.iter().copied()).collect();
    let unique: HashSet<&str> = listed.iter().copied().collect();
    assert_eq!(unique.len(), listed.len(), "a module is listed twice");
    listed.sort_unstable();
    let mut names: Vec<&str> = MODULE_NAMES.to_vec();
    names.sort_unstable();
    assert_eq!(listed, names);
    for c in catalog::CATEGORIES {
        assert!(!c.label.is_empty() && !c.description.is_empty(), "{}", c.id);
    }
}

#[test]
fn every_module_has_a_short_description_and_a_unique_two_letter_code() {
    let mut codes = HashSet::new();
    for key in MODULE_NAMES {
        let text = catalog::description(key).unwrap_or_else(|| panic!("{key} has no description"));
        let n = text.encode_utf16().count();
        assert!((20..=140).contains(&n), "{key}: description length {n}");
        assert!(text.ends_with('.'), "{key}: description should be a sentence");
        let code = catalog::code(key).unwrap_or_else(|| panic!("{key} has no code"));
        let b = code.as_bytes();
        assert!(b.len() == 2 && b[0].is_ascii_uppercase() && b[1].is_ascii_lowercase(), "{key}: code {code}");
        assert!(codes.insert(code), "two modules share {code}");
    }
    assert_eq!(catalog::DESCRIPTIONS.len(), MODULE_NAMES.len());
    assert_eq!(catalog::CODES.len(), MODULE_NAMES.len());
    let live = catalog::description("liverole").unwrap();
    assert!(live.contains("off here leaves it on current streamers") && live.contains("/liverole disable in Discord removes it too"));
}

#[test]
fn build_catalog_mirrors_the_module_registry() {
    let c = build_catalog();
    let modules = c["modules"].as_object().unwrap();
    assert_eq!(modules.len(), MODULE_NAMES.len());
    for info in &MODULES {
        let key: &str = info.key.as_ref();
        let m = &modules[key];
        assert_eq!(m["key"], key);
        assert_eq!(m["flag"], info.flag);
        assert_eq!(m["label"], info.label);
        assert_eq!(m["emoji"], info.emoji);
        assert_eq!(m["configCommand"], info.config_command);
        assert_eq!(m["default"], info.default);
        assert_eq!(m["description"], catalog::description(key).unwrap());
        assert_eq!(m["code"], catalog::code(key).unwrap());
        for t in m["subToggles"].as_array().unwrap() {
            let mut keys: Vec<&String> = t.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["default", "description", "field", "label"]);
        }
        let lists: Vec<&str> = Collections::builtin().iter().filter(|col| col.module() == key).map(|col| col.key.as_str()).collect();
        assert_eq!(m["collections"], json!(lists), "{key}");
    }
    assert_eq!(c["modules"]["moderation"]["subToggles"][0]["field"], "modDmTargets");
    let subs: usize = modules.values().map(|m| m["subToggles"].as_array().unwrap().len()).sum();
    assert_eq!(subs, 31);
    assert_eq!(c["modules"]["reactionroles"]["groups"], json!([]), "nothing beyond the flag");
}

#[test]
fn the_catalog_carries_the_general_page_every_collection_and_the_placeholders() {
    let c = build_catalog();
    let mut keys: Vec<&String> = c.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["categories", "collections", "general", "modules", "placeholders"]);
    assert_eq!(c["general"]["title"], "General");
    let general: Vec<&str> = c["general"]["groups"][0]["fields"].as_array().unwrap().iter().map(|f| f["field"].as_str().unwrap()).collect();
    assert_eq!(general, ["timezone", "embedColor", "customPrefix"]);
    let cols: Vec<&String> = c["collections"].as_object().unwrap().keys().collect();
    let want: Vec<&str> = Collections::builtin().keys().collect();
    assert_eq!(cols, want);
    let group = |t: &str| c["placeholders"].as_array().unwrap().iter().find(|p| p["token"] == t).map(|p| p["group"].clone()).unwrap();
    assert_eq!(group("{server.member_count}"), "server");
    assert_eq!(group("{channel.name}"), "channel");
    assert_eq!(group("{random}"), "other");
    assert_eq!(catalog::placeholder_group("{username}"), "other");
    assert_eq!(catalog::placeholder_group("{user}"), "user");
    let text = catalog_json();
    for hook in ["\"read\"", "\"write\"", "\"normalize\"", "\"check\""] {
        assert!(!text.contains(hook), "{hook} leaked");
    }
}

#[test]
fn a_fake_collection_set_builds_its_own_catalog_and_validates_its_own_requests() {
    fn no_x(data: &Map<String, Value>, _: &CheckCtx) -> Option<Problem> {
        (data.get("name") == Some(&json!("x"))).then(|| Problem::new("name", "Not x."))
    }
    let c = Collection::from_json(json!({
        "key": "things", "module": "embeds", "label": "Things", "singular": "thing", "limit": 3, "titleField": "name",
        "fields": [{ "field": "name", "type": "text", "label": "Name", "maxLength": 10, "required": true }],
        "createOnly": [], "readOnly": [], "actions": [],
    }))
    .unwrap()
    .with_check(no_x);
    let set = Collections::new(vec![c]);
    assert!(set.lint().is_empty());
    let cat = build_catalog_with(&set);
    assert_eq!(cat["modules"]["embeds"]["collections"], json!(["things"]));
    assert_eq!(cat["collections"]["things"]["canCreate"], true);
    let ok = set.validate_record_input("things", &json!({ "name": "a" }), Mode::Create, None, None, NOW).unwrap();
    assert_eq!(Value::Object(ok.data), json!({ "name": "a" }));
    let err = set.validate_record_input("things", &json!({ "name": "x" }), Mode::Create, None, None, NOW).unwrap_err();
    assert_eq!(err.as_field().unwrap().message, "Not x.");
    let err = set.validate_record_input("embeds", &json!({}), Mode::Create, None, None, NOW).unwrap_err();
    assert_eq!(err.as_record().unwrap().code, RecordErrorCode::NotFound);
}

// ── Descriptors (test/dashboard-collections*.test.js, dashboard-panels, dashboard-moderation) ──────────────────

#[test]
fn every_collection_is_registered_real_and_tied_to_a_module() {
    assert_eq!(Collections::builtin().len(), 22);
    for c in Collections::builtin().iter() {
        assert!(!c.placeholder(), "{} is still a placeholder", c.key);
        assert!(MODULE_NAMES.contains(&c.module()), "{}: module {}", c.key, c.module());
        assert!(!c.label().is_empty() && !c.singular().is_empty(), "{}", c.key);
        let names: HashSet<&str> = c.all_fields().map(|f| f.field()).collect();
        if let Some(t) = c.title_field() {
            assert!(names.contains(t) || t == "id", "{}: titleField {t}", c.key);
        }
        // The panels' row subtitle is the item's summary (dashboard-panels.test.js); the others name a descriptor.
        if let Some(s) = c.get("subtitleField").and_then(Value::as_str) {
            assert!(names.contains(s) || (s == "summary" && ["reactionRoles", "ticketPanels"].contains(&c.key.as_str())), "{}: subtitleField {s}", c.key);
        }
        assert!(c.public_json().to_string().len() < 60_000, "{}: public descriptor is small", c.key);
        assert!(is_collection_key(&c.key));
    }
}

#[test]
fn limits_come_from_limits() {
    use bot_config::constants::limits as l;
    for (key, limit) in [
        ("levelRoles", l::LEVEL_ROLES),
        ("shopItems", l::SHOP_ITEMS),
        ("achievements", l::ACHIEVEMENTS),
        ("starboards", l::STARBOARDS),
        ("translateChannels", l::TRANSLATE_CHANNELS),
        ("tempVoiceHubs", l::TEMPVOICE_HUBS),
        ("statChannels", l::STAT_CHANNELS),
        ("economyBoosts", l::ECONOMY_BOOSTS),
        ("embeds", l::SAVED_EMBEDS),
        ("tags", l::CUSTOM_TAGS),
        ("automations", l::AUTOMATIONS),
        ("ticketPanels", l::TICKET_PANELS),
    ] {
        assert_eq!(Collections::builtin().get(key).unwrap().limit(), json!(limit), "{key}");
    }
    for key in ["cases", "levels", "balances", "giveaways", "polls"] {
        assert_eq!(Collections::builtin().get(key).unwrap().limit(), Value::Null, "{key}: no cap");
    }
}

#[test]
fn the_channel_and_role_kinds_are_the_ones_the_commands_offer() {
    let field = |key: &str, name: &str| Collections::builtin().get(key).unwrap().fields.iter().find(|f| f.field() == name).cloned().unwrap();
    assert_eq!(field("starboards", "channelId").channel_types().unwrap(), ["text", "announcement"]);
    assert_eq!(field("tempVoiceHubs", "channelId").channel_types().unwrap(), ["voice"]);
    assert_eq!(field("tempVoiceHubs", "categoryId").channel_types().unwrap(), ["category"]);
    assert_eq!(field("statChannels", "channelId").channel_types().unwrap(), ["voice"]);
    assert!(field("levelRoles", "roleId").assignable());
    assert!(field("shopItems", "roleId").assignable());
    let tiers = field("achievements", "tiers");
    assert!(tiers.child("roleAdd").unwrap().assignable() && tiers.child("roleRemove").unwrap().assignable());
    assert!(!field("economyBoosts", "roleId").assignable());
    assert!(!field("statChannels", "roleId").assignable());
    assert!(!field("tempVoiceHubs", "moderatorRoleIds").assignable());
}

#[test]
fn cases_levels_and_balances_are_searchable_member_lists_without_a_cap() {
    for key in ["cases", "levels", "balances"] {
        let c = Collections::builtin().get(key).unwrap();
        assert!(c.searchable(), "{key}");
        assert_eq!(c.public_json()["avatarField"], "avatarUrl", "{key}");
        assert_eq!(c.public_json()["canDelete"], false, "{key}");
    }
    let rr = Collections::builtin().get("reactionRoles").unwrap();
    assert_eq!(rr.create_only, ["template", "channelId", "layout"]);
    assert!(Collections::builtin().get("tags").unwrap().check_stored_roles());
    assert!(Collections::builtin().get("automations").unwrap().check_stored_roles());
    assert!(!Collections::builtin().get("embeds").unwrap().check_stored_roles());
}

// ── Requests ────────────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn level_rewards_refuse_a_level_nobody_can_reach() {
    let (path, message) = field_err(validate("levelRoles", json!({ "level": 999, "roleId": "900000000000000301" }), Mode::Create, None, None));
    assert_eq!(path, "level");
    assert!(message.starts_with("Level 999 needs more than 1,000,000,000 XP, so nobody can reach it."), "{message}");
    assert!(message.ends_with(&format!("The highest reachable level is {}.", small::highest_reachable_level())));
    let (path, _) = field_err(validate("levelRoles", json!({ "level": 5 }), Mode::Create, None, None));
    assert_eq!(path, "roleId");
    let (path, message) = field_err(validate("levelRoles", json!({ "zzz": 1 }), Mode::Update, None, Some(json!({ "level": 5 }))));
    assert_eq!((path.as_str(), message.as_str()), ("zzz", "\"zzz\" is not something you can set here."));
}

#[test]
fn shop_items_stock_zero_only_after_creation() {
    let (path, _) = field_err(validate("shopItems", json!({ "name": "Cookie", "price": 1, "stock": 0 }), Mode::Create, None, None));
    assert_eq!(path, "stock");
    let ok = validate("shopItems", json!({ "stock": 0 }), Mode::Update, None, Some(json!({ "type": "ITEM" }))).unwrap();
    assert_eq!(ok.data["stock"], 0);
    let ok = validate("shopItems", json!({ "name": "Cookie", "price": 100 }), Mode::Create, None, None).unwrap();
    assert_eq!(ok.data["stock"], Value::Null, "empty is unlimited");
}

#[test]
fn achievement_deadlines_follow_the_clock_and_an_unchanged_one_stays_valid() {
    let base = json!({ "name": "a", "trigger": "MESSAGES", "tiers": [{ "goal": 5 }] });
    let with = |d: &str| {
        let mut b = obj(base.clone());
        b.insert("deadline".into(), json!(d));
        Value::Object(b)
    };
    assert!(validate("achievements", with("2026-06-02T00:00:00.000Z"), Mode::Create, None, None).is_ok());
    let (path, message) = field_err(validate("achievements", with("2026-06-01T12:00:30.000Z"), Mode::Create, None, None));
    assert_eq!((path.as_str(), message.as_str()), ("deadline", "The deadline must be in the future."));
    let past = "2026-01-01T00:00:00.000Z";
    let current = json!({ "trigger": "MESSAGES", "deadline": past });
    assert!(validate("achievements", json!({ "deadline": past }), Mode::Update, None, Some(current)).is_ok());
}

#[test]
fn giveaway_updates_carry_only_the_odds_and_actions_validate_their_own_fields() {
    let (path, _) = field_err(validate("giveaways", json!({ "prize": "x" }), Mode::Update, None, Some(json!({}))));
    assert_eq!(path, "prize");
    let ok = validate("giveaways", json!({ "winners": 2 }), Mode::Action, Some("reroll"), Some(json!({}))).unwrap();
    assert_eq!(Value::Object(ok.data), json!({ "winners": 2 }));
    let (path, message) = field_err(validate("giveaways", json!({ "endsAt": "2025-01-01T00:00:00.000Z" }), Mode::Action, Some("restart"), Some(json!({}))));
    assert_eq!((path.as_str(), message.as_str()), ("endsAt", "That end time is in the past."));
    let err = validate("giveaways", json!({}), Mode::Action, Some("nope"), Some(json!({}))).unwrap_err();
    assert_eq!(err.as_record().unwrap().message, "Unknown action.");
}

#[test]
fn automation_rules_need_their_parameters_and_on_needs_an_action() {
    let (path, message) = field_err(validate("automations", json!({ "name": "a", "trigger": "MESSAGE_SENT", "enabled": true }), Mode::Create, None, None));
    assert_eq!((path.as_str(), message.as_str()), ("enabled", auto::NEEDS_ACTIONS));
    let (path, message) = field_err(validate(
        "automations",
        json!({ "name": "a", "trigger": "MEMBER_JOINED", "actions": [{ "type": "CREATE_THREAD" }] }),
        Mode::Create,
        None,
        None,
    ));
    assert_eq!(path, "actions.0.threadChannelId");
    assert_eq!(message, "Action 1: there is no message to start a thread from, so pick a channel.");
    let (path, message) = field_err(validate("automations", json!({ "name": "a", "preset": "welcomer" }), Mode::Create, None, None));
    assert_eq!((path.as_str(), message.as_str()), ("presetChannelId", "The Welcomer preset needs a channel to post in."));
}

#[test]
fn plan_buttons_numbers_new_buttons_like_automation_action_add() {
    let btn = |index: Value| json!({ "type": "SEND_MESSAGE", "content": "x", "button": { "kind": "automation", "label": "b", "index": index } });
    let plan = auto::plan_buttons(&[btn(Value::Null), btn(json!(3)), btn(Value::Null)], &[json!(3), json!(5)], Some(&json!(6)), true).unwrap();
    assert_eq!(plan.indexes, vec![Some(json!(7)), Some(json!(3)), Some(json!(8))]);
    assert_eq!(plan.button_seq, 8.0);
    let err = auto::plan_buttons(&[btn(Value::Null)], &[], Some(&json!(99)), true).unwrap_err();
    assert_eq!((err.path.as_str(), err.message.as_str()), ("actions.0.button", auto::BUTTONS_USED));
}

#[test]
fn a_role_kept_in_a_take_action_is_checked_again_once_it_becomes_a_give_action() {
    let c = Collections::builtin().get("automations").unwrap();
    let role = "900000000000000301";
    let stored = json!({ "actions": [{ "type": "TAKE_ROLES", "roleIds": [role] }] });
    let (refs, keys) = c.record_refs(Some(&stored));
    assert!(refs.roles.contains(role));
    let r = dash_schema::RoleRef { id: role.into(), path: "actions.0.roleIds.0".into(), assignable: true, allow_everyone: false, allow_dangerous: false };
    let same = obj(json!({ "actions": [{ "type": "TAKE_ROLES", "roleIds": [role] }] }));
    assert!(keys.contains(&c.ref_key_for(&r, &same)), "unchanged: kept");
    let give = obj(json!({ "actions": [{ "type": "GIVE_ROLES", "roleIds": [role] }] }));
    assert!(!keys.contains(&c.ref_key_for(&r, &give)), "a Take role turned into a Give role is new");
    // Other collections key by path only.
    let tags = Collections::builtin().get("tags").unwrap();
    assert_eq!(tags.ref_key_for(&r, &give), dash_schema::ref_key("actions.0.roleIds.0", role));
    let keys: IndexSet<String> = ["actions.roleIds|1".to_string()].into_iter().collect();
    assert!(auto::action_role_keys(None, keys).is_empty());
}

#[test]
fn stored_roles_of_a_record_are_found_with_their_paths() {
    let tags = Collections::builtin().get("tags").unwrap();
    let item = json!({ "actions": [{ "type": "addRole", "roleId": "900000000000000301" }, { "type": "reply", "roleId": null }] });
    let held = dash_schema::collect_assignable_roles(&tags.fields, Some(&item));
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].path, "actions.0.roleId");
}

// ── Channel access (test/sec-dashboard.test.js) ────────────────────────────────────────────────────────────────

fn ctx_with(member: DashboardMember, tasks: Option<Arc<MemoryTasks>>) -> StoreCtx {
    StoreCtx {
        guild_id: G.into(),
        user_id: "955040000000000010".into(),
        member: Some(member),
        tasks: tasks.map(|t| t as Arc<dyn TaskRunner>),
        ..StoreCtx::default()
    }
}

fn manager() -> DashboardMember {
    DashboardMember { owner: false, permissions: (1u64 << 5).to_string(), highest_role_position: None }
}

#[tokio::test]
async fn owners_and_administrators_never_ask_the_bot() {
    let tasks = Arc::new(MemoryTasks::new());
    let checks = [ChannelCheck::new("955040000000000020").path("channelId")];
    for m in [DashboardMember { owner: true, ..manager() }, DashboardMember { permissions: "8".into(), ..manager() }] {
        require_channel_access(&ctx_with(m, Some(tasks.clone())), &checks, "").await.unwrap();
    }
    assert!(tasks.calls().is_empty());
    // A write without a channel checks nothing either.
    require_channel_access(&ctx_with(manager(), Some(tasks.clone())), &[ChannelCheck::default()], "").await.unwrap();
    assert!(tasks.calls().is_empty());
}

#[tokio::test]
async fn a_hidden_channel_is_refused_at_its_field_and_the_check_payload_is_the_tasks() {
    let tasks = Arc::new(MemoryTasks::new());
    tasks.answer(
        "access.channels",
        TaskResult::done("x", Some(json!({ "problems": [{ "index": 1, "text": "You need View Channel, Send Messages in #staff-only." }] }))),
    );
    let checks = [
        ChannelCheck::new("955040000000000021").path("a"),
        ChannelCheck::new("955040000000000020").needs(&VIEW_NEEDS).optional().path("actions.0.channelId"),
    ];
    let err = require_channel_access(&ctx_with(manager(), Some(tasks.clone())), &checks, "The test channel: ").await.unwrap_err();
    let e = err.as_record().unwrap();
    assert_eq!(e.code, RecordErrorCode::Invalid);
    assert_eq!(e.path.as_deref(), Some("actions.0.channelId"));
    assert_eq!(e.message, "The test channel: You need View Channel, Send Messages in #staff-only.");
    let calls = tasks.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "access.channels");
    assert_eq!(calls[0].3.actor_id, "955040000000000010");
    assert_eq!(
        calls[0].2,
        json!({ "checks": [{ "channelId": "955040000000000021" }, { "channelId": "955040000000000020", "needs": ["ViewChannel"], "optional": true }] })
    );
}

/// `wanted[first.index]` reads a canonical numeric-string index as JS property keys do, and a falsy first problem is
/// no problem (`if (!first) return;`).
#[tokio::test]
async fn problem_index_and_falsy_first_follow_js_property_semantics() {
    let checks = [ChannelCheck::new("955040000000000021").path("a"), ChannelCheck::new("955040000000000020").path("b")];
    let tasks = Arc::new(MemoryTasks::new());
    for (index, path) in [(json!("1"), Some("b")), (json!("01"), None), (json!("1.0"), None), (json!(1.0), Some("b")), (json!(true), None)] {
        tasks.answer("access.channels", TaskResult::done("x", Some(json!({ "problems": [{ "index": index, "text": "No." }] }))));
        let err = require_channel_access(&ctx_with(manager(), Some(tasks.clone())), &checks, "").await.unwrap_err();
        assert_eq!(err.as_record().unwrap().path.as_deref(), path, "index {index}");
    }
    for first in [json!(null), json!(0), json!(""), json!(false)] {
        tasks.answer("access.channels", TaskResult::done("x", Some(json!({ "problems": [first] }))));
        require_channel_access(&ctx_with(manager(), Some(tasks.clone())), &checks, "").await.unwrap();
    }
}

#[tokio::test]
async fn without_the_bot_or_an_answer_the_write_is_refused_rather_than_allowed() {
    let checks = [ChannelCheck::new("955040000000000021")];
    let err = require_channel_access(&ctx_with(manager(), None), &checks, "").await.unwrap_err();
    assert_eq!(err.as_record().unwrap().code, RecordErrorCode::Unavailable);
    let tasks = Arc::new(MemoryTasks::new());
    tasks.answer("access.channels", TaskResult { status: TaskStatus::Pending, message: "wait".into(), data: None });
    let err = require_channel_access(&ctx_with(manager(), Some(tasks.clone())), &checks, "").await.unwrap_err();
    assert!(err.as_record().unwrap().message.starts_with("The bot could not check your access to the channel in time."));
    tasks.answer("access.channels", TaskResult { status: TaskStatus::Failed, message: "You are not a member of this server.".into(), data: None });
    let err = require_channel_access(&ctx_with(manager(), Some(tasks.clone())), &checks, "").await.unwrap_err();
    assert_eq!(err.as_record().unwrap().code, RecordErrorCode::Failed);
    tasks.fail("access.channels", "database down");
    let err = require_channel_access(&ctx_with(manager(), Some(tasks)), &checks, "").await.unwrap_err();
    assert!(matches!(err, StoreError::Internal(_)));
}

// ── Member helpers (test/dashboard-moderation.test.js) ─────────────────────────────────────────────────────────

#[tokio::test]
async fn member_search_ids_pass_through_names_go_to_discord_failures_say_so() {
    assert_eq!(levels::search_user_ids(None, "1", ALICE).await.unwrap(), [ALICE]);
    assert_eq!(levels::search_user_ids(None, "1", &format!("<@!{ALICE}>")).await.unwrap(), [ALICE]);
    assert!(levels::search_user_ids(None, "1", "alice").await.unwrap().is_empty(), "no Discord client: ids only");
    let fake = Arc::new(FakeDiscord::new());
    fake.set_user(MemberProfile { id: ALICE.into(), username: "alice".into(), ..MemberProfile::default() });
    let discord: Arc<dyn DashboardDiscord> = fake.clone();
    assert_eq!(levels::search_user_ids(Some(&discord), "1", "al").await.unwrap(), [ALICE]);
    fake.fail_next("searchMembers", DiscordApiError::new("503", 503, "unavailable"));
    let err = levels::search_user_ids(Some(&discord), "1", "al").await.unwrap_err();
    assert_eq!(err.code, RecordErrorCode::Discord);
}

#[tokio::test]
async fn profiles_never_fail_a_list() {
    let fake = Arc::new(FakeDiscord::new());
    let discord: Arc<dyn DashboardDiscord> = fake.clone();
    fake.fail_next("getUsers", DiscordApiError::new("down", 0, "unavailable"));
    assert!(levels::load_profiles(Some(&discord), &[ALICE.to_string()]).await.is_empty());
    assert!(levels::load_profiles(None, &[ALICE.to_string()]).await.is_empty());
    let before = fake.calls().len();
    assert!(levels::load_profiles(Some(&discord), &["bad".to_string()]).await.is_empty());
    assert_eq!(fake.calls().len(), before, "no call without ids");
    fake.set_user(MemberProfile { id: ALICE.into(), username: "alice".into(), global_name: Some("Alice".into()), ..MemberProfile::default() });
    let map = levels::load_profiles(Some(&discord), &[ALICE.to_string(), ALICE.to_string()]).await;
    assert_eq!(levels::profile_name(map[ALICE].as_ref(), Some(ALICE)), "Alice");
}

// ── Drift guards against the feature crates (test/dashboard-collections-more.test.js, dashboard-panels) ─────────

#[test]
fn copied_limits_and_tables_match_the_services() {
    use feat_economy::economy_service as eco;
    assert_eq!(small::ITEM_NAME_MAX, eco::ITEM_NAME_MAX);
    assert_eq!(small::ITEM_DESCRIPTION_MAX, eco::ITEM_DESCRIPTION_MAX);
    assert_eq!(small::ITEM_MESSAGE_MAX, eco::ITEM_MESSAGE_MAX);
    assert_eq!(small::ITEM_IMAGE_MAX, eco::ITEM_IMAGE_MAX);
    assert_eq!(small::MAX_STOCK, eco::MAX_STOCK);
    assert_eq!(small::BOOST_NAME_MAX, eco::BOOST_NAME_MAX);
    assert_eq!(small::BOOST_PERCENT_MAX, eco::BOOST_PERCENT_MAX);
    use feat_starboard_stats::{starboard_service as sb, stats_service as st};
    assert_eq!(small::MAX_EMOJIS_PER_BOARD, sb::MAX_EMOJIS_PER_BOARD);
    assert_eq!(small::MAX_IGNORED_CHANNELS, sb::MAX_IGNORED_CHANNELS);
    for (kind, label, template) in small::KIND_INFO {
        let k: bot_db::models::StatKind = serde_json::from_value(json!(kind)).unwrap();
        assert_eq!(st::kind_info(k), (label, template), "{kind}");
    }
    use feat_tempvoice::temp_voice_service as tv;
    assert_eq!(small::DEFAULT_NAME_FORMAT, tv::DEFAULT_NAME_FORMAT);
    assert_eq!(small::MAX_HUB_ROLE_LIST, tv::MAX_HUB_ROLE_LIST);
    use feat_tickets::ticket_service::rules as tr;
    assert_eq!(ticket_panels::NAME_FORMAT_TOKENS, tr::NAME_FORMAT_TOKENS);
    for s in ["Ticket {number}", "  Héllo  Wörld!! ", "__-x--y-__", "ＡＢＣ　ｄｅｆ", "💥", "ﬁ İstanbul ΣΑΣ", "{ User.Name }-{topic}", "{nope}", ""] {
        assert_eq!(ticket_panels::sanitize_channel_name(s), tr::sanitize_channel_name(s), "{s}");
        assert_eq!(ticket_panels::validate_name_format(Some(&json!(s))), tr::validate_name_format(s), "{s}");
    }
    for e in ["<:party:900000000000000401>", "❤️", "\u{FE0F}", "", "x", "<a:spin:900000000000000402>"] {
        assert_eq!(reaction_roles::emoji_key(Some(&json!(e))), feat_reactionroles::reaction_role_service::emoji_key_of_stored(Some(e)), "{e:?}");
    }
}

#[test]
fn more_copied_limits_match_the_services() {
    use dash_collections::collections::{cases, giveaways_polls as gp};
    use feat_achievements::achievement_service as ach;
    assert_eq!(small::NAME_MAX, ach::NAME_MAX);
    assert_eq!(small::DESCRIPTION_MAX, ach::DESCRIPTION_MAX);
    assert_eq!(small::GOAL_INPUT_MAX, ach::GOAL_INPUT_MAX);
    assert_eq!(small::MAX_REWARD_AMOUNT, ach::MAX_REWARD_AMOUNT);
    assert_eq!(small::DEADLINE_MIN_MS, ach::DEADLINE_MIN_MS as f64);
    assert_eq!(small::DEADLINE_MAX_MS, ach::DEADLINE_MAX_MS as f64);
    assert_eq!(small::TIER_NAMES, ach::TIER_NAMES);
    for (trigger, max) in small::GOAL_MAX {
        assert_eq!(ach::goal_max(trigger), Some(max), "{trigger}");
        assert_eq!(ach::trigger_label(trigger), small::trigger_label(trigger), "{trigger}");
    }
    use feat_giveaways_polls::{giveaway_service as gs, poll_service as ps};
    assert_eq!(gp::MIN_DURATION_MS, gs::MIN_DURATION_MS as f64);
    assert_eq!(gp::MAX_DURATION_MS, gs::MAX_DURATION_MS as f64);
    assert_eq!(gp::MAX_ROLE_WEIGHTS, gs::MAX_ROLE_WEIGHTS);
    assert_eq!(gp::MIN_BONUS_WEIGHT, gs::MIN_BONUS_WEIGHT);
    assert_eq!(gp::MAX_WEIGHT, gs::MAX_WEIGHT);
    assert_eq!(gp::DEFAULT_BONUS_WEIGHT, gs::DEFAULT_BONUS_WEIGHT);
    assert_eq!(gp::MAX_PRIZE_AMOUNT, gs::MAX_PRIZE_AMOUNT);
    assert_eq!(gp::QUESTION_MAX, ps::poll::QUESTION_MAX);
    assert_eq!(gp::OPTION_MAX, ps::poll::OPTION_MAX);
    assert_eq!(gp::OPTIONS_MIN, ps::poll::OPTIONS_MIN);
    assert_eq!(gp::DURATION_MIN_MS, ps::poll::DURATION_MIN_MS);
    assert_eq!(gp::DURATION_MAX_MS, ps::poll::DURATION_MAX_MS);
    use feat_moderation::moderation_service as ms;
    assert_eq!(cases::REASON_MAX, ms::REASON_MAX);
    assert_eq!(cases::TIMED_ACTION_MIN_MS, ms::TIMED_ACTION_MIN_MS);
    assert_eq!(cases::TIMED_ACTION_MAX_MS, ms::TIMED_ACTION_MAX_MS);
    assert_eq!(cases::MAX_CASE_ID, feat_moderation::commands::case::MAX_CASE_ID);
    use feat_reactionroles::reaction_role_service as rrs;
    assert_eq!(reaction_roles::MAX_ITEMS, rrs::MAX_ITEMS);
    assert_eq!(reaction_roles::TYPE_LABELS, rrs::TYPE_LABELS);
    assert_eq!(reaction_roles::LAYOUT_LABELS, rrs::LAYOUT_LABELS);
    assert_eq!(reaction_roles::DEFAULT_TITLE, rrs::DEFAULT_TITLE);
    let langs: Vec<(String, String)> = feat_translation::translation_service::LANGUAGES.iter().map(|(c, n)| ((*c).to_string(), (*n).to_string())).collect();
    assert_eq!(small::translate_languages(), langs.as_slice());
    let options = reaction_roles::template_options();
    assert_eq!(options[0]["value"], "none");
    assert!(options.len() > 1 && options.iter().skip(1).all(|o| o["description"].as_str().is_some_and(|d| d.starts_with("Creates (or reuses) "))));
}

#[test]
fn the_translate_language_table_is_read_from_the_descriptors() {
    let table = small::translate_languages();
    assert!(table.len() > 50);
    assert_eq!(small::language_name("es"), Some("Spanish"));
    assert_eq!(small::describe_languages(&[json!("es"), json!("zz")]), "Spanish, zz");
}

// ── The in-memory doubles ──────────────────────────────────────────────────────────────────────────────────────

fn actor() -> Actor {
    Actor { user_id: ALICE.into(), user: None, member: Some(DashboardMember { owner: true, ..DashboardMember::default() }) }
}

#[tokio::test]
async fn the_registry_bridge_dispatches_like_records_bridge_js() {
    let handler: dash_collections::testing::ActionHandler =
        Arc::new(|_, id, _, ctx, current| Ok(ActionAnswer::new(json!({ "id": id, "by": ctx.user_id, "had": current.is_some() }), "Posted.")));
    let store = Arc::new(MemoryStore::new("e").with_action("publish", handler));
    let mut registry = StoreRegistry::new();
    assert_eq!(registry.missing().len(), 22);
    registry.register("embeds", store.clone()).unwrap();
    assert!(registry.register("nope", store.clone()).is_err());
    assert_eq!(registry.missing().len(), 21);
    let bridge = RegistryBridge::new(registry);

    let saved = bridge.create(G, "embeds", obj(json!({ "name": "rules" })), &actor()).await.unwrap();
    assert_eq!(saved.item, json!({ "id": "e1", "name": "rules" }));
    assert_eq!(bridge.list(G, "embeds", ListOpts::default()).await.unwrap().items.len(), 1);
    assert!(bridge.get(G, "embeds", "e1").await.unwrap().is_some());
    let updated = bridge.update(G, "embeds", "e1", obj(json!({ "name": "faq" })), &actor()).await.unwrap();
    assert_eq!(updated.item["name"], "faq");
    let answer = bridge.action(G, "embeds", "e1", "publish", Map::new(), &actor()).await.unwrap();
    assert_eq!(answer.message.as_deref(), Some("Posted."));
    assert_eq!(answer.item.unwrap()["had"], true);
    let err = bridge.action(G, "embeds", "e1", "unpublish", Map::new(), &actor()).await.unwrap_err();
    assert_eq!(err.as_record().unwrap().message, "Unknown action.");
    let err = bridge.list(G, "tags", ListOpts::default()).await.unwrap_err();
    assert_eq!(err.as_record().unwrap().message, "Unknown list.");
    bridge.remove(G, "embeds", "e1", &actor()).await.unwrap();
    assert!(bridge.get(G, "embeds", "e1").await.unwrap().is_none());
    store.fail_next(RecordError::invalid("nope").with_path("name").into());
    let err = bridge.create(G, "embeds", Map::new(), &actor()).await.unwrap_err();
    assert_eq!(err.to_json(), json!({ "kind": "record", "code": "invalid", "message": "nope", "path": "name" }));
    let methods: Vec<String> = store.calls().iter().map(|c| c.method.clone()).collect();
    assert_eq!(methods, ["create", "list", "get", "update", "action", "remove", "get", "create"]);
}

#[tokio::test]
async fn memory_settings_store_rows_and_refuse_broken_cross_field_rules() {
    let settings = MemorySettings::new();
    let row = settings.read_guild_settings(G).await.unwrap();
    assert_eq!(row["guildId"], G);
    let valid = dash_schema::validate_settings_changes(&json!({ "changes": { "customPrefix": "?" } })).unwrap();
    let written = settings
        .write_settings(G, valid.changes.clone(), SettingsWriteCtx { user_id: ALICE.into(), discord: None })
        .await
        .unwrap();
    assert_eq!(written.row["customPrefix"], "?");
    assert_eq!(settings.row(G).unwrap()["customPrefix"], "?");
    settings.fail_reads(true);
    assert!(settings.read_guild_settings(G).await.is_err());
    settings.fail_writes(true);
    let err = settings.write_settings(G, valid.changes, SettingsWriteCtx::default()).await.unwrap_err();
    assert!(matches!(err, SettingsWriteError::Internal(_)));
}

#[tokio::test]
async fn the_fake_discord_answers_resources_and_records_writes() {
    let fake = FakeDiscord::new();
    let res: DiscordResources = serde_json::from_value(json!({
        "guild": { "id": G, "name": "Guild", "ownerId": ALICE },
        "channels": [{ "id": "955040000000000021", "name": "general", "type": 0, "kind": "text" }],
        "roles": [{ "id": "955040000000000030", "name": "Staff", "position": 2, "mentionable": false, "assignable": true, "dangerous": false }],
    }))
    .unwrap();
    fake.set_resources(G, res);
    let r = fake.get_guild_resources(G, false).await.unwrap();
    assert_eq!(r.channel("955040000000000021").unwrap().name, "general");
    let ctx = StoreCtx { user_id: ALICE.into(), ..StoreCtx::default() };
    assert!(embeds::member_for(&ctx, Some(&r)).unwrap().owner, "the owner named by the resources");
    assert_eq!(embeds::ping_role_problem(Some(&r), "955040000000000030", None).unwrap(),
        "The dashboard could not check whether you may ping @Staff. Make the role mentionable in Server Settings → Roles, or set it with the slash command in Discord.");
    let sent = fake.send_message("955040000000000021", json!({ "content": "hi" })).await.unwrap();
    assert_eq!(sent.channel_id, "955040000000000021");
    assert!(fake.get_guild_resources("1", false).await.is_err());
    // The resources serialise to the shape of getGuildResources.
    let v = serde_json::to_value(&*r).unwrap();
    assert_eq!(v["channels"][0]["type"], 0);
    assert_eq!(v["guild"]["ownerId"], ALICE);
    assert_eq!(v["bot"]["manageRoles"], false);
}

#[test]
fn errors_and_task_outcomes() {
    assert_eq!(RecordError::from_code("weird", "x").code, RecordErrorCode::Invalid);
    for (code, status) in RECORD_ERROR_STATUS {
        assert_eq!(RecordError::new(code, "x").status(), status);
    }
    assert_eq!(RecordErrorCode::Failed.status(), 422);
    assert_eq!(RecordError::invalid("x").with_path("").path, None);
    let saved = Saved::with_message(json!({ "id": 1 }), Some(""));
    assert_eq!(saved.message, None);
    let saved = Saved::with_message(json!({ "id": 1 }), Some("Posted."));
    assert_eq!(saved.message.as_deref(), Some("Posted."));
    assert!(is_record_id("cmwmlx2tn00191hmzh2c5yclo") && !is_record_id("a/b") && !is_record_id(&"a".repeat(41)));
}

// ── Database: the seeded rows through bot-db ───────────────────────────────────────────────────────────────────

macro_rules! fetch_json {
    ($pool:expr, $model:ty, $table:literal, $id:expr) => {{
        let sql = concat!("SELECT * FROM \"", $table, "\" WHERE \"id\" = $1");
        let id: &Value = $id;
        let row: $model = match id {
            Value::Number(n) => sqlx::query_as(sql).bind(n.as_i64().unwrap() as i32).fetch_one($pool).await,
            other => sqlx::query_as(sql).bind(other.as_str().unwrap().to_string()).fetch_one($pool).await,
        }
        .unwrap_or_else(|e| panic!("{} {id}: {e}", $table));
        serde_json::to_value(&row).unwrap()
    }};
}

#[tokio::test]
async fn seeded_rows_read_through_bot_db_are_the_rows_the_item_builders_were_checked_against() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    let pool = bot_db::client::connect(&url).await.expect("connect");
    let baseline = golden("mockdb/baseline.json");
    let helpers = golden("dash-collections/helpers.json");
    let mut compared = 0;
    for table in [
        "SavedEmbed", "CustomTag", "AutoTrigger", "Reminder", "Feed", "Infraction", "UserLevel", "EconomyAccount", "Giveaway", "Poll",
        "ReactionRoleGroup", "ReactionRoleItem", "TicketPanel", "TicketPanelOption", "Automation",
    ] {
        for which in ["min", "full"] {
            let want = &baseline[table][which];
            let id = &want["id"];
            let got = match table {
                "SavedEmbed" => fetch_json!(&pool, bot_db::models::SavedEmbed, "SavedEmbed", id),
                "CustomTag" => fetch_json!(&pool, bot_db::models::CustomTag, "CustomTag", id),
                "AutoTrigger" => fetch_json!(&pool, bot_db::models::AutoTrigger, "AutoTrigger", id),
                "Reminder" => fetch_json!(&pool, bot_db::models::Reminder, "Reminder", id),
                "Feed" => fetch_json!(&pool, bot_db::models::Feed, "Feed", id),
                "Infraction" => fetch_json!(&pool, bot_db::models::Infraction, "Infraction", id),
                "UserLevel" => fetch_json!(&pool, bot_db::models::UserLevel, "UserLevel", id),
                "EconomyAccount" => fetch_json!(&pool, bot_db::models::EconomyAccount, "EconomyAccount", id),
                "Giveaway" => fetch_json!(&pool, bot_db::models::Giveaway, "Giveaway", id),
                "Poll" => fetch_json!(&pool, bot_db::models::Poll, "Poll", id),
                "ReactionRoleGroup" => fetch_json!(&pool, bot_db::models::ReactionRoleGroup, "ReactionRoleGroup", id),
                "ReactionRoleItem" => fetch_json!(&pool, bot_db::models::ReactionRoleItem, "ReactionRoleItem", id),
                "TicketPanel" => fetch_json!(&pool, bot_db::models::TicketPanel, "TicketPanel", id),
                "TicketPanelOption" => fetch_json!(&pool, bot_db::models::TicketPanelOption, "TicketPanelOption", id),
                _ => fetch_json!(&pool, bot_db::models::Automation, "Automation", id),
            };
            assert_eq!(got, *want, "{table}.{which}: bot-db reads what Prisma reads");
            compared += 1;
        }
    }
    // The item builders on those rows answer what the JS answered for them.
    let rows = |t: &str| vec![baseline[t]["min"].clone(), baseline[t]["full"].clone()];
    let expect = |f: &str, first: &Value| -> Value {
        helpers["cases"].as_array().unwrap().iter().find(|c| c["fn"] == f && c["args"][0] == *first).map(|c| c["ok"].clone()).unwrap_or_else(|| panic!("{f}"))
    };
    for row in rows("SavedEmbed") {
        assert_eq!(embeds::embed_item(&row).to_string(), expect("embedItem", &row).to_string());
    }
    for row in rows("AutoTrigger") {
        assert_eq!(dash_collections::collections::triggers::trigger_item(&row).to_string(), expect("triggers.triggerItem", &row).to_string());
    }
    for row in rows("Infraction") {
        let profiles: HashMap<String, Option<MemberProfile>> = HashMap::new();
        let want = helpers["cases"].as_array().unwrap().iter().find(|c| c["fn"] == "cases.caseItem" && c["args"][0] == row).unwrap();
        let mut profiles = profiles;
        for (k, v) in want["args"][1].as_object().unwrap() {
            profiles.insert(k.clone(), serde_json::from_value(json!({ "id": v["id"], "username": v["username"], "globalName": v["globalName"], "avatarUrl": v["avatarUrl"].as_str().unwrap_or(""), "bot": v["bot"] })).ok());
        }
        assert_eq!(dash_collections::collections::cases::case_item(&row, &profiles, NOW).to_string(), want["ok"].to_string());
    }
    for row in rows("Automation") {
        let name_of = |_: &Value| None;
        let item = auto::automation_item(&row, &name_of, &["p1".to_string()], None);
        let want = helpers["cases"].as_array().unwrap().iter().find(|c| c["fn"] == "auto.automationItem" && c["args"][0] == row && c["args"][1] == json!(["p1"])).unwrap();
        // The JS run had a nameOf that only knows 'abcdefgh1'; the seeded rows have no such button.
        assert_eq!(item.to_string(), want["ok"].to_string());
    }
    assert_eq!(compared, 30);
    pool.close().await;
}
