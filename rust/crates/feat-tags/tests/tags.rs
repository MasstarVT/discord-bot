//! Port of `test/custom-commands.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: names, parsing, the action model, usage, payloads, access, trigger selection, cooldowns, the executor and
//! both hook bodies (through a fake `TagIo`), the `/tag` payload and plugin, command validation through the router,
//! and sweeps of the pure functions against the JS (`rust/fixtures/gen/feat-tags-js.mjs pure`).
//! With `TEST_DATABASE_URL` / `TEST_REDIS_URL`: raw SQL, caps, optimistic writes, counters, trigger references, the
//! autocomplete queries, the trigger cache and cooldowns in both directions, and `/tag` end to end.

use std::collections::HashMap;
use std::sync::Mutex;

use bot_core::command::option_type;
use bot_core::hooks::HookCtx;
use bot_core::interaction_helpers::{ChannelUsable, Payload, RoleCheck};
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, database_url, fake_message, redis_url, run_node};
use bot_db::{CooldownScope, CustomTag};
use bot_util::placeholder_parser::{PlaceholderGuild, PlaceholderUser, RenderContext};
use feat_tags::commands::tag as cmd;
use feat_tags::tag_service::{self as svc, *};
use serde_json::{Value, json};
use serenity::builder::CreateEmbed;

const JS: &str = "rust/fixtures/gen/feat-tags-js.mjs";
const U: &str = "111111111111111111";
const ROLE_A: &str = "222222222222222222";
const ROLE_B: &str = "666666666666666666";
const CH: &str = "333333333333333333";
const CH2: &str = "777777777777777777";

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_tags::register(&mut reg);
    reg
}

fn tag_row(over: Value) -> CustomTag {
    let mut base = json!({
        "id": "t1", "guildId": "g", "name": "hi", "content": "Hello {user}! {1:friend}", "createdBy": U, "uses": 0,
        "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "useEmbed": false,
        "description": null, "enabled": true, "actions": null, "allowRoleIds": [], "denyRoleIds": [], "allowChannelIds": [],
        "denyChannelIds": [], "cooldownScope": "NONE", "cooldownSeconds": 0, "deleteInvocation": true
    });
    for (k, v) in over.as_object().cloned().unwrap_or_default() {
        base[k] = v;
    }
    serde_json::from_value(base).expect("a CustomTag")
}

fn proj(over: Value) -> TriggerProjection {
    let mut base = json!({
        "id": "a", "trigger": "hello", "response": "Hi", "matchType": "CONTAINS", "caseSensitive": false, "cooldown": 0,
        "deleteMessage": false, "channelIds": [], "exemptRoleIds": [], "v": "2026-01-01T00:00:00.000Z"
    });
    for (k, v) in over.as_object().cloned().unwrap_or_default() {
        base[k] = v;
    }
    serde_json::from_value(base).expect("a projection")
}

fn actions(v: Value) -> Vec<TagAction> {
    normalize_actions(Some(&v)).unwrap_or_default()
}

fn to_js(list: &Option<Vec<TagAction>>) -> String {
    list.as_ref().map(|l| serde_json::to_string(&actions_to_json(l)).unwrap_or_default()).unwrap_or_else(|| "null".into())
}

fn js_pure(calls: &[Value]) -> Vec<Value> {
    let arg = serde_json::to_string(calls).unwrap_or_default();
    let out = run_node(JS, &["pure", &arg]);
    let out = out.as_array().cloned().unwrap_or_default();
    assert_eq!(out.len(), calls.len(), "one answer per call");
    out
}

/// `{ parse, roles }` of a serenity CreateAllowedMentions, empty lists dropped (the JS shape).
fn mentions_json(m: Option<&serenity::builder::CreateAllowedMentions>) -> Value {
    let v = m.map(|m| serde_json::to_value(m).unwrap_or(Value::Null)).unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    for key in ["parse", "roles", "users", "replied_user"] {
        match v.get(key) {
            Some(Value::Array(a)) if a.is_empty() && key != "parse" => {}
            Some(Value::Null) | None => {}
            Some(x) => {
                out.insert(if key == "replied_user" { "repliedUser".into() } else { key.into() }, x.clone());
            }
        }
    }
    Value::Object(out)
}

fn payload_json(p: &Payload) -> Value {
    json!({
        "content": p.content,
        "embeds": p.embeds.as_ref().map(|e| e.iter().map(|x| serde_json::to_value(x).unwrap_or(Value::Null)).collect::<Vec<_>>()).unwrap_or_default(),
        "allowedMentions": mentions_json(p.allowed_mentions.as_ref()),
        "ephemeral": p.ephemeral,
    })
}

fn embed_json(e: &CreateEmbed) -> Value {
    serde_json::to_value(e).unwrap_or(Value::Null)
}

// ── A fake Discord for the executor ───────────────────────────────────────────

#[derive(Default)]
struct FakeIo {
    /// channel id -> the bot can post there.
    channels: HashMap<String, bool>,
    /// role id -> (assignable reason, mentionable).
    roles: HashMap<String, (Option<String>, bool)>,
    bot_mentions_everyone: bool,
    manage_messages: bool,
    member_fetch: Option<Vec<String>>,
    /// "op" or "op:target" -> Discord error code.
    fail: HashMap<String, u64>,
    defer_fails: bool,
    log: Mutex<Vec<(String, String, Value)>>,
}

impl FakeIo {
    fn with_channel(mut self, id: &str, usable: bool) -> Self {
        self.channels.insert(id.into(), usable);
        self
    }
    fn with_role(mut self, id: &str, unassignable: Option<&str>, mentionable: bool) -> Self {
        self.roles.insert(id.into(), (unassignable.map(String::from), mentionable));
        self
    }
    fn failing(mut self, op: &str, code: u64) -> Self {
        self.fail.insert(op.into(), code);
        self
    }
    fn record(&self, op: &str, target: &str, body: Value) -> Result<(), IoError> {
        if let Some(code) = self.fail.get(&format!("{op}:{target}")).or_else(|| self.fail.get(op)) {
            return Err(IoError::code(*code));
        }
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push((op.into(), target.into(), body));
        Ok(())
    }
    fn calls(&self, op: &str) -> Vec<(String, Value)> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(o, _, _)| o == op).map(|(_, t, b)| (t.clone(), b.clone())).collect()
    }
    fn ops(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(o, _, _)| o.clone()).collect()
    }
}

impl TagIo for FakeIo {
    fn channel_exists(&self, channel_id: &str) -> bool {
        self.channels.contains_key(channel_id)
    }
    fn channel_usable(&self, channel_id: &str, _needs: &[&str]) -> ChannelUsable {
        match self.channels.get(channel_id) {
            Some(true) => ChannelUsable { ok: true, missing: vec![], reason: None },
            Some(false) => ChannelUsable { ok: false, missing: vec!["SendMessages".into()], reason: Some(format!("I need **Send Messages** in <#{channel_id}>.")) },
            None => ChannelUsable { ok: false, missing: vec![], reason: Some("Channel not found".into()) },
        }
    }
    fn check_assignable_role(&self, role_id: &str) -> RoleCheck {
        match self.roles.get(role_id) {
            Some((None, _)) => RoleCheck { ok: true, reason: None, dangerous: vec![] },
            Some((Some(r), _)) => RoleCheck { ok: false, reason: Some(r.clone()), dangerous: vec![] },
            None => RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: vec![] },
        }
    }
    fn role_exists(&self, role_id: &str) -> bool {
        self.roles.contains_key(role_id)
    }
    fn pingable_by_bot(&self, _channel_id: &str, role_ids: &[String]) -> Vec<String> {
        role_ids.iter().filter(|r| self.roles.get(*r).is_some_and(|(_, m)| *m || self.bot_mentions_everyone)).cloned().collect()
    }
    fn bot_can_manage_messages(&self, _channel_id: &str) -> bool {
        self.manage_messages
    }
    fn names(&self) -> &dyn NameLookup {
        &NoNames
    }
    async fn fetch_member_roles(&self, _user_id: &str) -> Option<Vec<String>> {
        self.member_fetch.clone()
    }
    async fn add_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), IoError> {
        self.record("add", role_id, json!([user_id, reason]))
    }
    async fn remove_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), IoError> {
        self.record("remove", role_id, json!([user_id, reason]))
    }
    async fn send_channel(&self, channel_id: &str, payload: Payload) -> Result<(), IoError> {
        self.record("send", channel_id, payload_json(&payload))
    }
    async fn send_dm(&self, user_id: &str, payload: Payload) -> Result<(), IoError> {
        self.record("dm", user_id, payload_json(&payload))
    }
    async fn delete_message(&self, _channel_id: &str, message_id: &str) -> Result<(), IoError> {
        let r = self.record("delete", message_id, Value::Null);
        if r.is_err() {
            self.log.lock().unwrap_or_else(|e| e.into_inner()).push(("delete-attempt".into(), message_id.into(), Value::Null));
        }
        r
    }
    async fn reply_notice(&self, _channel_id: &str, message_id: &str, embed: CreateEmbed) -> Result<String, IoError> {
        self.record("notice", message_id, embed_json(&embed)).map(|_| "notice-1".into())
    }
    fn schedule_delete(&self, _channel_id: &str, message_id: &str, after_ms: u64) {
        let _ = self.record("schedule-delete", message_id, json!(after_ms));
    }
    async fn interaction_reply(&self, payload: Payload) -> Result<(), IoError> {
        self.record("reply", "", payload_json(&payload))
    }
    async fn interaction_follow_up(&self, payload: Payload) -> Result<(), IoError> {
        self.record("followUp", "", payload_json(&payload))
    }
    async fn interaction_edit_reply(&self, payload: Payload) -> Result<(), IoError> {
        self.record("editReply", "", payload_json(&payload))
    }
    async fn interaction_defer_ephemeral(&self) -> bool {
        let _ = self.record("deferReply", "", json!({ "ephemeral": true }));
        !self.defer_fails
    }
    async fn interaction_safe_reply(&self, payload: Payload) {
        let _ = self.record("safeReply", "", payload_json(&payload));
    }
}

fn user_ctx() -> RenderContext {
    RenderContext {
        user: Some(PlaceholderUser { id: U.into(), username: Some("ann".into()), tag: Some("ann".into()), ..Default::default() }),
        ..Default::default()
    }
}

/// The JS scenario's render context: a user and a guild without an icon.
fn guild_ctx() -> RenderContext {
    RenderContext { guild: Some(PlaceholderGuild { id: "g".into(), name: Some("G".into()), ..Default::default() }), ..user_ctx() }
}

fn opts(source: Source, member: Option<Vec<String>>) -> RunOpts<'static> {
    RunOpts {
        source,
        guild_id: "g".into(),
        channel: ChannelScope::bare(CH),
        member_roles: member,
        user_id: U.into(),
        args: Vec::new(),
        settings: None,
        prefix: Some("!".into()),
        render: guild_ctx(),
        rng: None,
        check_permissions: false,
        ack_budget_ms: Some(2000),
        message: (source == Source::Prefix).then(|| InvokingMessage { id: "m1".into(), channel_id: CH.into(), channel_name: Some("chan".into()) }),
    }
}

fn offline_app() -> TestApp {
    let t = TestApp::new();
    configure_count_uses(&t, false);
    t
}

fn desc_of(v: &Value) -> String {
    v["embeds"][0]["description"].as_str().unwrap_or("").to_string()
}

// ── 1. Names ──────────────────────────────────────────────────────────────────

#[test]
fn names_normalisation_fixed_points_and_strict_validation() {
    assert_eq!(normalize_tag_name("Hello World"), "hello-world");
    assert_eq!(normalize_tag_name("FAQ!"), "faq");
    assert_eq!(normalize_tag_name("!!!"), "");
    assert_eq!(normalize_tag_name("  Rules "), "rules");
    assert_eq!(normalize_tag_name(&"a".repeat(60)).len(), 50);
    assert_eq!(normalize_tag_name(""), "");
    for stored in ["a", "abc_def-1", &"x".repeat(50), "0-9_"] {
        assert!(is_tag_name(stored));
        assert_eq!(normalize_tag_name(stored), stored, "{stored} is a fixed point");
    }
    assert!(validate_new_tag_name("c++").is_err(), "lossy names are refused, not stripped");
    assert_eq!(validate_new_tag_name("Server Rules"), Ok("server-rules".into()));
    assert!(validate_new_tag_name(&"a".repeat(33)).is_err());
    assert!(validate_new_tag_name(&"a".repeat(32)).is_ok());
    assert!(validate_new_tag_name("règles").is_err());
    assert!(validate_new_tag_name("").is_err());
}

// ── 2. Prefix parsing ─────────────────────────────────────────────────────────

#[test]
fn prefix_parsing_and_arguments() {
    assert_eq!(effective_prefix(Some("!")).as_deref(), Some("!"));
    assert_eq!(effective_prefix(Some("")), None, "an empty prefix means prefix commands are off");
    assert_eq!(effective_prefix(Some(" !")).as_deref(), Some("!"));
    assert_eq!(effective_prefix(Some("abcdef")).as_deref(), Some("!"));
    assert_eq!(effective_prefix(Some(">>")).as_deref(), Some(">>"));
    assert_eq!(effective_prefix(None).as_deref(), Some("!"));

    assert_eq!(parse_invocation("!rules a  b\nc", Some("!")), Some(Invocation { name: "rules".into(), args: vec!["a".into(), "b".into(), "c".into()] }));
    assert_eq!(parse_invocation("!Rules", Some("!")).map(|i| i.name), Some("rules".into()));
    for bad in ["! rules", "!!", "?rules", "!ru.les", &format!("!{}", "a".repeat(51))] {
        assert_eq!(parse_invocation(bad, Some("!")), None, "{bad}");
    }
    assert_eq!(parse_invocation(">>hi", Some(">>")), Some(Invocation { name: "hi".into(), args: vec![] }));
    assert_eq!(parse_invocation("!hi", None), None, "no prefix, no invocation");

    assert!(parse_args("  ").is_empty());
    assert_eq!(parse_args(&vec!["w"; 150].join(" ")).len(), 100);
    assert_eq!(parse_args("a \"b c\""), vec!["a", "\"b", "c\""], "quotes mean nothing");
}

/// Names, prefixes, invocations, arguments and display helpers on many inputs: the Rust answers equal the JS.
#[test]
fn parsing_and_display_agree_with_the_js() {
    let names = ["Hello World", "FAQ!", "!!!", "  Rules ", "c++", "Server\tRules", "règles", "ÉCOLE", "a b  c", "x_y-z", "İstanbul", "😀smile", ""];
    let prefixes: Vec<Value> = vec![json!("!"), json!(""), json!(" !"), json!("abcdef"), json!(">>"), json!("😀😀"), json!("a b"), json!("12345"), Value::Null];
    let contents = ["!rules a  b\nc", "!Rules", "! rules", "!!", "?rules", "!ru.les", "!hi\u{a0}there", "!ÀB", "!hi\u{2003}x y", "!x\t\ty  z  "];
    let texts = ["a  b\n c", "   ", "x`y`", "\u{feff}a\u{a0}b"];
    let mut calls: Vec<Value> = Vec::new();
    for n in names {
        calls.push(json!(["normalizeTagName", n]));
        calls.push(json!(["validateNewTagName", n]));
    }
    for p in &prefixes {
        calls.push(json!(["effectivePrefix", p]));
    }
    for c in contents {
        calls.push(json!(["parseInvocation", c, "!"]));
        calls.push(json!(["parseArgs", c]));
    }
    for t in texts {
        calls.push(json!(["oneLine", t]));
        calls.push(json!(["codeSafe", t]));
    }
    let js = js_pure(&calls);
    let mut i = 0;
    for n in names {
        assert_eq!(json!(normalize_tag_name(n)), js[i], "normalizeTagName {n:?}");
        let v = match validate_new_tag_name(n) {
            Ok(name) => json!({ "ok": true, "name": name }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(v, js[i + 1], "validateNewTagName {n:?}");
        i += 2;
    }
    for p in &prefixes {
        assert_eq!(json!(effective_prefix(p.as_str())), js[i], "effectivePrefix {p}");
        i += 1;
    }
    for c in contents {
        let inv = parse_invocation(c, Some("!")).map(|v| json!({ "name": v.name, "args": v.args })).unwrap_or(Value::Null);
        assert_eq!(inv, js[i], "parseInvocation {c:?}");
        assert_eq!(json!(parse_args(c)), js[i + 1], "parseArgs {c:?}");
        i += 2;
    }
    for t in texts {
        assert_eq!(json!(one_line(t)), js[i], "oneLine {t:?}");
        assert_eq!(json!(code_safe(t)), js[i + 1], "codeSafe {t:?}");
        i += 2;
    }
}

// ── 3. Actions ────────────────────────────────────────────────────────────────

#[test]
fn the_action_model() {
    assert_eq!(normalize_actions(None), None);
    assert_eq!(normalize_actions(Some(&json!({ "version": 2, "actions": [] }))), None, "an unknown format is not an array");
    assert_eq!(normalize_actions(Some(&json!([]))), Some(vec![]));
    let mixed = actions(json!([
        { "type": "reply", "responses": ["hi"] },
        { "type": "reply", "responses": ["", "  "] },
        { "type": "send", "responses": ["x"] },
        { "type": "addRole", "roleId": "nope" },
        { "type": "wat", "responses": ["x"] },
        null,
    ]));
    assert_eq!(serde_json::to_string(&actions_to_json(&mixed)).unwrap_or_default(), r#"[{"type":"reply","responses":["hi"],"embed":false,"dm":false}]"#);
    assert_eq!(actions(json!(vec![json!({ "type": "reply", "responses": ["x"] }); 7])).len(), 5, "capped at LIMITS.TAG_ACTIONS");
    assert_eq!(actions(json!([{ "type": "reply", "responses": vec!["x"; 15] }]))[0].responses.len(), 10);
    let pings = actions(json!([
        { "type": "reply", "dm": true, "responses": ["x"], "pingRoleIds": [ROLE_A] },
        { "type": "reply", "responses": ["x"], "pingRoleIds": [ROLE_A, ROLE_A, ROLE_B, "bad", "1".repeat(17), "2".repeat(17), "3".repeat(17), "4".repeat(17), "5".repeat(17)] },
    ]));
    assert!(pings[0].ping_role_ids.is_empty(), "a DM reply never pings");
    assert_eq!(pings[1].ping_role_ids.len(), 5);

    let legacy = tag_row(json!({ "content": "x", "useEmbed": true }));
    assert_eq!(get_effective_actions(&legacy), vec![TagAction::reply(false, true, "x")]);
    let unknown = tag_row(json!({ "actions": { "version": 2 }, "content": "legacy" }));
    assert_eq!(get_effective_actions(&unknown), legacy_actions("legacy", false));
    let cols = sync_legacy_columns(&actions(json!([{ "type": "addRole", "roleId": ROLE_A }, { "type": "send", "channelId": CH, "responses": ["a", "b"], "embed": false }])));
    assert_eq!(cols, vec![("content", json!("a")), ("useEmbed", json!(false))]);
    assert!(sync_legacy_columns(&actions(json!([{ "type": "addRole", "roleId": ROLE_A }]))).is_empty());
    let list = actions(json!([
        { "type": "reply", "dm": true, "responses": ["a"] },
        { "type": "send", "channelId": CH, "responses": ["b"] },
        { "type": "reply", "dm": false, "responses": ["c"] },
    ]));
    assert_eq!(primary_message_index(&list), Some(0));
    assert_eq!(channel_reply_index(&list), Some(2));
    assert_eq!(primary_message_index(&actions(json!([{ "type": "addRole", "roleId": ROLE_A }]))), None);
    assert_eq!(channel_reply_index(&[]), None);
}

#[test]
fn normalize_actions_agrees_with_the_js_key_for_key() {
    let inputs = vec![
        Value::Null,
        json!({ "version": 2 }),
        json!("text"),
        json!([]),
        json!([{ "type": "reply", "responses": ["hi"] }, { "type": "reply", "responses": ["", "  "] }, { "type": "send", "responses": ["x"] }, null, 5]),
        json!([{ "type": "send", "channelId": CH, "responses": ["a", 3, "b"], "embed": "true", "pingRoleIds": [ROLE_A, ROLE_A, "x"] }]),
        json!([{ "type": "reply", "dm": true, "embed": true, "responses": ["x"], "pingRoleIds": [ROLE_A] }, { "type": "toggleRole", "roleId": ROLE_B }, { "type": "removeRole", "roleId": "12" }]),
        json!(vec![json!({ "type": "addRole", "roleId": ROLE_A }); 7]),
        json!([{ "type": "reply", "responses": vec!["r"; 12], "dm": "yes" }]),
        json!([{ "type": "send", "channelId": "1".repeat(21), "responses": ["x"] }]),
    ];
    let calls: Vec<Value> = inputs.iter().map(|v| json!(["normalizeActions", v])).collect();
    let js = js_pure(&calls);
    for (v, expected) in inputs.iter().zip(js) {
        let ours = to_js(&normalize_actions(if v.is_null() { None } else { Some(v) }));
        assert_eq!(ours, serde_json::to_string(&expected).unwrap_or_default(), "normalizeActions {v}");
    }
}

// ── 4. Usage and arguments ────────────────────────────────────────────────────

#[test]
fn usage_text_argument_plans_and_random_picks() {
    let t = |content: &str, name: &str| tag_row(json!({ "name": name, "content": content }));
    assert_eq!(build_usage(&t("Hi {1}, {2:friend} {3...}", "greet"), Some("!"), false), "!greet <1> [more…]");
    assert_eq!(build_usage(&t("Hi {1}, {2:friend} {3...}", "greet"), Some("!"), true), "/tag use name:greet args:<1> [more…]");
    assert_eq!(build_usage(&t("{1} and {2}", "x"), Some("!"), false), "!x <1> <2>");
    assert_eq!(build_usage(&t("{2}", "x"), Some("!"), false), "!x <1> <2>", "arguments are positional");
    assert_eq!(build_usage(&t("no args", "x"), Some("!"), false), "!x");
    assert_eq!(build_usage(&t("{1}", "x"), None, false), "/tag use name:x args:<1>", "prefix off falls back to the slash form");

    let zero = || 0.0;
    let ctx = |a: &[&str]| RenderContext { args: Some(a.iter().map(|s| s.to_string()).collect()), ..Default::default() };
    assert_eq!(plan_run(get_effective_actions(&t("{1} {2}", "x")), &ctx(&["a"]), &zero).missing, vec![2]);
    assert!(plan_run(get_effective_actions(&t("{1} {2}", "x")), &ctx(&["a", "b"]), &zero).missing.is_empty());
    let abc = ["a".to_string(), "b".into(), "c".into()];
    assert_eq!(pick_response(&abc, &|| 0.99), "c");
    assert_eq!(pick_response(&abc, &|| 0.0), "a");
    assert_eq!(pick_response(&["a".into()], &|| 5.0), "a", "the index is clamped");
}

#[test]
fn usage_and_plans_agree_with_the_js() {
    let contents = [
        "Hi {1}, {2:friend} {3...}",
        "{1} and {2}",
        "{2}",
        "no args",
        "{...}",
        "{2...4} {...3}",
        "{1:}",
        "{1:a{b}",
        "{12:x}",
        "{1234:x}",
        "{1...}{",
        "{:x}",
        "{4} {user} {random}",
        "{1:",
    ];
    let mut calls = Vec::new();
    for c in contents {
        let tag = json!({ "id": "t", "guildId": "g", "name": "x", "content": c, "useEmbed": false, "actions": null });
        calls.push(json!(["buildUsage", tag, { "prefix": "!" }]));
        calls.push(json!(["buildUsage", tag, { "prefix": null }]));
        calls.push(json!(["planRun", tag, ["a"], 0]));
    }
    let multi = json!({ "id": "t", "guildId": "g", "name": "m", "content": "x", "actions": [
        { "type": "reply", "responses": ["{1}", "{3:x}"] }, { "type": "addRole", "roleId": ROLE_A }, { "type": "send", "channelId": CH, "responses": ["{2} {...}"] }
    ] });
    calls.push(json!(["buildUsage", multi, { "prefix": "?" }]));
    calls.push(json!(["planRun", multi, [], 0.6]));
    let js = js_pure(&calls);
    let mut i = 0;
    for c in contents {
        let tag = tag_row(json!({ "id": "t", "name": "x", "content": c }));
        assert_eq!(json!(build_usage(&tag, Some("!"), false)), js[i], "buildUsage {c:?}");
        assert_eq!(json!(build_usage(&tag, None, false)), js[i + 1], "buildUsage(null) {c:?}");
        let ctx = RenderContext { args: Some(vec!["a".into()]), ..Default::default() };
        let plan = plan_run(get_effective_actions(&tag), &ctx, &|| 0.0);
        assert_eq!(json!({ "steps": plan.steps.iter().map(|s| s.response.clone()).collect::<Vec<_>>(), "missing": plan.missing }), js[i + 2], "planRun {c:?}");
        i += 3;
    }
    let m = tag_row(json!({ "name": "m", "content": "x", "actions": multi["actions"] }));
    assert_eq!(json!(build_usage(&m, Some("?"), false)), js[i]);
    let plan = plan_run(get_effective_actions(&m), &RenderContext { args: Some(vec![]), ..Default::default() }, &|| 0.6);
    assert_eq!(json!({ "steps": plan.steps.iter().map(|s| s.response.clone()).collect::<Vec<_>>(), "missing": plan.missing }), js[i + 1]);
}

// ── 5. Payloads ───────────────────────────────────────────────────────────────

fn rendered_json(r: &Option<bot_core::custom_embed::Rendered>) -> Value {
    let Some(r) = r else { return Value::Null };
    let embeds: Vec<Value> = r
        .embeds
        .iter()
        .map(|e| {
            let mut e = e.clone();
            let has = e.as_object_mut().and_then(|o| o.remove("timestamp")).is_some();
            e["hasTimestamp"] = json!(has);
            e
        })
        .collect();
    json!({ "content": r.content, "embeds": embeds, "allowedMentions": mentions_json(Some(&r.allowed_mentions)) })
}

#[test]
fn payloads_mentions_clamping_and_colours_agree_with_the_js() {
    let reply = json!({ "type": "reply", "responses": ["x"] });
    let ping = json!({ "type": "reply", "responses": ["x"], "pingRoleIds": [ROLE_A] });
    let embed = json!({ "type": "reply", "embed": true, "responses": ["x"] });
    let long = format!("{}{{user}}", "x".repeat(1995));
    let cases: Vec<(Value, String, Value, Option<Vec<String>>)> = vec![
        (reply.clone(), "{user} hi".into(), Value::Null, None),
        (ping.clone(), "{user} hi".into(), Value::Null, None),
        (ping.clone(), "hi".into(), Value::Null, Some(vec![])),
        (reply.clone(), long.clone(), Value::Null, None),
        (embed.clone(), format!("{}{{user}}", "x".repeat(4100)), Value::Null, None),
        (reply.clone(), "{server.icon}".into(), Value::Null, None),
        (reply.clone(), "y".repeat(2500), Value::Null, None),
        (embed.clone(), "hi".into(), json!({ "embedColor": 0x123456 }), None),
        (embed.clone(), "hi".into(), json!({ "embedColor": -5 }), None),
        (embed.clone(), "hi".into(), json!({ "embedColor": 0x1000000 }), None),
    ];
    let calls: Vec<Value> = cases
        .iter()
        .map(|(a, r, s, p)| json!(["buildMessagePayload", a, r, U, s, p.as_ref().map(|p| json!({ "pingRoleIds": p })).unwrap_or(json!({}))]))
        .collect();
    let js = js_pure(&calls);
    for ((a, r, s, p), expected) in cases.iter().zip(js) {
        let action = actions(json!([a]))[0].clone();
        let settings: Option<bot_db::GuildSettings> =
            s.as_object().map(|o| bot_core::settings::MemorySettingsStore::new_row("g", o.clone()).expect("a settings row"));
        let ours = build_message_payload(&action, r, &user_ctx(), settings.as_ref(), p.as_deref());
        assert_eq!(rendered_json(&ours), expected, "{a} {r:.40}");
    }
}

// ── 5b. The slash executor ────────────────────────────────────────────────────

#[tokio::test]
async fn slash_fallback_an_unsendable_primary_reply_never_leaves_the_interaction_unacknowledged() {
    let t = offline_app();
    let io = FakeIo::default();
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [{ "type": "reply", "responses": ["{server.icon}"] }, { "type": "addRole", "roleId": ROLE_A }] }));
    let res = run_tag(&t, &tag, opts(Source::Slash, Some(vec![])), &io).await;
    assert_eq!(res.status, RunStatus::Ran);
    assert!(!res.success, "{res:?} {:?}", io.log.lock().map(|l| l.clone()).unwrap_or_default());
    assert_eq!(io.ops(), vec!["deferReply", "editReply"]);
    let text = desc_of(&io.calls("editReply")[0].1);
    assert!(text.contains(FailureCode::Empty.text()), "{text}");
    assert!(text.contains(FailureCode::RoleGone.text()), "{text}");
    assert!(text.contains("⚠️ Action 2 (Give (deleted)): The role no longer exists."), "{text}");
}

#[tokio::test]
async fn slash_the_primary_reply_goes_first_other_actions_follow_failures_arrive_ephemerally() {
    let t = offline_app();
    let io = FakeIo::default();
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [
        { "type": "addRole", "roleId": ROLE_A }, { "type": "reply", "responses": ["Hi {user}"] }, { "type": "reply", "dm": true, "responses": ["psst"] }
    ] }));
    let res = run_tag(&t, &tag, opts(Source::Slash, Some(vec![])), &io).await;
    assert!(res.success);
    assert_eq!(io.ops(), vec!["reply", "dm", "followUp"], "no defer: the reply is the acknowledgement");
    let reply = &io.calls("reply")[0].1;
    assert_eq!(reply["content"], format!("Hi <@{U}>"));
    assert_eq!(reply["allowedMentions"], json!({ "parse": ["users"] }));
    let follow = &io.calls("followUp")[0].1;
    assert_eq!(follow["ephemeral"], true);
    assert!(desc_of(follow).contains(FailureCode::RoleGone.text()));
    assert_eq!(follow["embeds"][0]["title"], "⚠️  Some Actions Failed");
}

#[tokio::test]
async fn slash_access_empty_usage_and_cooldown_store_answer_ephemerally() {
    let t = offline_app();
    let run = |tag: CustomTag| {
        let t = &t;
        async move {
            let io = FakeIo::default();
            let res = run_tag(t, &tag, opts(Source::Slash, Some(vec![])), &io).await;
            (res, io.calls("safeReply"))
        }
    };
    let (r, calls) = run(tag_row(json!({ "denyChannelIds": [CH] }))).await;
    assert_eq!(r.status, RunStatus::Denied);
    assert_eq!(calls[0].1["ephemeral"], true);
    assert!(desc_of(&calls[0].1).contains("can't be used in this channel"));
    let (_, calls) = run(tag_row(json!({ "allowRoleIds": [ROLE_A] }))).await;
    assert!(desc_of(&calls[0].1).contains("don't have a role"));
    let (r, calls) = run(tag_row(json!({ "actions": [] }))).await;
    assert_eq!(r.status, RunStatus::Empty);
    assert!(desc_of(&calls[0].1).contains("no actions"));
    let (r, calls) = run(tag_row(json!({ "content": "Hi {1}" }))).await;
    assert_eq!(r.status, RunStatus::Usage);
    assert_eq!(desc_of(&calls[0].1), "Usage: `/tag use name:hi args:<1>`");
    let (r, calls) = run(tag_row(json!({ "content": "Hi {1}", "description": "Says hi" }))).await;
    assert_eq!(r.status, RunStatus::Usage);
    assert_eq!(desc_of(&calls[0].1), "Usage: `/tag use name:hi args:<1>`\nSays hi");

    t.cache_backend.set_available(false);
    let (r, calls) = run(tag_row(json!({ "cooldownScope": "USER", "cooldownSeconds": 30 }))).await;
    assert_eq!(r.status, RunStatus::Unavailable, "fail closed while Redis is down");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["ephemeral"], true);
    assert_eq!(calls[0].1["embeds"][0]["title"], "⚠️  Temporarily Unavailable");
}

// ── 6. Scope and access ───────────────────────────────────────────────────────

fn scope(id: &str, thread: bool, parent: Option<&str>, parent_parent: Option<&str>) -> ChannelScope {
    ChannelScope { id: id.into(), thread, parent_id: parent.map(String::from), parent_parent_id: parent_parent.map(String::from) }
}

#[test]
fn channel_scope_ids_and_the_access_matrix() {
    let thread = scope("t", true, Some("c"), Some("cat"));
    let channel = scope("c", false, Some("cat"), None);
    assert_eq!(channel_scope_ids(Some(&thread)), vec!["t", "c", "cat"]);
    assert_eq!(channel_scope_ids(Some(&channel)), vec!["c", "cat"]);
    assert!(channel_scope_ids(None).is_empty());

    let member = vec!["r1".to_string()];
    let check = |over: Value, roles: Option<&[String]>, ch: &ChannelScope| {
        let tag = tag_row(over);
        check_tag_access(AccessLists::of(&tag), roles, Some(ch)).map_err(AccessCode::as_str)
    };
    assert_eq!(check(json!({ "denyChannelIds": ["c"] }), Some(&member), &thread), Err("CHANNEL_DENIED"));
    assert_eq!(check(json!({ "allowChannelIds": ["cat"] }), Some(&member), &channel), Ok(()));
    assert_eq!(check(json!({ "allowChannelIds": ["x"] }), Some(&member), &channel), Err("CHANNEL_NOT_ALLOWED"));
    assert_eq!(check(json!({ "denyRoleIds": ["r1"] }), Some(&member), &channel), Err("ROLE_DENIED"));
    assert_eq!(check(json!({ "allowRoleIds": ["r2"] }), Some(&member), &channel), Err("ROLE_NOT_ALLOWED"));
    assert_eq!(check(json!({ "allowRoleIds": ["r1"] }), Some(&member), &channel), Ok(()));
    assert_eq!(check(json!({ "allowRoleIds": ["r2"] }), None, &channel), Err("ROLE_NOT_ALLOWED"));
    assert_eq!(check(json!({}), None, &channel), Ok(()));
    assert_eq!(check(json!({ "denyChannelIds": ["c"], "allowRoleIds": ["r1"] }), Some(&member), &channel), Err("CHANNEL_DENIED"), "channel rules come first");
    let roles = |over: Value| check_tag_roles(AccessLists::of(&tag_row(over)), Some(&member)).map_err(AccessCode::as_str);
    assert_eq!(roles(json!({ "allowRoleIds": ["r2"], "denyChannelIds": ["c"] })), Err("ROLE_NOT_ALLOWED"));
    assert_eq!(roles(json!({ "denyRoleIds": ["r1"] })), Err("ROLE_DENIED"));
    assert_eq!(roles(json!({ "allowRoleIds": ["r1"], "denyChannelIds": ["c"] })), Ok(()), "channel rules do not hide a command");

    let applies = |channels: &[&str], exempt: &[&str], ch: &ChannelScope| {
        trigger_applies_here(&proj(json!({ "channelIds": channels, "exemptRoleIds": exempt })), Some(ch), Some(&member))
    };
    assert!(applies(&["c"], &[], &thread), "a thread matches its parent");
    assert!(applies(&["cat"], &[], &channel), "a category matches its channels");
    assert!(!applies(&["other"], &[], &channel));
    assert!(!applies(&[], &["r1"], &channel), "an exempt role is skipped");
    assert!(applies(&[], &["gone"], &channel), "stale ids never match");
}

#[test]
fn access_agrees_with_the_js() {
    let channels = [
        json!({ "id": "t", "thread": true, "parentId": "c", "parentParentId": "cat" }),
        json!({ "id": "c", "thread": false, "parentId": "cat" }),
        json!({ "id": "c" }),
        Value::Null,
    ];
    let lists = [
        json!({}),
        json!({ "denyChannelIds": ["c"] }),
        json!({ "allowChannelIds": ["cat"] }),
        json!({ "allowChannelIds": ["x"], "allowRoleIds": ["r1"] }),
        json!({ "denyRoleIds": ["r1"] }),
        json!({ "allowRoleIds": ["r2"], "denyRoleIds": ["r3"] }),
        json!({ "allowRoleIds": ["G"] }),
        json!({ "denyRoleIds": ["G"] }),
    ];
    // discord.js puts @everyone (the guild id, "G" here) in every member's role cache.
    let members = [json!(["G", "r1"]), json!(["G"]), Value::Null];
    let full = |l: &Value| {
        let mut b = json!({ "allowRoleIds": [], "denyRoleIds": [], "allowChannelIds": [], "denyChannelIds": [] });
        for (k, v) in l.as_object().cloned().unwrap_or_default() {
            b[k] = v;
        }
        b
    };
    let mut calls = Vec::new();
    for c in &channels {
        calls.push(json!(["channelScopeIds", c]));
        for l in &lists {
            for m in &members {
                calls.push(json!(["checkTagAccess", full(l), m, c]));
                calls.push(json!(["triggerAppliesHere", { "channelIds": full(l)["allowChannelIds"], "exemptRoleIds": full(l)["denyRoleIds"] }, c, m]));
            }
        }
    }
    let js = js_pure(&calls);
    let to_scope = |c: &Value| -> Option<ChannelScope> {
        c.as_object().map(|_| ChannelScope {
            id: c["id"].as_str().unwrap_or("").into(),
            thread: c["thread"] == true,
            parent_id: c["parentId"].as_str().map(String::from),
            parent_parent_id: c["parentParentId"].as_str().map(String::from),
        })
    };
    let mut i = 0;
    for c in &channels {
        let sc = to_scope(c);
        assert_eq!(json!(channel_scope_ids(sc.as_ref())), js[i]);
        i += 1;
        for l in &lists {
            for m in &members {
                let roles: Option<Vec<String>> = m.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
                let tag = tag_row(full(l));
                let got = match check_tag_access(AccessLists::of(&tag), roles.as_deref(), sc.as_ref()) {
                    Ok(()) => json!({ "ok": true }),
                    Err(code) => json!({ "ok": false, "code": code.as_str() }),
                };
                assert_eq!(got, js[i], "checkTagAccess {l} {m} {c}");
                let t = proj(json!({ "channelIds": full(l)["allowChannelIds"], "exemptRoleIds": full(l)["denyRoleIds"] }));
                assert_eq!(json!(trigger_applies_here(&t, sc.as_ref(), roles.as_deref())), js[i + 1], "triggerAppliesHere {l} {m} {c}");
                i += 2;
            }
        }
    }
}

/// discord.js's `member.roles.cache` holds @everyone (the guild id), so a list naming the guild matches every member.
#[tokio::test]
async fn everyone_is_one_of_every_members_roles() {
    let roles = member_role_ids(None, "G", &[serenity::all::RoleId::new(222222222222222222)]);
    assert_eq!(roles, vec!["G".to_string(), ROLE_A.to_string()]);
    let allow = tag_row(json!({ "allowRoleIds": ["G"] }));
    assert_eq!(check_tag_roles(AccessLists::of(&allow), Some(&roles)), Ok(()));
    let deny = tag_row(json!({ "denyRoleIds": ["G"] }));
    assert_eq!(check_tag_roles(AccessLists::of(&deny), Some(&roles)), Err(AccessCode::RoleDenied));
    assert!(!trigger_applies_here(&proj(json!({ "exemptRoleIds": ["G"] })), None, Some(&roles)));

    // Roles the guild no longer has do not count (the cache getter skips them).
    let t = TestApp::new().with_fake_guilds(&[bot_core::testing::FakeGuild::new(HG, "600000000000000099").role(ROLE_A, 1, serenity::all::Permissions::empty())]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, serenity::all::GuildId::new(HG.parse().unwrap_or(1)));
    let ids = member_role_ids(guild.as_ref(), HG, &[serenity::all::RoleId::new(222222222222222222), serenity::all::RoleId::new(666666666666666666)]);
    assert_eq!(ids, vec![HG.to_string(), ROLE_A.to_string()]);

    // The trigger hook: a trigger exempting @everyone never fires for a member.
    let t = hook_app(SettingsPatch::new()).await;
    prime_triggers(&t, HG, vec![proj(json!({ "exemptRoleIds": [HG] }))]);
    let io = FakeIo::default();
    let mut ctx = message_ctx(&t, "730000000000000001", "hello");
    ctx.data.member = Some(serde_json::from_value(json!({
        "user": { "id": U, "username": "ann", "discriminator": "0", "avatar": null }, "roles": [],
        "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "guild_id": HG
    })).expect("member"));
    handle_triggers(&ctx, &io, false).await;
    assert!(io.ops().is_empty());
}

// ── 7. Trigger validation and selection ───────────────────────────────────────

#[test]
fn trigger_pattern_validation() {
    assert!(validate_trigger_pattern("(a+)+", "REGEX", false).is_err());
    assert!(validate_trigger_pattern("hello", "CONTAINS", false).is_ok());
    assert!(validate_trigger_pattern("", "EXACT", false).is_err());
    assert!(validate_trigger_pattern("   ", "EXACT", false).is_err());
    assert!(validate_trigger_pattern(&"x".repeat(501), "CONTAINS", false).is_err());
    assert!(validate_trigger_pattern("^hi\\s+there$", "REGEX", true).is_ok());
    assert!(validate_trigger_pattern("(a", "REGEX", false).is_err(), "syntax errors");
    assert!(validate_trigger_pattern("(a)\\1", "REGEX", false).is_err(), "backreferences");
    assert!(validate_trigger_pattern(&"x".repeat(201), "REGEX", false).is_err(), "regexes are capped at 200 characters");
    assert_eq!(validate_trigger_pattern("x", "WAT", false), Err("Unknown match type.".into()));
}

#[tokio::test]
async fn trigger_selection_keeps_creation_order_and_runs_regexes_safely() {
    let t = offline_app();
    let ch = ChannelScope::bare(CH);
    let pick = |triggers: Vec<TriggerProjection>, content: &str| {
        prime_triggers(&t, "g1", triggers);
        let t = &t;
        let content = content.to_string();
        let ch = ch.clone();
        async move {
            let set = get_trigger_set(t, "g1").await.expect("primed");
            select_trigger(&set, &content, Some(&ch), None).map(|p| p.id.clone())
        }
    };
    assert_eq!(pick(vec![proj(json!({ "id": "A", "trigger": "hello" })), proj(json!({ "id": "B", "trigger": "hel+o", "matchType": "REGEX" }))], "hello").await.as_deref(), Some("A"));
    assert_eq!(pick(vec![proj(json!({ "id": "A", "trigger": "hel+o", "matchType": "REGEX" })), proj(json!({ "id": "B", "trigger": "hello" }))], "hello").await.as_deref(), Some("A"));
    assert_eq!(pick(vec![proj(json!({ "id": "A", "trigger": "bye" })), proj(json!({ "id": "B", "trigger": "h.llo", "matchType": "REGEX" }))], "hello").await.as_deref(), Some("B"));
    assert_eq!(
        pick(vec![proj(json!({ "id": "A", "trigger": "hello", "channelIds": ["other"] })), proj(json!({ "id": "B", "trigger": "hell" }))], "hello").await.as_deref(),
        Some("B"),
        "a trigger outside its channel is skipped"
    );
    assert_eq!(pick(vec![proj(json!({ "id": "A", "trigger": "nope" }))], "hello").await, None);

    assert!(matches_plain(&proj(json!({ "matchType": "EXACT", "trigger": "hi" })), "hi "), "EXACT compares trimmed text");
    assert!(!matches_plain(&proj(json!({ "matchType": "EXACT", "trigger": "hi" })), "hi there"));
    assert!(matches_plain(&proj(json!({ "trigger": "HELLO" })), "say hello"), "case-insensitive by default");
    assert!(!matches_plain(&proj(json!({ "trigger": "hello", "caseSensitive": true })), "HELLO"));
    assert!(matches_plain(&proj(json!({ "matchType": "STARTS_WITH", "trigger": "he" })), "hello"));
    assert!(matches_plain(&proj(json!({ "matchType": "ENDS_WITH", "trigger": "lo" })), "hello"));
    assert!(!matches_plain(&proj(json!({ "matchType": "REGEX", "trigger": "h" })), "hello"), "regexes never go through matchesPlain");

    // A legacy row with a catastrophic pattern compiles to nothing and is skipped without blocking.
    prime_triggers(&t, "g2", vec![proj(json!({ "id": "L", "trigger": "(a+)+$", "matchType": "REGEX" })), proj(json!({ "id": "B", "trigger": "a" }))]);
    let set = get_trigger_set(&t, "g2").await.expect("primed");
    assert!(set[0].re.is_none());
    let t0 = std::time::Instant::now();
    let hit = select_trigger(&set, &format!("{}!", "a".repeat(40)), Some(&ch), None).map(|p| p.id.clone());
    assert!(t0.elapsed().as_millis() < 500, "returned quickly");
    assert_eq!(hit.as_deref(), Some("B"));

    // Many safe regexes cost one batched call per message.
    let many: Vec<TriggerProjection> = (0..60).map(|i| proj(json!({ "id": format!("r{i}"), "trigger": format!("^word{i}\\b"), "matchType": "REGEX" }))).collect();
    prime_triggers(&t, "g3", many);
    let set = get_trigger_set(&t, "g3").await.expect("primed");
    let t0 = std::time::Instant::now();
    for _ in 0..20 {
        select_trigger(&set, "nothing that matches here", Some(&ch), None);
    }
    assert!(t0.elapsed().as_millis() < 1000);
    assert_eq!(select_trigger(&set, "word42 is it", Some(&ch), None).map(|p| p.id.as_str()), Some("r42"));
}

#[tokio::test]
async fn trigger_matching_and_selection_agree_with_the_js() {
    let triggers = vec![
        proj(json!({ "id": "1", "trigger": "Hello", "matchType": "EXACT" })),
        proj(json!({ "id": "2", "trigger": "wörld", "matchType": "CONTAINS", "caseSensitive": true })),
        proj(json!({ "id": "3", "trigger": "^ab+c$", "matchType": "REGEX" })),
        proj(json!({ "id": "4", "trigger": "end", "matchType": "ENDS_WITH", "channelIds": ["c"] })),
        proj(json!({ "id": "5", "trigger": "ÉT", "matchType": "STARTS_WITH" })),
        proj(json!({ "id": "6", "trigger": "x{2,}", "matchType": "REGEX", "caseSensitive": true, "exemptRoleIds": ["r1"] })),
    ];
    let contents = ["hello ", "WÖRLD", "a wörld", "ABBC", "the END", "état", "XX", "xx", "nothing", "\u{a0}Hello\u{a0}"];
    let scopes = [json!({ "id": "c" }), json!({ "id": "d" })];
    let members = [json!(["r1"]), Value::Null];
    let mut calls = vec![json!(["primeTriggers", triggers])];
    for c in contents {
        for s in &scopes {
            for m in &members {
                calls.push(json!(["selectTrigger", c, s, m]));
            }
        }
        for t in &triggers {
            calls.push(json!(["matchesPlain", t, c]));
        }
    }
    let js = js_pure(&calls);
    let t = offline_app();
    prime_triggers(&t, "gx", triggers.clone());
    let set = get_trigger_set(&t, "gx").await.expect("primed");
    let mut i = 1;
    for c in contents {
        for s in &scopes {
            for m in &members {
                let roles: Option<Vec<String>> = m.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
                let sc = ChannelScope::bare(s["id"].as_str().unwrap_or(""));
                let got = select_trigger(&set, c, Some(&sc), roles.as_deref()).map(|p| json!(p.id)).unwrap_or(Value::Null);
                assert_eq!(got, js[i], "selectTrigger {c:?} {s} {m}");
                i += 1;
            }
        }
        for tr in &triggers {
            assert_eq!(json!(matches_plain(tr, c)), js[i], "matchesPlain {} {c:?}", tr.id);
            i += 1;
        }
    }
}

// ── 8. Cooldowns ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn cooldowns_fail_closed_while_redis_is_down_and_keys_follow_the_scope() {
    let t = offline_app();
    t.cache_backend.set_available(false);
    assert_eq!(acquire_cooldown(&t, "tag:cd:g:t", 30).await, Cooldown::Unavailable);
    assert_eq!(tag_cooldown_key("t", CooldownScope::None, 0, "g", "u"), None);
    assert_eq!(tag_cooldown_key("t", CooldownScope::Server, 30, "g", "u").as_deref(), Some("tag:cd:g:t"));
    assert_eq!(tag_cooldown_key("t", CooldownScope::User, 30, "g", "u").as_deref(), Some("tag:cd:g:t:u"));
    assert_eq!(tag_cooldown_key("t", CooldownScope::User, 0, "g", "u"), None, "0 seconds is no cooldown");
    assert_eq!(trigger_cooldown_key("g", "t", "u"), "trigger:cd:g:t:u");
    assert_eq!(cooldown_label(CooldownScope::None, 0), "None");
    assert_eq!(cooldown_label(CooldownScope::Server, 90), "Server-wide · 1 minute 30 seconds");
    assert_eq!(cooldown_label(CooldownScope::User, 3600), "Per user · 1 hour");
    let js = js_pure(&[
        json!(["cooldownLabel", { "cooldownScope": "SERVER", "cooldownSeconds": 90061 }]),
        json!(["cooldownLabel", { "cooldownScope": "USER", "cooldownSeconds": 31536000 }]),
        json!(["tagCooldownKey", { "id": "t", "cooldownScope": "USER", "cooldownSeconds": 5 }, "g", "u"]),
    ]);
    assert_eq!(json!(cooldown_label(CooldownScope::Server, 90061)), js[0]);
    assert_eq!(json!(cooldown_label(CooldownScope::User, 31_536_000)), js[1]);
    assert_eq!(json!(tag_cooldown_key("t", CooldownScope::User, 5, "g", "u")), js[2]);
}

#[tokio::test]
async fn cooldown_paths_with_a_working_store_take_refuse_release_and_clear() {
    let t = offline_app();
    let tag = tag_row(json!({ "deleteInvocation": false, "cooldownScope": "USER", "cooldownSeconds": 60 }));
    let io = FakeIo::default().with_channel(CH, true);
    let first = run_tag(&t, &tag, opts(Source::Prefix, Some(vec![])), &io).await;
    assert!(first.success);
    let key = format!("tag:cd:g:t1:{U}");
    assert!(t.cache_backend.raw(&key).is_some(), "the key is taken");
    assert!(t.cache_backend.pttl(&key) > 59_000);
    let second = run_tag(&t, &tag, opts(Source::Prefix, Some(vec![])), &io).await;
    assert_eq!(second.status, RunStatus::Cooldown);
    let notice = io.calls("notice");
    assert_eq!(notice.len(), 1, "the cooldown notice is a reply");
    let until: i64 = t.cache_backend.raw(&key).and_then(|v| v.parse().ok()).unwrap_or(0);
    assert_eq!(notice[0].1["description"], format!("You can use this command again <t:{}:R>.", (until as f64 / 1000.0).ceil() as i64));
    assert_eq!(io.calls("schedule-delete")[0].1, json!(10_000));
    // A third try within 10 s is not noticed again.
    run_tag(&t, &tag, opts(Source::Prefix, Some(vec![])), &io).await;
    assert_eq!(io.calls("notice").len(), 1);

    // A failed run releases the key.
    let failing = FakeIo::default().with_channel(CH, true).failing("send", 50013);
    let other = tag_row(json!({ "id": "t2", "deleteInvocation": false, "cooldownScope": "SERVER", "cooldownSeconds": 60 }));
    let res = run_tag(&t, &other, opts(Source::Prefix, Some(vec![])), &failing).await;
    assert!(!res.success);
    assert!(t.cache_backend.raw("tag:cd:g:t2").is_none(), "a failed run does not burn the cooldown");

    // Server-wide wording, then clearing every key of the command.
    run_tag(&t, &other, opts(Source::Slash, Some(vec![])), &io).await;
    let slash = FakeIo::default();
    let r = run_tag(&t, &other, opts(Source::Slash, Some(vec![])), &slash).await;
    assert_eq!(r.status, RunStatus::Cooldown);
    assert!(desc_of(&slash.calls("safeReply")[0].1).starts_with("This command can be used again <t:"));
    t.cache_backend.put_raw("tag:cd:g:t2:u1", "1", Some(60_000));
    clear_tag_cooldowns(&t, "g", "t2").await;
    assert!(t.cache_backend.raw("tag:cd:g:t2").is_none() && t.cache_backend.raw("tag:cd:g:t2:u1").is_none());
}

// ── 9. The prefix hook ────────────────────────────────────────────────────────

const HG: &str = "940000000000000001";

async fn hook_app(settings: SettingsPatch) -> TestApp {
    let guild = bot_core::testing::FakeGuild::new(HG, "600000000000000099").role(ROLE_A, 1, serenity::all::Permissions::empty());
    let t = TestApp::new().with_fake_guilds(&[guild]);
    configure_count_uses(&t, false);
    t.settings().update_settings(HG, settings).await.expect("memory store");
    t
}

fn message_ctx(t: &TestApp, id: &str, content: &str) -> HookCtx<MessageCreate> {
    HookCtx::new(
        t.app.clone(),
        MessageCreate {
            guild_id: serenity::all::GuildId::new(HG.parse().unwrap_or(1)),
            message: fake_message(id, HG, CH, U, content),
            member: None,
            channel_id: serenity::all::ChannelId::new(CH.parse().unwrap_or(1)),
            channel: None,
        },
    )
}

fn hook_tag(over: Value) -> CustomTag {
    let mut t = tag_row(over);
    t.guild_id = HG.into();
    t
}

#[tokio::test]
async fn prefix_hook_runs_the_command_deletes_the_usage_and_stops_the_pipeline() {
    let t = hook_app(SettingsPatch::new().set("customPrefix", "!")).await;
    prime_tags(&t, HG, vec![hook_tag(json!({}))]);
    let io = FakeIo::default().with_channel(CH, true);
    let ctx = message_ctx(&t, "700000000000000001", "!hi Bob");
    handle_prefix_invocation(&ctx, &io, false).await;
    let sent = io.calls("send");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["content"], format!("Hello <@{U}>! Bob"));
    assert_eq!(sent[0].1["allowedMentions"], json!({ "parse": ["users"] }));
    assert_eq!(io.calls("delete").len(), 1);
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&t, "700000000000000001").as_deref(), Some("custom-command"));
    assert!(ctx.stopped());
    assert_eq!(ctx.state.get::<CustomCommandState>(CUSTOM_COMMAND_STATE), Some(CustomCommandState { tag_id: "t1".into(), name: "hi".into() }));
}

#[tokio::test]
async fn prefix_hook_unknown_names_other_prefixes_disabled_and_off_prefix_do_nothing() {
    let cases: Vec<(&str, Value, SettingsPatch, usize)> = vec![
        ("!unknown", json!({}), SettingsPatch::new().set("customPrefix", "!"), 0),
        ("?hi", json!({}), SettingsPatch::new().set("customPrefix", "!"), 0),
        ("!hi Bob", json!({ "enabled": false }), SettingsPatch::new().set("customPrefix", "!"), 0),
        ("!hi Bob", json!({}), SettingsPatch::new().set("customPrefix", ""), 0),
        (">>hi Bob", json!({}), SettingsPatch::new().set("customPrefix", ">>"), 1),
    ];
    for (content, over, settings, expected) in cases {
        let t = hook_app(settings).await;
        prime_tags(&t, HG, vec![hook_tag(over.clone())]);
        let io = FakeIo::default().with_channel(CH, true);
        let ctx = message_ctx(&t, "700000000000000002", content);
        handle_prefix_invocation(&ctx, &io, false).await;
        assert_eq!(io.calls("send").len(), expected, "{content} {over}");
        if expected == 0 {
            assert!(!ctx.stopped());
            assert!(!ctx.state.contains(CUSTOM_COMMAND_STATE), "{content}");
        }
    }
}

#[tokio::test]
async fn prefix_hook_delete_failures_and_delete_off() {
    for (fail, stopped, deleted_attempts) in [(Some(50013), false, 1), (Some(10008), true, 1), (None, true, 1)] {
        let t = hook_app(SettingsPatch::new()).await;
        prime_tags(&t, HG, vec![hook_tag(json!({}))]);
        let mut io = FakeIo::default().with_channel(CH, true);
        if let Some(code) = fail {
            io = io.failing("delete", code);
        }
        let ctx = message_ctx(&t, "700000000000000003", "!hi");
        handle_prefix_invocation(&ctx, &io, false).await;
        assert_eq!(io.calls("send").len(), 1);
        assert_eq!(io.calls("delete").len() + io.calls("delete-attempt").len(), deleted_attempts);
        assert_eq!(ctx.stopped(), stopped, "{fail:?}");
    }
    let t = hook_app(SettingsPatch::new()).await;
    prime_tags(&t, HG, vec![hook_tag(json!({ "deleteInvocation": false }))]);
    let io = FakeIo::default().with_channel(CH, true);
    let ctx = message_ctx(&t, "700000000000000004", "!hi");
    handle_prefix_invocation(&ctx, &io, false).await;
    assert_eq!(io.calls("send").len(), 1);
    assert!(io.calls("delete").is_empty());
    assert!(!ctx.stopped());
}

#[tokio::test]
async fn prefix_hook_cooldown_store_down_access_usage_throttle_and_deleted_messages() {
    // Fail closed while Redis is down.
    let t = hook_app(SettingsPatch::new()).await;
    t.cache_backend.set_available(false);
    prime_tags(&t, HG, vec![hook_tag(json!({ "cooldownScope": "USER", "cooldownSeconds": 30 }))]);
    let io = FakeIo::default().with_channel(CH, true);
    handle_prefix_invocation(&message_ctx(&t, "700000000000000005", "!hi"), &io, false).await;
    assert!(io.calls("send").is_empty());

    // Access lists are enforced, silently; the message still counts as an invocation.
    let t = hook_app(SettingsPatch::new()).await;
    prime_tags(&t, HG, vec![hook_tag(json!({ "denyChannelIds": [CH] }))]);
    let io = FakeIo::default().with_channel(CH, true);
    let ctx = message_ctx(&t, "700000000000000006", "!hi");
    handle_prefix_invocation(&ctx, &io, false).await;
    assert!(io.ops().is_empty());
    assert_eq!(ctx.state.get::<CustomCommandState>(CUSTOM_COMMAND_STATE).map(|s| s.name), Some("hi".into()));

    // Missing arguments answer with the usage, which deletes itself later.
    let t = hook_app(SettingsPatch::new()).await;
    prime_tags(&t, HG, vec![hook_tag(json!({ "content": "Hi {1}", "deleteInvocation": false }))]);
    let io = FakeIo::default().with_channel(CH, true);
    handle_prefix_invocation(&message_ctx(&t, "700000000000000007", "!hi"), &io, false).await;
    assert!(io.calls("send").is_empty());
    let notice = io.calls("notice");
    assert_eq!(notice.len(), 1);
    assert_eq!(notice[0].1["description"], "Usage: `!hi <1>`");
    assert_eq!(notice[0].1["title"], "⚠️  Missing Arguments");
    assert_eq!(io.calls("schedule-delete").len(), 1);

    // One invocation per user per second.
    let t = hook_app(SettingsPatch::new()).await;
    prime_tags(&t, HG, vec![hook_tag(json!({ "deleteInvocation": false }))]);
    let io = FakeIo::default().with_channel(CH, true);
    for n in 0..2 {
        handle_prefix_invocation(&message_ctx(&t, &format!("70000000000000001{n}"), "!hi"), &io, false).await;
    }
    assert_eq!(io.calls("send").len(), 1);

    // A message the bot already deleted is skipped.
    let t = hook_app(SettingsPatch::new()).await;
    prime_tags(&t, HG, vec![hook_tag(json!({}))]);
    bot_core::deletion_tracker::mark_bot_deletion(&t, &["700000000000000020"], Some("automod"), None);
    let io = FakeIo::default().with_channel(CH, true);
    handle_prefix_invocation(&message_ctx(&t, "700000000000000020", "!hi"), &io, false).await;
    assert!(io.ops().is_empty());
}

#[tokio::test]
async fn the_sync_filters() {
    let t = offline_app();
    assert!(is_possible_invocation(&t, "nomemo", "!hi"));
    assert!(!is_possible_invocation(&t, "nomemo", "h"));
    assert!(!is_possible_invocation(&t, "nomemo", ""));
    t.settings().update_settings(HG, SettingsPatch::new().set("customPrefix", ">>")).await.expect("memory store");
    assert!(is_possible_invocation(&t, HG, ">>hi"));
    assert!(!is_possible_invocation(&t, HG, "!hi"));
    t.settings().update_settings(HG, SettingsPatch::new().set("customPrefix", "")).await.expect("memory store");
    assert!(!is_possible_invocation(&t, HG, "!hi"), "prefix commands are off");

    assert!(!has_trigger_content(&t, "nomemo", "x", true));
    assert!(has_trigger_content(&t, "nomemo", "x", false));
    assert!(!has_trigger_content(&t, "nomemo", "", false));
    t.settings().update_settings(HG, SettingsPatch::new().set("autoTriggersEnabled", false)).await.expect("memory store");
    assert!(!has_trigger_content(&t, HG, "x", false));
}

// ── 10. The trigger hook ──────────────────────────────────────────────────────

async fn fire(trig: Value, settings: SettingsPatch, io: FakeIo, state: bool) -> (TestApp, FakeIo, HookCtx<MessageCreate>) {
    let t = hook_app(settings).await;
    let mut base = json!({ "response": "Hi {user} @everyone <@&222222222222222222>", "deleteMessage": true });
    for (k, v) in trig.as_object().cloned().unwrap_or_default() {
        base[k] = v;
    }
    prime_triggers(&t, HG, vec![proj(base)]);
    let ctx = message_ctx(&t, "710000000000000001", "well hello there");
    if state {
        ctx.state.insert(CUSTOM_COMMAND_STATE, CustomCommandState { tag_id: "t1".into(), name: "hi".into() });
    }
    handle_triggers(&ctx, &io, false).await;
    (t, io, ctx)
}

#[tokio::test]
async fn trigger_hook_sends_a_safe_reply_then_deletes_without_stopping() {
    let (t, io, ctx) = fire(json!({}), SettingsPatch::new(), FakeIo::default(), false).await;
    let sent = io.calls("send");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["allowedMentions"], json!({ "parse": ["users"] }));
    assert!(sent[0].1["content"].as_str().unwrap_or("").starts_with(&format!("Hi <@{U}>")));
    assert_eq!(io.calls("delete").len(), 1);
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&t, "710000000000000001").as_deref(), Some("trigger"));
    assert!(!ctx.stopped());

    let (_, io, _) = fire(json!({ "response": format!("{}{{user}}", "x".repeat(1995)) }), SettingsPatch::new(), FakeIo::default(), false).await;
    assert_eq!(io.calls("send")[0].1["content"].as_str().map(|s| s.encode_utf16().count()), Some(2000), "clamped after expansion");
}

#[tokio::test]
async fn trigger_hook_skips_failures_cooldowns_switches_and_empty_results() {
    let (_, io, _) = fire(json!({}), SettingsPatch::new(), FakeIo::default(), true).await;
    assert!(io.ops().is_empty(), "a message already answered by a custom command is skipped");
    let (_, io, _) = fire(json!({}), SettingsPatch::new(), FakeIo::default().failing("send", 50013), false).await;
    assert!(io.ops().is_empty(), "a failed send never deletes the message");
    let (_, io, _) = fire(json!({}), SettingsPatch::new().set("autoTriggersEnabled", false), FakeIo::default(), false).await;
    assert!(io.ops().is_empty(), "triggers switched off stay silent");
    let (_, io, _) = fire(json!({ "response": "{server.icon}" }), SettingsPatch::new(), FakeIo::default(), false).await;
    assert!(io.ops().is_empty(), "an empty result sends nothing: {:?}", io.log.lock().map(|l| l.clone()).unwrap_or_default());

    // Redis down: a trigger with a cooldown does not fire.
    let t = hook_app(SettingsPatch::new()).await;
    t.cache_backend.set_available(false);
    prime_triggers(&t, HG, vec![proj(json!({ "cooldown": 30 }))]);
    let io = FakeIo::default();
    handle_triggers(&message_ctx(&t, "710000000000000002", "hello"), &io, false).await;
    assert!(io.ops().is_empty());

    // With a working store: fires, then the cooldown ends the scan; a failed send releases it.
    let t = hook_app(SettingsPatch::new()).await;
    prime_triggers(&t, HG, vec![proj(json!({ "id": "cd", "cooldown": 30 })), proj(json!({ "id": "other", "response": "second" }))]);
    let io = FakeIo::default();
    handle_triggers(&message_ctx(&t, "710000000000000003", "hello"), &io, false).await;
    assert_eq!(io.calls("send").len(), 1);
    assert!(t.cache_backend.raw(&format!("trigger:cd:{HG}:cd:{U}")).is_some());
    t.clock.advance(1_100);
    tokio::time::sleep(std::time::Duration::from_millis(1_050)).await; // the per-channel throttle runs on real time
    handle_triggers(&message_ctx(&t, "710000000000000004", "hello"), &io, false).await;
    assert_eq!(io.calls("send").len(), 1, "the matching trigger on cooldown ends the scan");
}

#[tokio::test]
async fn trigger_hook_one_reply_per_channel_per_second_and_first_applicable_wins() {
    let t = hook_app(SettingsPatch::new()).await;
    prime_triggers(&t, HG, vec![proj(json!({ "deleteMessage": false }))]);
    let io = FakeIo::default();
    for n in 0..2 {
        handle_triggers(&message_ctx(&t, &format!("72000000000000000{n}"), "hello"), &io, false).await;
    }
    assert_eq!(io.calls("send").len(), 1);

    let t = hook_app(SettingsPatch::new()).await;
    prime_triggers(
        &t,
        HG,
        vec![
            proj(json!({ "id": "x", "trigger": "hello", "response": "exempt", "exemptRoleIds": [ROLE_A] })),
            proj(json!({ "id": "y", "trigger": "hello", "response": "second" })),
            proj(json!({ "id": "z", "trigger": "hello", "response": "third" })),
        ],
    );
    let io = FakeIo::default();
    let mut ctx = message_ctx(&t, "720000000000000010", "hello");
    ctx.data.member = Some(serde_json::from_value(json!({
        "user": { "id": U, "username": "ann", "discriminator": "0", "avatar": null }, "roles": [ROLE_A],
        "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "guild_id": HG
    })).expect("member"));
    handle_triggers(&ctx, &io, false).await;
    assert_eq!(io.calls("send").iter().map(|(_, b)| b["content"].clone()).collect::<Vec<_>>(), vec![json!("second")]);
}

// ── 11 / 12. Declarations ─────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["tag"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "custom-commands");
}

#[test]
fn command_payload_budget_and_permission_keys() {
    let def = cmd::command();
    let json = def.payload();
    let chars = cmd::data().char_count();
    assert!(chars <= 3500, "/tag is {chars} characters");
    let opts = json["options"].as_array().cloned().unwrap_or_default();
    assert!(opts.len() <= 25);
    assert!(json.get("default_member_permissions").is_none_or(Value::is_null), "members must see /tag use");
    let meta = def.metadata();
    assert_eq!(meta["module"], "customcommands");
    assert_eq!(meta["gated"], false);
    assert_eq!(meta["permissionLevel"], 0);
    let trig = opts.iter().find(|o| o["name"] == "trigger").cloned().unwrap_or(Value::Null);
    assert_eq!(trig["type"], 2);
    let subs: Vec<&str> = trig["options"].as_array().into_iter().flatten().filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(subs, vec!["enable", "disable", "add", "edit", "remove", "toggle", "info", "list"]);
    for key in ["action add", "action remove", "response add", "response remove", "create", "edit", "delete", "toggle", "cooldown", "permissions"] {
        assert_eq!(meta["subcommandPermissions"][key], 2, "{key}");
    }
    assert_eq!(meta["subcommandPermissions"]["action list"], 0);
}

#[test]
fn the_plugin_declares_exactly_the_two_hooks() {
    let reg = registry();
    let p = reg.plugin_named("custom-commands").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Customcommands));
    let hooks: Vec<(&str, &str)> = p.hooks.iter().map(|h| (h.event().as_str(), h.id())).collect();
    assert_eq!(hooks, vec![("messageCreate", "custom-commands"), ("messageCreate", "custom-commands:triggers")]);
    let at = |id: &str| bot_config::hook_index("messageCreate", id);
    assert!(at("custom-commands") < at("custom-commands:triggers"));
}

/// The static checks of the JS test (13), on the Rust sources.
#[test]
fn static_checks_on_the_owned_files() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for f in ["tag_service.rs", "plugin.rs", "tag_io.rs", "commands/tag.rs"] {
        let text = std::fs::read_to_string(dir.join(f)).unwrap_or_default();
        assert!(!text.is_empty(), "{f}");
        assert!(!text.contains("println!"), "{f}: no println");
        assert!(!text.contains("\"GuildSettings\""), "{f}: settings only through the service");
        assert!(!text.contains("Regex::new"), "{f}: user regexes only through safe_regex");
        assert!(!text.contains("now()"), "{f}: no bare now() in SQL");
    }
}

// ── 14. The executor: roles, DMs, sends, pings ────────────────────────────────

#[tokio::test]
async fn roles_are_added_and_removed_one_at_a_time_with_the_command_name() {
    let t = offline_app();
    let io = FakeIo::default().with_role(ROLE_A, None, false).with_role(ROLE_B, None, false);
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [
        { "type": "addRole", "roleId": ROLE_A }, { "type": "removeRole", "roleId": ROLE_B }, { "type": "toggleRole", "roleId": ROLE_A }
    ] }));
    let res = run_tag(&t, &tag, opts(Source::Prefix, Some(vec![ROLE_B.into()])), &io).await;
    assert!(res.success);
    let calls: Vec<(String, String)> = io.log.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(o, r, _)| (o.clone(), r.clone())).collect();
    assert_eq!(calls, vec![("add".into(), ROLE_A.into()), ("remove".into(), ROLE_B.into()), ("remove".into(), ROLE_A.into())]);
    assert_eq!(io.calls("add")[0].1, json!([U, "Custom command hi"]));

    // Already has / lacks: a success without an API call.
    let io = FakeIo::default().with_role(ROLE_A, None, false).with_role(ROLE_B, None, false);
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [{ "type": "addRole", "roleId": ROLE_A }, { "type": "removeRole", "roleId": ROLE_B }] }));
    let res = run_tag(&t, &tag, opts(Source::Prefix, Some(vec![ROLE_A.into()])), &io).await;
    assert!(res.success);
    assert!(io.ops().is_empty());
    assert_eq!(res.results[0].summary.as_deref(), Some(format!("ℹ️ You already have <@&{ROLE_A}>.").as_str()));
    assert_eq!(res.results[1].summary.as_deref(), Some(format!("ℹ️ You don't have <@&{ROLE_B}>.").as_str()));

    // No member on the message: fetched; not a member: MEMBER_GONE.
    let io = FakeIo { member_fetch: Some(vec![]), ..FakeIo::default() }.with_role(ROLE_A, None, false);
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [{ "type": "addRole", "roleId": ROLE_A }] }));
    assert!(run_tag(&t, &tag, opts(Source::Prefix, None), &io).await.success);
    let io = FakeIo::default().with_role(ROLE_A, None, false);
    let res = run_tag(&t, &tag, opts(Source::Prefix, None), &io).await;
    assert_eq!(res.results[0].code, Some(FailureCode::MemberGone));
}

#[tokio::test]
async fn unassignable_missing_roles_and_discord_errors_map_to_codes() {
    let t = offline_app();
    let io = FakeIo::default().with_role(ROLE_A, Some("That role is managed by an integration."), false).with_role(ROLE_B, Some("That role is above my highest role."), false);
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [
        { "type": "addRole", "roleId": ROLE_A }, { "type": "addRole", "roleId": ROLE_B }, { "type": "addRole", "roleId": "888888888888888888" }
    ] }));
    let res = run_tag(&t, &tag, opts(Source::Prefix, Some(vec![])), &io).await;
    assert!(!res.success);
    assert_eq!(res.results.iter().map(|r| r.code).collect::<Vec<_>>(), vec![Some(FailureCode::RoleUnassignable), Some(FailureCode::RoleUnassignable), Some(FailureCode::RoleGone)]);
    assert!(io.ops().is_empty());
    assert_eq!(failure_text(Some(FailureCode::RoleUnassignable), res.results[0].detail.as_deref()), "That role is managed by an integration.");

    let io = FakeIo::default().with_role(ROLE_A, None, false).failing("add", 50013).failing("dm", 50007).failing("send", 10003).with_channel(CH, true);
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [
        { "type": "addRole", "roleId": ROLE_A }, { "type": "reply", "dm": true, "responses": ["x"] }, { "type": "reply", "responses": ["y"] }
    ] }));
    let res = run_tag(&t, &tag, opts(Source::Prefix, Some(vec![])), &io).await;
    assert_eq!(res.results.iter().map(|r| r.code).collect::<Vec<_>>(), vec![Some(FailureCode::MissingPerms), Some(FailureCode::DmClosed), Some(FailureCode::ChannelGone)]);
    assert!(!res.success);
    assert_eq!(failure_text(Some(FailureCode::MissingPerms), None), "I'm missing permissions there.");
    assert_eq!(failure_text(Some(FailureCode::MissingPerms), Some("Send Messages")), "I'm missing permissions there: Send Messages.");
    assert_eq!(failure_text(None, None), "Discord rejected the request.");
}

#[tokio::test]
async fn send_actions_post_to_the_target_and_report_gone_or_unusable_ones() {
    let t = offline_app();
    let io = FakeIo::default().with_channel(CH, true).with_channel(CH2, true).with_channel("888888888888888881", false);
    let tag = tag_row(json!({ "deleteInvocation": false, "actions": [
        { "type": "send", "channelId": CH2, "responses": ["to target"] },
        { "type": "send", "channelId": "888888888888888882", "responses": ["gone"] },
        { "type": "send", "channelId": "888888888888888881", "responses": ["locked"] },
    ] }));
    let mut o = opts(Source::Prefix, Some(vec![]));
    o.check_permissions = true;
    let res = run_tag(&t, &tag, o, &io).await;
    assert_eq!(res.results.iter().map(|r| r.ok).collect::<Vec<_>>(), vec![true, false, false]);
    assert_eq!(res.results[1].code, Some(FailureCode::ChannelGone));
    assert_eq!(res.results[2].code, Some(FailureCode::MissingPerms));
    assert_eq!(res.results[2].detail.as_deref(), Some("Send Messages"));
    let sent = io.calls("send");
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].0.as_str(), sent[0].1["content"].clone()), (CH2, json!("to target")));
    assert_eq!(res.results[0].summary.as_deref(), Some(format!("📤 Posted in <#{CH2}>.").as_str()));
}

#[tokio::test]
async fn role_pings_follow_pingable_role_ids_with_the_bot_as_sender() {
    let t = offline_app();
    let reply = tag_row(json!({ "deleteInvocation": false, "actions": [{ "type": "reply", "responses": ["hi"], "pingRoleIds": [ROLE_A] }] }));
    for (io, expected) in [
        (FakeIo::default().with_role(ROLE_A, None, false), json!({ "parse": ["users"] })),
        (FakeIo::default().with_role(ROLE_A, None, true), json!({ "parse": ["users"], "roles": [ROLE_A] })),
        (FakeIo { bot_mentions_everyone: true, ..FakeIo::default() }.with_role(ROLE_A, None, false), json!({ "parse": ["users"], "roles": [ROLE_A] })),
        (FakeIo::default(), json!({ "parse": ["users"] })),
    ] {
        let io = io.with_channel(CH, true);
        run_tag(&t, &reply, opts(Source::Prefix, Some(vec![])), &io).await;
        assert_eq!(io.calls("send")[0].1["allowedMentions"], expected);
    }
}

#[tokio::test]
async fn prefix_permissions_empty_lists_random_replies_and_the_usage_delete() {
    let t = offline_app();
    // A prefix reply respects the channel permissions when they are checked.
    let io = FakeIo::default().with_channel(CH, false);
    let mut o = opts(Source::Prefix, Some(vec![]));
    o.check_permissions = true;
    let res = run_tag(&t, &tag_row(json!({ "deleteInvocation": false })), o, &io).await;
    assert_eq!(res.results[0].code, Some(FailureCode::MissingPerms));
    assert!(io.calls("send").is_empty());

    // A hand-edited empty action list stays silent for prefix use.
    let io = FakeIo::default().with_channel(CH, true);
    let res = run_tag(&t, &tag_row(json!({ "actions": [] })), opts(Source::Prefix, Some(vec![])), &io).await;
    assert_eq!(res.status, RunStatus::Empty);
    assert!(io.ops().is_empty());

    // Random replies pick one response per run.
    let io = FakeIo::default().with_channel(CH, true);
    let mut o = opts(Source::Prefix, Some(vec![]));
    o.rng = Some(std::sync::Arc::new(|| 0.5));
    run_tag(&t, &tag_row(json!({ "deleteInvocation": false, "actions": [{ "type": "reply", "responses": ["one", "two", "three"] }] })), o, &io).await;
    assert_eq!(io.calls("send")[0].1["content"], "two");

    // A successful delete is reported; a failed run neither deletes nor counts.
    let io = FakeIo::default().with_channel(CH, true);
    let res = run_tag(&t, &tag_row(json!({})), opts(Source::Prefix, Some(vec![])), &io).await;
    assert!(res.deleted);
    let io = FakeIo::default().with_channel(CH, true).failing("send", 50013);
    let res = run_tag(&t, &tag_row(json!({})), opts(Source::Prefix, Some(vec![])), &io).await;
    assert!(!res.success && !res.deleted);
    assert!(io.calls("delete").is_empty());

    // Without Manage Messages the usage stays.
    let io = FakeIo::default().with_channel(CH, true);
    let mut o = opts(Source::Prefix, Some(vec![]));
    o.check_permissions = true;
    let res = run_tag(&t, &tag_row(json!({})), o, &io).await;
    assert!(res.success && !res.deleted);

    // Slash after a defer: further channel replies go through channel.send, and the summary lists them.
    let io = FakeIo::default().with_channel(CH, true);
    let tag = tag_row(json!({ "actions": [{ "type": "reply", "dm": true, "responses": ["a"] }, { "type": "send", "channelId": CH, "responses": ["b"] }] }));
    let res = run_tag(&t, &tag, opts(Source::Slash, Some(vec![])), &io).await;
    assert!(res.success);
    assert_eq!(io.ops(), vec!["deferReply", "dm", "send", "editReply"]);
    let edit = &io.calls("editReply")[0].1;
    assert_eq!(edit["embeds"][0]["title"], "✅  Done");
    assert_eq!(desc_of(edit), format!("📨 Sent you a DM.\n📤 Posted in <#{CH}>."));
}

// ── 15. Display helpers ───────────────────────────────────────────────────────

#[test]
fn display_helpers() {
    let a = |v: Value| actions(json!([v]))[0].clone();
    assert_eq!(describe_action(&a(json!({ "type": "reply", "responses": ["a", "b"], "embed": true })), None), "💬 Reply in channel · embed · 2 replies");
    assert_eq!(describe_action(&a(json!({ "type": "reply", "dm": true, "responses": ["a"] })), None), "📨 Reply by DM · 1 reply");
    assert_eq!(
        describe_action(&a(json!({ "type": "send", "channelId": CH, "responses": ["a"], "pingRoleIds": [ROLE_A] })), None),
        format!("📤 Send to <#{CH}> · 1 reply · pings <@&{ROLE_A}>")
    );
    assert_eq!(describe_action(&a(json!({ "type": "toggleRole", "roleId": ROLE_A })), None), format!("🔁 Toggle <@&{ROLE_A}>"));
    assert_eq!(action_label(&a(json!({ "type": "send", "channelId": CH, "responses": ["a"] })), &NoNames), "Send to (deleted)");
    assert_eq!(action_label(&a(json!({ "type": "reply", "dm": true, "responses": ["a"] })), &NoNames), "Reply by DM");
    assert_eq!(code_safe("a`b"), "aˋb");
    assert_eq!(one_line("  a \n\t b  "), "a b");
    assert_eq!(format_id_list(&[], IdKind::Role, None, "Everyone", 1024), "Everyone");
    assert_eq!(format_id_list(&[ROLE_A.into(), ROLE_B.into()], IdKind::Role, None, "x", 1024), format!("<@&{ROLE_A}>, <@&{ROLE_B}>"));

    let samples = [
        json!({ "type": "reply", "responses": ["a", "b"], "embed": true }),
        json!({ "type": "reply", "dm": true, "responses": ["a"] }),
        json!({ "type": "send", "channelId": CH, "responses": ["a"], "pingRoleIds": [ROLE_A, ROLE_B] }),
        json!({ "type": "addRole", "roleId": ROLE_A }),
        json!({ "type": "removeRole", "roleId": ROLE_A }),
        json!({ "type": "toggleRole", "roleId": ROLE_A }),
    ];
    let mut calls = Vec::new();
    for s in &samples {
        let normal = actions(json!([s]))[0].to_json();
        calls.push(json!(["describeAction", normal]));
        calls.push(json!(["actionLabel", normal]));
    }
    calls.push(json!(["formatIdList", [ROLE_A, CH], { "kind": "channel" }]));
    calls.push(json!(["formatIdList", [], { "kind": "role", "empty": "Everyone" }]));
    let js = js_pure(&calls);
    for (i, s) in samples.iter().enumerate() {
        let x = actions(json!([s]))[0].clone();
        assert_eq!(json!(describe_action(&x, None)), js[2 * i], "describeAction {s}");
        assert_eq!(json!(action_label(&x, &NoNames)), js[2 * i + 1], "actionLabel {s}");
    }
    let n = samples.len() * 2;
    assert_eq!(json!(format_id_list(&[ROLE_A.into(), CH.into()], IdKind::Channel, None, "None", 1024)), js[n]);
    assert_eq!(json!(format_id_list(&[], IdKind::Role, None, "Everyone", 1024)), js[n + 1]);
}

#[test]
fn command_helpers() {
    assert_eq!(cmd::length_problem(&"x".repeat(2001), false).as_deref(), Some("Plain-text replies can be at most 2,000 characters (this one is 2001). Use `embed:True` for up to 4,000."));
    assert_eq!(cmd::length_problem(&"x".repeat(4001), true).as_deref(), Some("Embed replies can be at most 4,000 characters (this one is 4001)."));
    assert_eq!(cmd::length_problem("ok", false), None);
    assert_eq!(cmd::clean_description(Some("  ")), None);
    assert_eq!(cmd::clean_description(Some(&"d".repeat(150))).map(|d| d.encode_utf16().count()), Some(100));
    assert_eq!(cmd::shown_name(None, "x"), "/tag use x");
    assert_eq!(cmd::shown_name(Some("?"), "x"), "?x");
    assert_eq!(cmd::neutralise_fences("a```b"), "a`\u{200b}`\u{200b}`b");
    assert_eq!(cmd::cooldown_text(0), "None");
    assert_eq!(cmd::cooldown_text(30), "30s per user");
    assert_eq!(cmd::cooldown_patch("NONE", Some("5m")).map(|p| p.1), Ok(" (duration ignored)"));
    assert_eq!(cmd::cooldown_patch("USER", None).err(), Some(("Duration Needed", "Give a duration such as 30s, 10m, 2h or 1d.")));
    assert_eq!(cmd::cooldown_patch("USER", Some("soon")).err().map(|e| e.0), Some("Invalid Duration"));
    assert_eq!(cmd::cooldown_patch("USER", Some("400d")).err().map(|e| e.0), Some("Too Long"));
    assert_eq!(cmd::cooldown_patch("SERVER", Some("90s")).map(|p| p.0), Ok(vec![("cooldownScope", json!("SERVER")), ("cooldownSeconds", json!(90))]));

    // The pure mutators of the action and reply subcommands.
    let legacy = tag_row(json!({ "content": "x" }));
    let (patch, n) = cmd::append_action(&legacy, &TagAction::role(ActionKind::AddRole, ROLE_A)).expect("appended");
    assert_eq!(n, 2);
    assert_eq!(patch[0].1, json!([{ "type": "reply", "responses": ["x"], "embed": false, "dm": false }, { "type": "addRole", "roleId": ROLE_A }]));
    let full = tag_row(json!({ "actions": vec![json!({ "type": "addRole", "roleId": ROLE_A }); 5] }));
    assert_eq!(cmd::append_action(&full, &TagAction::reply(false, false, "y")).err().map(|e| e.code), Some("LIMIT"));
    assert_eq!(cmd::remove_action(&legacy, 1).err().map(|e| e.title), Some("Last Action".into()));
    assert_eq!(cmd::remove_action(&legacy, 3).err().map(|e| e.message), Some("This command has 1 action(s).".into()));
    let (_, at, count) = cmd::add_response(&legacy, None, "second").expect("added");
    assert_eq!((at, count), (1, 2));
    let role_only = tag_row(json!({ "actions": [{ "type": "addRole", "roleId": ROLE_A }] }));
    assert_eq!(cmd::add_response(&role_only, None, "z").err().map(|e| e.code), Some("NO_MESSAGE"));
    assert_eq!(cmd::remove_response(&legacy, 1, 1).err().map(|e| e.title), Some("Last Reply".into()));
    let two = tag_row(json!({ "actions": [{ "type": "reply", "responses": ["a", "b"] }] }));
    let (patch, left) = cmd::remove_response(&two, 1, 1).expect("removed");
    assert_eq!(left, 1);
    assert_eq!(patch[1], ("content", json!("b")), "the mirror columns follow");
    assert_eq!(cmd::edit_patch(&role_only, Some("t"), None, None, None).err().map(|e| e.code), Some("NO_MESSAGE"));
    let p = cmd::edit_patch(&legacy, Some("new"), Some(true), Some("-"), Some(false)).expect("patch");
    assert_eq!(p, vec![("content", json!("new")), ("useEmbed", json!(true)), ("description", Value::Null), ("deleteInvocation", json!(false))]);
    let p = cmd::apply_access_rule(&tag_row(json!({ "allowRoleIds": [ROLE_A] })), "deny", Some(ROLE_A), Some(CH)).expect("lists");
    assert_eq!(p, vec![("allowRoleIds", json!([])), ("denyRoleIds", json!([ROLE_A])), ("allowChannelIds", json!([])), ("denyChannelIds", json!([CH]))]);
    assert_eq!(cmd::action_option_problem("reply", None, false, false, false, false), Some(("Missing Text", "This action needs `text`.")));
    assert_eq!(cmd::action_option_problem("dm", Some("x"), false, false, false, true), Some(("Wrong Option", "DMs never ping roles.")));
    assert_eq!(cmd::action_option_problem("send", Some("x"), false, false, false, false), Some(("Missing Option", "This action needs `text` and `channel`.")));
    assert_eq!(cmd::action_option_problem("addRole", Some(""), false, true, false, false).map(|p| p.0), Some("Wrong Option"));
    assert_eq!(cmd::action_option_problem("addRole", None, false, true, false, false), None);
    let raw = cmd::raw_answer(&tag_row(json!({ "content": "a `b`" })));
    assert_eq!(raw.0, "**Raw text of `hi`:**\n```\na `b`\n```");
    assert!(raw.1.is_empty());
    let raw = cmd::raw_answer(&tag_row(json!({ "content": "```x```", "actions": [{ "type": "reply", "responses": ["```x```"] }] })));
    assert_eq!(raw.0, "**Raw text of `hi`** is attached (7 characters).\nEvery action and reply is in the attached JSON.");
    assert_eq!(raw.1.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), vec!["hi.txt", "hi-actions.json"]);
    assert_eq!(String::from_utf8_lossy(&raw.1[1].1), "[\n  {\n    \"type\": \"reply\",\n    \"responses\": [\n      \"```x```\"\n    ],\n    \"embed\": false,\n    \"dm\": false\n  }\n]");
    let line = cmd::list_line(&tag_row(json!({ "uses": 1, "useEmbed": true, "cooldownScope": "USER", "cooldownSeconds": 5, "allowRoleIds": [ROLE_A] })), Some("!"));
    assert_eq!(line, "`!hi` — Hello {user}! {1:friend} · 1 use 🖼️ ⏳ 🔒");
}

// ── 16. Command validation through the router (no database needed) ───────────

const CG: &str = "950000000000000001";
const ADMIN: &str = "600000000000000009";

async fn router_app(module_on: bool) -> TestApp {
    let t = TestApp::new().with_guilds([CG]);
    configure_count_uses(&t, false);
    t.install(registry());
    t.settings().update_settings(CG, SettingsPatch::new().set("customCommandsEnabled", module_on)).await.expect("memory store");
    t
}

fn admin(sub: &str) -> FakeInteraction {
    FakeInteraction::slash("tag").guild(CG).user(ADMIN).permissions(serenity::all::Permissions::MANAGE_GUILD).subcommand(sub)
}

async fn last_embed(t: &TestApp, i: serenity::all::Interaction) -> (Vec<&'static str>, String, String, bool) {
    t.responses.clear();
    bot_core::router::route_interaction(t, i).await;
    let calls = t.responses.calls();
    let last = calls.last().cloned();
    (
        t.responses.names(),
        last.as_ref().and_then(|c| c.embed_title().map(String::from)).unwrap_or_default(),
        last.as_ref().and_then(|c| c.embed_description().map(String::from)).unwrap_or_default(),
        calls.first().is_some_and(|c| c.ephemeral()),
    )
}

#[tokio::test]
async fn tag_create_edit_cooldown_and_permissions_validate_before_the_database() {
    let t = router_app(true).await;
    let s = option_type::STRING;
    let (names, title, text, eph) = last_embed(&t, admin("create").option("name", s, json!("c++")).option("content", s, json!("x")).build()).await;
    assert_eq!(names, vec!["deferReply", "editReply"]);
    assert!(eph, "management replies are ephemeral");
    assert_eq!((title.as_str(), text.as_str()), ("❌  Invalid Name", svc::INVALID_NAME_REASON));
    let (_, title, _, _) = last_embed(&t, admin("create").option("name", s, json!("ok")).option("content", s, json!("   ")).build()).await;
    assert_eq!(title, "❌  Empty");
    let (_, title, text, _) = last_embed(&t, admin("create").option("name", s, json!("ok")).option("content", s, json!("x".repeat(2001))).build()).await;
    assert_eq!(title, "❌  Too Long");
    assert!(text.starts_with("Plain-text replies can be at most 2,000 characters (this one is 2001)."));

    let (_, title, _, _) = last_embed(&t, admin("edit").option("name", s, json!("ok")).build()).await;
    assert_eq!(title, "❌  Nothing to Change");
    let (_, title, _, _) = last_embed(&t, admin("cooldown").option("name", s, json!("ok")).option("scope", s, json!("USER")).build()).await;
    assert_eq!(title, "❌  Duration Needed");
    let (_, title, _, _) = last_embed(&t, admin("permissions").option("name", s, json!("ok")).option("rule", s, json!("allow")).build()).await;
    assert_eq!(title, "❌  Nothing Selected");
    let (_, title, text, _) =
        last_embed(&t, admin("permissions").option("name", s, json!("ok")).option("rule", s, json!("deny")).option("role", option_type::ROLE, json!(CG)).build()).await;
    assert_eq!(title, "❌  Not Allowed");
    assert!(text.starts_with("@everyone cannot be used here."));
}

#[tokio::test]
async fn tag_action_add_refuses_options_that_do_not_belong() {
    let t = router_app(true).await;
    let s = option_type::STRING;
    let add = |kind: &str| {
        FakeInteraction::slash("tag")
            .guild(CG)
            .user(ADMIN)
            .permissions(serenity::all::Permissions::MANAGE_GUILD)
            .group("action", "add")
            .option("name", s, json!("hi"))
            .option("type", s, json!(kind))
    };
    let (_, title, _, _) = last_embed(&t, add("reply").build()).await;
    assert_eq!(title, "❌  Missing Text");
    let (_, title, text, _) = last_embed(&t, add("reply").option("text", s, json!("x")).option("role", option_type::ROLE, json!(ROLE_A)).build()).await;
    assert_eq!((title.as_str(), text.as_str()), ("❌  Wrong Option", "`role`/`channel` are not used by this action."));
    let (_, title, _, _) = last_embed(&t, add("addRole").option("text", s, json!("x")).option("role", option_type::ROLE, json!(ROLE_A)).build()).await;
    assert_eq!(title, "❌  Wrong Option");
    let (_, title, _, _) = last_embed(&t, add("send").option("text", s, json!("x")).build()).await;
    assert_eq!(title, "❌  Missing Option");
    let (_, title, text, _) = last_embed(&t, add("dm").option("text", s, json!("x")).option("ping_role", option_type::ROLE, json!(ROLE_A)).build()).await;
    assert_eq!((title.as_str(), text.as_str()), ("❌  Wrong Option", "DMs never ping roles."));
    let (_, title, text, _) = last_embed(&t, add("reply").option("text", s, json!("x")).option("ping_role", option_type::ROLE, json!(CG)).build()).await;
    assert_eq!((title.as_str(), text.as_str()), ("❌  Not Allowed", "@everyone can never be pinged by a custom command."));
    let (_, title, _, _) = last_embed(&t, add("addRole").option("role", option_type::ROLE, json!(ROLE_A)).build()).await;
    assert_eq!(title, "❌  Role Not Allowed", "offline the role cannot be checked, so it is refused");
}

#[tokio::test]
async fn tag_trigger_add_refuses_unsafe_invalid_and_empty_input() {
    let t = router_app(false).await;
    let s = option_type::STRING;
    let add = |pattern: &str, response: &str, mt: Option<&str>| {
        let mut f = FakeInteraction::slash("tag")
            .guild(CG)
            .user(ADMIN)
            .permissions(serenity::all::Permissions::MANAGE_GUILD)
            .group("trigger", "add")
            .option("pattern", s, json!(pattern))
            .option("response", s, json!(response));
        if let Some(m) = mt {
            f = f.option("match_type", s, json!(m));
        }
        f.build()
    };
    let (names, title, text, eph) = last_embed(&t, add("(a+)+", "x", Some("REGEX"))).await;
    assert_eq!(names, vec!["deferReply", "editReply"]);
    assert!(!eph, "trigger replies are public");
    assert_eq!(title, "❌  Invalid Pattern");
    assert!(text.starts_with("Nested repetition is not allowed"));
    let (_, _, text, _) = last_embed(&t, add("(a", "x", Some("REGEX"))).await;
    assert!(text.starts_with("Invalid regular expression:"), "{text}");
    let (_, title, _, _) = last_embed(&t, add("hello", "  ", None)).await;
    assert_eq!(title, "❌  Empty");
}

#[tokio::test]
async fn tag_trigger_enable_and_disable_write_the_settings() {
    let t = router_app(false).await;
    let trig = |sub: &str| FakeInteraction::slash("tag").guild(CG).user(ADMIN).permissions(serenity::all::Permissions::MANAGE_GUILD).group("trigger", sub).build();
    let (_, title, text, _) = last_embed(&t, trig("enable")).await;
    assert_eq!(title, "✅  Triggers On");
    assert_eq!(text, "Triggers now respond to matching messages. The **Custom Commands** module was off and is now on.");
    let s = t.settings().get_settings(CG).await.expect("row");
    assert!(s.custom_commands_enabled && s.auto_triggers_enabled);
    let (_, _, text, _) = last_embed(&t, trig("enable")).await;
    assert_eq!(text, "Triggers now respond to matching messages.");
    let (_, title, _, _) = last_embed(&t, trig("disable")).await;
    assert_eq!(title, "✅  Triggers Off");
    let s = t.settings().get_settings(CG).await.expect("row");
    assert!(s.custom_commands_enabled && !s.auto_triggers_enabled, "disable only pauses triggers");
}

#[tokio::test]
async fn tag_placeholders_is_an_ephemeral_reply_with_the_argument_tokens() {
    let t = router_app(true).await;
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::slash("tag").guild(CG).user(ADMIN).subcommand("placeholders").build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert!(calls[0].ephemeral());
    let e = &calls[0].body["embeds"][0];
    assert_eq!(e["title"], "ℹ️  Custom Command Variables");
    assert_eq!(e["fields"][0]["name"], "Arguments");
    assert!(e["fields"][0]["value"].as_str().unwrap_or("").contains("`{1:text}` first argument"));
    assert_eq!(e["footer"]["text"], "Run with !name arg1 arg2 or /tag use name:<name> args:arg1 arg2");
}

#[tokio::test]
async fn while_the_module_is_off_every_other_subcommand_refuses_before_any_defer() {
    let t = router_app(false).await;
    for sub in ["use", "list", "create", "info"] {
        let (names, title, text, eph) = last_embed(&t, admin(sub).option("name", option_type::STRING, json!("hi")).option("content", option_type::STRING, json!("x")).build()).await;
        assert_eq!(names, vec!["reply"], "{sub}");
        assert!(eph);
        assert_eq!(title, "⚠️  Module Disabled");
        assert_eq!(text, "The **Custom Commands** module is turned off on this server. An admin can enable it with `/module enable customcommands`.");
    }
    // The name autocomplete answers nothing.
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::autocomplete("tag").guild(CG).subcommand("use").focused("name", "h").build()).await;
    assert_eq!(t.responses.calls()[0].choices(), Vec::<Value>::new());
}

// ── 17. Database ──────────────────────────────────────────────────────────────

async fn db_app(guilds: &[&'static str]) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = match redis_url() {
        Some(r) => DbTestApp::with_redis(&url, &r).await,
        None => DbTestApp::new(&url).await,
    }
    .with_guilds(guilds.iter().copied());
    configure_count_uses(&t, false);
    t.reset_guilds(guilds).await;
    for g in guilds {
        t.settings().ensure_guild_settings(g).await.expect("settings row");
    }
    Some(t)
}

fn new_tag(g: &str, name: &str) -> NewTag {
    NewTag { guild_id: g.into(), name: name.into(), content: format!("Hello from {name} {{1:friend}}"), use_embed: false, description: None, created_by: U.into() }
}

fn new_trigger(g: &str, pattern: &str) -> NewTrigger {
    NewTrigger {
        guild_id: g.into(),
        trigger: pattern.into(),
        response: format!("re {pattern}"),
        match_type: "CONTAINS".into(),
        cooldown: 0,
        delete_message: false,
        case_sensitive: false,
        channel_ids: vec![],
        exempt_role_ids: vec![],
        created_by: U.into(),
    }
}

#[tokio::test]
async fn every_raw_sql_statement_executes_against_the_migrated_schema() {
    let Some(t) = db_app(&[]).await else { return };
    for (name, sql, param) in RAW_SQL {
        let mut tx = t.db().begin().await.expect("tx");
        sqlx::query(sql).bind(param).execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
    }
}

#[tokio::test]
async fn create_tag_row_cap_and_duplicates_and_trigger_cap() {
    const G: &str = "960000000000000001";
    let Some(t) = db_app(&[G]).await else { return };
    create_tag_row(&t, new_tag(G, "first")).await.expect("created");
    let dup = create_tag_row(&t, new_tag(G, "first")).await.expect_err("duplicate");
    assert!(is_unique_violation(&dup), "{dup}");
    // Fill to the cap in one statement, then one more is refused.
    let sql = r#"INSERT INTO "CustomTag" ("id","guildId","name","content","createdBy","updatedAt") SELECT 'fill' || g, $1, 'fill-' || g, 'x', 'u', (now() AT TIME ZONE 'UTC') FROM generate_series(1, 499) g"#;
    sqlx::query(sql).bind(G).execute(t.db()).await.expect("filled");
    let err = create_tag_row(&t, new_tag(G, "over")).await.expect_err("capped");
    let e = as_tag_error(&err).expect("a TagError");
    assert_eq!((e.code, e.title.as_str()), ("LIMIT", "Limit Reached"));
    assert_eq!(e.message, "This server already has 500 custom commands, the maximum. Delete one with `/tag delete` first.");

    let sql = r#"INSERT INTO "AutoTrigger" ("id","guildId","trigger","response","createdBy","updatedAt") SELECT 'tfill' || g, $1, 'p' || g, 'r', 'u', (now() AT TIME ZONE 'UTC') FROM generate_series(1, 100) g"#;
    sqlx::query(sql).bind(G).execute(t.db()).await.expect("filled");
    let err = create_trigger_row(&t, new_trigger(G, "over")).await.expect_err("capped");
    assert_eq!(as_tag_error(&err).map(|e| e.message.clone()), Some("This server already has 100 triggers, the maximum. Remove one with `/tag trigger remove`.".into()));
    t.reset_guilds(&[G]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_tag_optimistic_concurrency_retries_once_then_conflicts() {
    const G: &str = "960000000000000002";
    let Some(t) = db_app(&[G]).await else { return };
    let row = create_tag_row(&t, new_tag(G, "race")).await.expect("created");
    let url = database_url().unwrap_or_default();
    // Every call of the mutator changes the row behind its back first.
    let bump = move |n: usize| {
        let url = url.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(async {
                let pool = bot_db::sqlx::PgPool::connect(&url).await.expect("pool");
                let sql = r#"UPDATE "CustomTag" SET "description" = $1, "updatedAt" = "updatedAt" + interval '1 second' WHERE "name" = 'race' AND "guildId" = $2"#;
                sqlx::query(sql).bind(format!("bump {n}")).bind(G).execute(&pool).await.expect("bumped");
                pool.close().await;
            });
        })
        .join()
        .expect("joined");
    };
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = calls.clone();
    let b2 = bump.clone();
    let err = write_tag(&t, G, "race", move |_| {
        let n = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        b2(n);
        Ok(vec![("enabled", json!(false))])
    })
    .await
    .expect_err("conflict");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2, "two attempts");
    assert_eq!(as_tag_error(&err).map(|e| (e.code, e.message.clone())), Some(("CONFLICT", "Someone else changed this command. Run the command again.".into())));

    // One conflict, then the retry wins.
    let c3 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c4 = c3.clone();
    let updated = write_tag(&t, G, "race", move |_| {
        if c4.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            bump(9);
        }
        Ok(vec![("enabled", json!(false))])
    })
    .await
    .expect("retried");
    assert!(!updated.enabled);
    assert!(updated.updated_at > row.updated_at);
    assert_eq!(updated.description.as_deref(), Some("bump 9"), "the other write is kept");

    // An empty patch returns the row unchanged; a missing name is NOT_FOUND.
    let same = write_tag(&t, G, "race", |_| Ok(vec![])).await.expect("unchanged");
    assert_eq!(same.updated_at, updated.updated_at);
    let err = write_tag(&t, G, "Nope!", |_| Ok(vec![])).await.expect_err("missing");
    assert_eq!(as_tag_error(&err).map(|e| e.message.clone()), Some("No custom command named `nope` exists here.".into()));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn write_trigger_conflicts_when_the_row_changed_and_uses_are_clamped() {
    const G: &str = "960000000000000003";
    let Some(t) = db_app(&[G]).await else { return };
    let row = create_trigger_row(&t, new_trigger(G, "hello")).await.expect("created");
    let updated = write_trigger(&t, G, &row, vec![("enabled", json!(false))]).await.expect("written");
    assert!(!updated.enabled);
    let err = write_trigger(&t, G, &row, vec![("enabled", json!(true))]).await.expect_err("stale");
    assert_eq!(as_tag_error(&err).map(|e| e.code), Some("CONFLICT"));

    let tag = create_tag_row(&t, new_tag(G, "count")).await.expect("created");
    sqlx::query(r#"UPDATE "CustomTag" SET "uses" = 2147483646 WHERE "id" = $1"#).bind(&tag.id).execute(t.db()).await.expect("set");
    let before = find_tag_by_id(&t, &tag.id).await.expect("read").expect("row");
    for _ in 0..3 {
        sqlx::query(TAG_USES_SQL).bind(&tag.id).execute(t.db()).await.expect("incremented");
    }
    let after = find_tag_by_id(&t, &tag.id).await.expect("read").expect("row");
    assert_eq!(after.uses, i32::MAX, "clamped to int4");
    assert_eq!(after.updated_at, before.updated_at, "raw SQL leaves updatedAt alone");
    sqlx::query(TRIGGER_USES_SQL).bind(&row.id).execute(t.db()).await.expect("incremented");
    assert_eq!(find_trigger_by_id(&t, &row.id).await.expect("read").map(|r| r.uses), Some(1));
    // The fire-and-forget counter, and the JS one, on the same row.
    configure_count_uses(&t, true);
    sqlx::query(r#"UPDATE "CustomTag" SET "uses" = 5 WHERE "id" = $1"#).bind(&tag.id).execute(t.db()).await.expect("set");
    increment_tag_uses(&t, &tag.id);
    for _ in 0..50 {
        if find_tag_by_id(&t, &tag.id).await.ok().flatten().map(|r| r.uses) == Some(6) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let js = run_node(JS, &["uses", &tag.id]);
    assert_eq!(js["uses"], 7, "{js}");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn resolve_trigger_and_trigger_number_agree_with_the_js() {
    const G: &str = "960000000000000004";
    let Some(t) = db_app(&[G]).await else { return };
    let mut rows = Vec::new();
    for p in ["Hello", "a_c", "abc", "Dup", "dup", "100%"] {
        rows.push(create_trigger_row(&t, new_trigger(G, p)).await.expect("created"));
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    for r in &rows {
        let n = trigger_number(&t, G, r).await.expect("number");
        assert_eq!(json!(n), run_node(JS, &["number", G, &r.id]), "{}", r.trigger);
    }
    let inputs = ["#1", "3", " #6 ", "#7", "0", "hello", "HELLO", "a_c", "abc", "dup", "ABC", "100%", "1000%", "nope`x", "", "12345", &rows[2].id];
    for input in inputs {
        let js = run_node(JS, &["resolve", G, input]);
        match resolve_trigger(&t, G, input).await {
            Ok(row) => assert_eq!(js["id"], json!(row.id), "resolve {input:?}: {js}"),
            Err(e) => {
                let te = as_tag_error(&e).expect("a TagError");
                assert_eq!((json!(te.code), json!(te.message)), (js["code"].clone(), js["error"].clone()), "resolve {input:?}");
            }
        }
    }
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn the_autocomplete_queries_agree_with_prisma() {
    const G: &str = "960000000000000005";
    let Some(t) = db_app(&[G]).await else { return };
    for p in ["hello world", "a_c", "abc", "50% off", "back\\slash", "UPPER"] {
        create_trigger_row(&t, new_trigger(G, p)).await.expect("created");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    for n in ["rules", "rul_es", "faq", "a-b", "ab"] {
        create_tag_row(&t, new_tag(G, n)).await.expect("created");
    }
    sqlx::query(r#"UPDATE "CustomTag" SET "uses" = 3 WHERE "name" = 'faq' AND "guildId" = $1"#).bind(G).execute(t.db()).await.expect("uses");
    for q in ["", "  ", "o", "_", "%", "a_c", "\\", "re a", "UPP", "c"] {
        let ours: Vec<Value> = cmd::trigger_choices(&t, G, q).await.expect("choices").into_iter().map(|c| c.value).collect();
        assert_eq!(json!(ours), run_node(JS, &["trigger-search", G, q]), "trigger search {q:?}");
    }
    for q in ["", "R", "_", "rul_", "a-", "Ü", "es"] {
        let norm = normalize_tag_name(q);
        let ours: Vec<String> = cmd::name_rows(&t, G, &norm, false, 25).await.expect("rows").into_iter().map(|r| r.name).collect();
        assert_eq!(json!(ours), run_node(JS, &["name-search", G, q]), "name search {q:?}");
    }
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn the_trigger_cache_is_shared_with_the_js_in_both_directions() {
    const G: &str = "960000000000000006";
    let Some(t) = db_app(&[G]).await else { return };
    if redis_url().is_none() {
        return;
    }
    let mut regex = new_trigger(G, "^h(i|ey)$");
    regex.match_type = "REGEX".into();
    regex.channel_ids = vec![CH.into()];
    create_trigger_row(&t, regex).await.expect("created");
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    create_trigger_row(&t, new_trigger(G, "second")).await.expect("created");
    let key = trigger_cache_key(G);
    assert_eq!(key, format!("guild:{G}:triggers"));

    // JS loads from Postgres and writes the cache; Rust reads that value.
    t.cache().del_one(&key).await;
    let js = run_node(JS, &["trigger-set", G]);
    let cached = t.cache().get(&key).await.expect("the JS wrote the list");
    let parsed = parse_cached_projections(&cached).expect("the Rust side reads it");
    assert_eq!(parsed.len(), 2);
    assert_eq!(serde_json::to_value(&parsed).ok(), Some(cached.clone()), "written back JSON-equal");
    let set = get_trigger_set(&t, G).await.expect("loaded");
    assert!(set[0].re.is_some(), "compiled");
    assert_eq!(js[0]["compiled"], true);
    for (ours, theirs) in set.iter().zip(js.as_array().cloned().unwrap_or_default()) {
        let mut theirs = theirs;
        theirs.as_object_mut().map(|o| o.remove("compiled"));
        assert_eq!(serde_json::to_value(&ours.t).ok(), Some(theirs));
    }

    // Rust loads from Postgres and writes the cache; a fresh JS process reads it from Redis.
    invalidate_trigger_cache(&t, G).await;
    assert!(t.cache().get(&key).await.is_none(), "invalidated");
    let t2 = db_app(&[]).await.expect("app");
    let set = get_trigger_set(&t2, G).await.expect("loaded");
    assert_eq!(set.len(), 2);
    let raw = t2.cache().get(&key).await.expect("the Rust side wrote it");
    let js = run_node(JS, &["cache-get", &key]);
    assert_eq!(js, raw);
    // A legacy-shaped value (full rows without v) is a miss on both sides.
    run_node(JS, &["cache-set", &key, r#"[{"id":"x","trigger":"t"}]"#, "60"]);
    let legacy = t.cache().get(&key).await.expect("written");
    assert!(parse_cached_projections(&legacy).is_none());
    let t3 = db_app(&[]).await.expect("app");
    assert_eq!(get_trigger_set(&t3, G).await.map(|s| s.len()), Some(2), "falls through to Postgres");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn cooldown_keys_are_shared_with_the_js() {
    let Some(t) = db_app(&[]).await else { return };
    if redis_url().is_none() {
        return;
    }
    let key = "tag:cd:960000000000000007:shared:1";
    t.cache().del_one(key).await;
    let js = run_node(JS, &["acquire", key, "60"]);
    assert_eq!(js, json!({ "ok": true }));
    match acquire_cooldown(&t, key, 60).await {
        Cooldown::Until(until) => assert!(until > t.clock().now_ms() + 50_000),
        other => panic!("expected the JS cooldown to hold, got {other:?}"),
    }
    let key2 = "tag:cd:960000000000000007:shared:2";
    t.cache().del_one(key2).await;
    assert_eq!(acquire_cooldown(&t, key2, 60).await, Cooldown::Ok);
    let js = run_node(JS, &["acquire", key2, "60"]);
    assert_eq!(js["ok"], false, "{js}");
    let until = js["until"].as_i64().unwrap_or(0);
    let ours: i64 = t.cache().get(key2).await.and_then(|v| v.as_i64()).unwrap_or(-1);
    assert_eq!(until, ours, "the JS reads the Rust deadline");
    clear_tag_cooldowns(&t, "960000000000000007", "shared").await;
    assert!(t.cache().get(key).await.is_none() && t.cache().get(key2).await.is_none());
}

#[tokio::test]
async fn find_tag_normalises_and_has_tag_name_reads_enabled_names() {
    const G: &str = "960000000000000008";
    let Some(t) = db_app(&[G]).await else { return };
    create_tag_row(&t, new_tag(G, "server-rules")).await.expect("created");
    let off = create_tag_row(&t, new_tag(G, "off")).await.expect("created");
    write_tag(&t, G, "off", |_| Ok(vec![("enabled", json!(false))])).await.expect("disabled");
    assert_eq!(find_tag(&t, G, "Server Rules").await.expect("query").map(|r| r.name), Some("server-rules".into()));
    assert_eq!(find_tag(&t, G, "!!!").await.expect("query"), None);
    assert!(has_tag_name(&t, G, "server-rules").await);
    assert!(!has_tag_name(&t, G, "off").await, "disabled names are not in the index");
    assert_eq!(find_tag_row(&t, G, "off").await.map(|r| r.id), Some(off.id));
    t.reset_guilds(&[G]).await;
}

// ── 17b. Cross-language rows ──────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_read_each_others_custom_commands() {
    const G: &str = "960000000000000009";
    let Some(t) = db_app(&[G]).await else { return };
    // JS creates and writes actions; Rust reads and runs them.
    let created = run_node(JS, &["create-tag", &json!({ "guildId": G, "name": "js-made", "content": "hi", "useEmbed": false, "createdBy": U }).to_string()]);
    assert!(created.get("error").is_none(), "{created}");
    let patch = json!({
        "actions": [{ "type": "reply", "responses": ["Hi {1}"], "embed": true, "dm": false }, { "type": "send", "channelId": CH2, "responses": ["log"], "embed": false, "pingRoleIds": [ROLE_A] }],
        "content": "Hi {1}", "useEmbed": true, "cooldownScope": "USER", "cooldownSeconds": 30, "allowRoleIds": [ROLE_A]
    });
    let js_row = run_node(JS, &["write-tag", G, "js-made", &patch.to_string()]);
    let row = find_tag(&t, G, "js-made").await.expect("query").expect("row");
    assert_eq!(serde_json::to_value(&row).ok(), Some(js_row.clone()), "the row reads the same through Prisma and Rust");
    assert_eq!(build_usage(&row, Some("!"), false), "!js-made <1>");
    let io = FakeIo::default().with_channel(CH, true).with_channel(CH2, true).with_role(ROLE_A, None, true);
    let mut o = opts(Source::Prefix, Some(vec![ROLE_A.into()]));
    o.guild_id = G.into();
    o.args = vec!["Bob".into()];
    let res = run_tag(&t, &row, o, &io).await;
    assert!(res.success, "{res:?}");
    assert_eq!(io.calls("send")[0].1["embeds"][0]["description"], "Hi Bob");
    assert_eq!(io.calls("send")[1].1["allowedMentions"], json!({ "parse": ["users"], "roles": [ROLE_A] }));

    // Rust creates and writes; JS reads the same row.
    let ours = create_tag_row(&t, NewTag { description: Some("from rust".into()), ..new_tag(G, "rust-made") }).await.expect("created");
    let list = vec![TagAction::reply(true, false, "psst"), TagAction::role(ActionKind::ToggleRole, ROLE_B)];
    let mut p: Patch = vec![("actions", actions_to_json(&list)), ("deleteInvocation", json!(false))];
    p.extend(sync_legacy_columns(&list));
    let written = write_tag(&t, G, "rust-made", move |_| Ok(p.clone())).await.expect("written");
    assert!(written.updated_at >= ours.updated_at);
    let back = run_node(JS, &["find-tag", G, "Rust Made"]);
    assert_eq!(serde_json::to_value(&written).ok(), Some(back.clone()));
    assert_eq!(back["actions"], json!([{ "type": "reply", "responses": ["psst"], "embed": false, "dm": true }, { "type": "toggleRole", "roleId": ROLE_B }]));
    let js_view = js_pure(&[json!(["getEffectiveActions", back])]);
    assert_eq!(js_view[0], actions_to_json(&get_effective_actions(&written)));

    // The dashboard's form of createTagRow: any columns.
    let list = vec![TagAction::send(CH2, true, "posted")];
    let mut fields: Patch = vec![
        ("name", json!("dash-made")),
        ("createdBy", json!(U)),
        ("cooldownScope", json!("USER")),
        ("cooldownSeconds", json!(30)),
        ("denyChannelIds", json!([CH])),
        ("actions", actions_to_json(&list)),
    ];
    fields.extend(sync_legacy_columns(&list));
    let dash = create_tag_row_with(&t, G, fields).await.expect("created");
    let back = run_node(JS, &["find-tag", G, "dash-made"]);
    assert_eq!(serde_json::to_value(&dash).ok(), Some(back));
    let trig = create_trigger_row_with(&t, G, vec![("trigger", json!("x")), ("response", json!("y")), ("matchType", json!("EXACT")), ("createdBy", json!(U))])
        .await
        .expect("created");
    assert_eq!((trig.cooldown, trig.enabled, trig.match_type.as_str()), (0, true, "EXACT"), "database defaults");
    t.reset_guilds(&[G]).await;
}

// ── 17c. /tag end to end ──────────────────────────────────────────────────────

#[tokio::test]
async fn tag_commands_end_to_end_against_the_database() {
    use bot_core::testing::{FakeGuild, TEST_BOT_ID, offline_discord_as};
    use serenity::all::Permissions as P;
    const G: &str = "960000000000000010";
    const CHAN: &str = "960000000000000110";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    let bot_role = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::MANAGE_ROLES | P::MANAGE_MESSAGES;
    let guild = FakeGuild::new(G, "600000000000000099")
        .role("960000000000000201", 5, bot_role)
        .member(TEST_BOT_ID, &["960000000000000201"])
        .role("960000000000000202", 4, P::MANAGE_GUILD)
        .member(ADMIN, &["960000000000000202"])
        .role("960000000000000203", 1, P::empty())
        .channel(CHAN, 0, "general", &[]);
    t.set_discord(offline_discord_as(&[guild.json()]));
    t.settings().update_settings(G, SettingsPatch::new().set("customCommandsEnabled", true).set("customPrefix", "?")).await.expect("settings");
    let s = option_type::STRING;
    let i = option_type::INTEGER;
    let a = |sub: &str| FakeInteraction::slash("tag").guild(G).channel(CHAN).user(ADMIN).roles(&["960000000000000202"]).permissions(P::MANAGE_GUILD).subcommand(sub);
    let ag = |g: &str, sub: &str| FakeInteraction::slash("tag").guild(G).channel(CHAN).user(ADMIN).roles(&["960000000000000202"]).permissions(P::MANAGE_GUILD).group(g, sub);
    let run = |i: serenity::all::Interaction| {
        let t = &t;
        async move {
            t.responses.clear();
            bot_core::router::route_interaction(t, i).await;
            t.responses.calls()
        }
    };

    let calls = run(a("create").option("name", s, json!("Greet Me")).option("content", s, json!("Hi {1} {nope}")).option("description", s, json!("Says hi")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Custom Command Created"));
    assert_eq!(
        calls[1].embed_description(),
        Some("Run it with `?greet-me` or `/tag use name:greet-me`.\nUsage: `?greet-me <1>`\n⚠️ Unknown variables (shown as typed): `{nope}`\nMore: /tag action add · /tag permissions · /tag cooldown")
    );
    let calls = run(a("create").option("name", s, json!("greet-me")).option("content", s, json!("again")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("❌  Already Exists"));

    let calls = run(ag("action", "add").option("name", s, json!("greet-me")).option("type", s, json!("addRole")).option("role", option_type::ROLE, json!("960000000000000203")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Action Added"), "{calls:?}");
    assert_eq!(calls[1].embed_description(), Some("Action 2: ➕ Give <@&960000000000000203>"));
    let calls = run(ag("response", "add").option("name", s, json!("greet-me")).option("text", s, json!("Yo {1}")).build()).await;
    assert_eq!(calls[1].embed_description(), Some("Action 1 now has 2 replies; one is picked at random each time."));
    let calls = run(ag("action", "list").option("name", s, json!("greet-me")).build()).await;
    assert_eq!(calls[1].embed_description(), Some("`1.` 💬 Reply in channel · 2 replies\n↳ `1` Hi {1} {nope}\n↳ `2` Yo {1}\n`2.` ➕ Give <@&960000000000000203>"));
    let calls = run(a("cooldown").option("name", s, json!("greet-me")).option("scope", s, json!("SERVER")).option("duration", s, json!("90s")).build()).await;
    assert_eq!(calls[1].embed_description(), Some("`greet-me`: **Server-wide · 1 minute 30 seconds**"));
    let calls = run(a("permissions").option("name", s, json!("greet-me")).option("rule", s, json!("deny")).option("channel", option_type::CHANNEL, json!(CHAN)).build()).await;
    let fields = &calls[1].body["embeds"][0]["fields"];
    assert_eq!(fields[3]["value"], format!("<#{CHAN}>"));
    assert_eq!(fields[0]["value"], "Everyone");

    let calls = run(a("info").option("name", s, json!("greet-me")).build()).await;
    let e = &calls[1].body["embeds"][0];
    assert_eq!(e["title"], "ℹ️  🏷️ ?greet-me");
    assert_eq!(e["description"], "Says hi");
    assert_eq!(e["fields"][0]["value"], "`?greet-me <1>`\n`/tag use name:greet-me args:<1>`");
    assert_eq!(e["fields"][6]["value"], format!("Denied channels: <#{CHAN}>"));

    let calls = run(a("list").build()).await;
    assert_eq!(calls[1].method, "editReply");
    assert!(calls[1].embed_description().unwrap_or("").starts_with("`?greet-me` — Says hi · 0 uses ⏳ 🔒 ⚙️×2"), "{calls:?}");
    let calls = run(a("raw").option("name", s, json!("greet-me")).build()).await;
    assert!(calls[1].content().unwrap_or("").starts_with("**Raw text of `greet-me`:**\n```\nHi {1} {nope}\n```"));

    // Members: /tag use in a denied channel is refused; the name autocomplete hides nothing they may run.
    let member = |sub: &str| FakeInteraction::slash("tag").guild(G).channel(CHAN).user("600000000000000050").subcommand(sub);
    let calls = run(member("use").option("name", s, json!("greet-me")).option("args", s, json!("Bob")).build()).await;
    assert_eq!(calls[0].embed_title(), Some("❌  Not Allowed"));
    assert!(calls[0].ephemeral());
    let calls = run(a("permissions").option("name", s, json!("greet-me")).option("rule", s, json!("clear")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Permissions Updated"));
    let calls = run(member("use").option("name", s, json!("greet-me")).build()).await;
    assert_eq!(calls[0].embed_title(), Some("⚠️  Missing Arguments"));
    assert_eq!(calls[0].embed_description(), Some("Usage: `/tag use name:greet-me args:<1>`\nSays hi"));
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::autocomplete("tag").guild(G).user("600000000000000050").subcommand("use").focused("name", "GREET").build()).await;
    assert_eq!(t.responses.calls()[0].choices(), vec![json!({ "name": "greet-me — Says hi", "value": "greet-me" })]);
    t.responses.clear();
    bot_core::router::route_interaction(
        &t,
        FakeInteraction::autocomplete("tag")
            .guild(G)
            .user(ADMIN)
            .roles(&["960000000000000202"])
            .group("response", "remove")
            .option("name", s, json!("greet-me"))
            .option("action", i, json!(1))
            .focused("reply", "")
            .build(),
    )
    .await;
    assert_eq!(t.responses.calls()[0].choices(), vec![json!({ "name": "1. Hi {1} {nope}", "value": 1 }), json!({ "name": "2. Yo {1}", "value": 2 })]);

    let calls = run(ag("response", "remove").option("name", s, json!("greet-me")).option("action", i, json!(1)).option("reply", i, json!(1)).build()).await;
    assert_eq!(calls[1].embed_description(), Some("Removed reply 1 of action 1; 1 left."));
    let calls = run(ag("action", "remove").option("name", s, json!("greet-me")).option("action", i, json!(2)).build()).await;
    assert_eq!(calls[1].embed_description(), Some("Removed: ➕ Give <@&960000000000000203>"));
    let calls = run(a("toggle").option("name", s, json!("greet-me")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Custom Command Disabled"));
    let calls = run(a("edit").option("name", s, json!("greet-me")).option("content", s, json!("New {1}")).option("delete_usage", option_type::BOOLEAN, json!(false)).build()).await;
    assert_eq!(calls[1].embed_description(), Some("`greet-me`\nReply text updated.\nDelete usage: **off**."));
    let back = run_node(JS, &["find-tag", G, "greet-me"]);
    assert_eq!(back["content"], "New {1}");
    assert_eq!(back["actions"][0]["responses"], json!(["New {1}"]));
    assert_eq!(back["enabled"], false);
    let calls = run(a("delete").option("name", s, json!("greet-me")).build()).await;
    assert_eq!(calls[1].embed_description(), Some("`greet-me` (0 uses) was deleted."));
    assert_eq!(run_node(JS, &["find-tag", G, "greet-me"]), Value::Null);

    // Triggers.
    let calls = run(ag("trigger", "add").option("pattern", s, json!("hello")).option("response", s, json!("Hi {user}")).option("delete_message", option_type::BOOLEAN, json!(true)).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Trigger Added"));
    let desc = calls[1].embed_description().unwrap_or("").to_string();
    assert!(desc.starts_with("**#1** · ID `"), "{desc}");
    assert!(desc.ends_with("**Pattern:** `hello`\n**Match:** Contains\n**Cooldown:** None\n**Delete message:** Yes\n**Channels:** All channels"), "{desc}");
    let calls = run(ag("trigger", "edit").option("trigger", s, json!("#1")).option("channel", option_type::CHANNEL, json!(CHAN)).option("cooldown", i, json!(5)).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Trigger Updated"), "{calls:?}");
    assert!(calls[1].embed_description().unwrap_or("").contains(&format!("**Cooldown:** 5s per user\n**Channels:** <#{CHAN}>\n**Exempt roles:** None")));
    let calls = run(ag("trigger", "toggle").option("trigger", s, json!("HELLO")).build()).await;
    assert_eq!(calls[1].embed_description(), Some("#1 `hello` is now **paused**."));
    let calls = run(ag("trigger", "list").build()).await;
    let d = calls[1].embed_description().unwrap_or("").to_string();
    assert!(d.starts_with("`#1` ⏸️ **[Contains]** `hello`\n↳ Hi {user} · 0 fires · ID `"), "{d}");
    let calls = run(ag("trigger", "info").option("trigger", s, json!("1")).build()).await;
    let e = &calls[1].body["embeds"][0];
    assert_eq!(e["title"], "ℹ️  Trigger #1");
    assert_eq!(e["fields"][8]["value"], "```\nhello\n```");
    let calls = run(ag("trigger", "remove").option("trigger", s, json!("#9")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("❌  Not Found"));
    assert_eq!(calls[1].embed_description(), Some("No trigger matches `#9`. Use /tag trigger list to see numbers and IDs."));
    let calls = run(ag("trigger", "remove").option("trigger", s, json!("#1")).build()).await;
    assert_eq!(calls[1].embed_description(), Some("#1 `hello` was deleted (0 fires)."));
    t.reset_guilds(&[G]).await;
}

/// The JS checked the option's resolved channel; a channel the cache does not hold yet (a new one, a thread) is
/// fetched over REST instead of being refused as "Channel not found".
#[tokio::test]
async fn tag_channel_options_fetch_an_uncached_channel_over_rest() {
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use serenity::all::Permissions as P;
    const G: &str = "960000000000000020";
    const CHAN: &str = "960000000000000120";
    const UNCACHED: &str = "960000000000000121";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    let bot_role = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS;
    let base = FakeGuild::new(G, "600000000000000099")
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role("960000000000000211", 5, bot_role)
        .member(TEST_BOT_ID, &["960000000000000211"])
        .role("960000000000000212", 4, P::MANAGE_GUILD)
        .member(ADMIN, &["960000000000000212"])
        .channel(CHAN, 0, "general", &[]);
    const UNCACHED2: &str = "960000000000000122";
    let with_new = base.clone().channel(UNCACHED, 0, "brand-new", &[]).channel(UNCACHED2, 0, "brand-new-2", &[]);
    let rest = FakeRest::new();
    for id in [UNCACHED, UNCACHED2] {
        let mut chan_json = with_new.json()["channels"].as_array().and_then(|a| a.iter().find(|c| c["id"] == id).cloned()).expect("channel json");
        chan_json["guild_id"] = json!(G);
        rest.on("GET", &format!("/channels/{id}"), 200, chan_json);
    }
    t.set_discord(rest.discord(&[base.json()]));
    t.settings().update_settings(G, SettingsPatch::new().set("customCommandsEnabled", true)).await.expect("settings");
    let s = option_type::STRING;
    let ag = |g: &str, sub: &str| FakeInteraction::slash("tag").guild(G).channel(CHAN).user(ADMIN).roles(&["960000000000000212"]).permissions(P::MANAGE_GUILD).group(g, sub);
    let a = |sub: &str| FakeInteraction::slash("tag").guild(G).channel(CHAN).user(ADMIN).roles(&["960000000000000212"]).permissions(P::MANAGE_GUILD).subcommand(sub);
    let run = |i: serenity::all::Interaction| {
        let t = &t;
        async move {
            t.responses.clear();
            bot_core::router::route_interaction(t, i).await;
            t.responses.calls()
        }
    };

    let calls = run(a("create").option("name", s, json!("post")).option("content", s, json!("Hi")).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Custom Command Created"));
    let calls = run(ag("action", "add").option("name", s, json!("post")).option("type", s, json!("send")).option("text", s, json!("News")).option("channel", option_type::CHANNEL, json!(UNCACHED)).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Action Added"), "{calls:?}");
    assert_eq!(calls[1].embed_description(), Some(format!("Action 2: 📤 Send to <#{UNCACHED}> · 1 reply").as_str()));
    assert!(!rest.calls_to("GET", &format!("/channels/{UNCACHED}")).is_empty(), "the channel was fetched");

    let calls = run(ag("trigger", "add").option("pattern", s, json!("hello")).option("response", s, json!("Hi")).option("channel", option_type::CHANNEL, json!(UNCACHED2)).build()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Trigger Added"), "{calls:?}");
    assert!(!rest.calls_to("GET", &format!("/channels/{UNCACHED2}")).is_empty(), "the channel was fetched");
    t.reset_guilds(&[G]).await;
}
