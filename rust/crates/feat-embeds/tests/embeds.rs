//! Port of `test/embeds.test.js`, plus the golden, database and cross-language checks every feature crate runs.
//!
//! Offline: names, the pure template mutators, URL and emoji checks, role-ping policy, import parsing, the Welcome
//! Channel presets, the hook filter, the plugin declaration and the `/embed` payload, all against JS-produced
//! fixtures (`rust/fixtures/golden/feat-embeds/pure.json`, from `rust/fixtures/gen/feat-embeds-golden.mjs`).
//! With `TEST_DATABASE_URL` (and `TEST_REDIS_URL`): the service flows (create, mutate, publish, update, delete, the
//! form, welcome-channel), every subcommand through the router against a fake Discord, and both directions of the JS
//! round trip (`rust/fixtures/gen/feat-embeds-js.mjs`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use bot_core::command::option_type;
use bot_core::custom_embed::{empty_template, normalize_template, template_modal_json};
use bot_core::interaction_helpers::{Payload, cached_channel, cached_guild};
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, load_golden, offline_discord_as, redis_url, run_node};
use feat_embeds::embed_service::{
    self as svc, Change, DiscordFailure, EditCode, EmbedDiscord, EmbedEditError, FieldEdit, MentionMode, MentionPolicy, PostedMessage, PublishOutcome,
    UpdateOutcome, WelcomeArgs, WelcomeChannelSpec, WelcomeOutcome,
};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Member, MessageId, Permissions as P, UserId};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_embeds::register(&mut reg);
    reg
}

fn settings_row(g: &str) -> bot_db::GuildSettings {
    let mut fields = serde_json::Map::new();
    fields.insert("guildId".into(), json!(g));
    let row = bot_core::settings::default_row_json(&bot_db::GUILD_SETTINGS_INFO, &fields);
    serde_json::from_value(Value::Object(row)).expect("settings row")
}

/// An `allowed_mentions` body as the JS wrote it: serenity always sends `users: []` (and `roles: []`), which Discord
/// reads the same as leaving them out.
fn mentions(v: &Value) -> Value {
    let mut m = v.as_object().cloned().unwrap_or_default();
    if m.get("users").and_then(Value::as_array).is_some_and(Vec::is_empty) {
        m.shift_remove("users");
    }
    if m.get("parse").and_then(Value::as_array).is_some_and(Vec::is_empty) && m.get("roles").and_then(Value::as_array).is_some_and(Vec::is_empty) {
        m.shift_remove("roles");
    }
    Value::Object(m)
}

fn pure() -> Value {
    load_golden("feat-embeds/pure.json")
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

// ── 1. Declarations ───────────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["embed"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "embeds");
}

fn char_count(node: &Value) -> usize {
    let len = |v: &Value| v.as_str().map(|s| s.encode_utf16().count()).unwrap_or(0);
    let mut n = len(&node["name"]) + len(&node["description"]);
    for o in node["options"].as_array().into_iter().flatten() {
        n += char_count(o);
    }
    n
}

fn walk(node: &Value, path: &str) {
    let d = node["description"].as_str().unwrap_or("");
    assert!(!d.is_empty() && d.encode_utf16().count() <= 100, "{path} description");
    assert!(node.get("choices").is_none(), "{path} must not use choices");
    assert!(node["options"].as_array().map(Vec::len).unwrap_or(0) <= 25);
    for o in node["options"].as_array().into_iter().flatten() {
        walk(o, &format!("{path} {}", o["name"].as_str().unwrap_or("")));
    }
}

#[test]
fn payload_fits_discord_limits_and_has_the_planned_shape() {
    let def = feat_embeds::commands::embed::command();
    let json = def.payload();
    assert_eq!(json["name"], "embed");
    assert!(char_count(&json) <= 2900, "payload is {} characters", char_count(&json));
    let opts = json["options"].as_array().cloned().unwrap_or_default();
    assert_eq!(opts.len(), 17);
    let named = |n: &str| opts.iter().find(|o| o["name"] == n).cloned().unwrap_or(Value::Null);
    assert_eq!(named("field")["options"].as_array().map(Vec::len), Some(4));
    assert_eq!(named("button")["options"].as_array().map(Vec::len), Some(2));
    walk(&json, "/embed");
    assert_eq!(json["default_member_permissions"], P::MANAGE_GUILD.bits().to_string());
    assert_eq!(json["contexts"], json!([0]));
    let meta = def.metadata();
    assert_eq!(meta["module"], "embeds");
    assert_eq!(meta["gated"], true);
    assert_eq!(meta["permissionLevel"], 2);
    assert_eq!(meta["hasAutocomplete"], true);
}

#[test]
fn plugin_declaration_and_custom_ids() {
    let reg = registry();
    let p = reg.plugin_named("embeds").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Embeds));
    assert_eq!(p.hooks.len(), 1);
    assert_eq!(p.hooks[0].event().as_str(), "messageDelete");
    assert_eq!(p.hooks[0].id(), "embeds");
    assert!(p.hooks[0].always());
    assert_eq!(p.components.len(), 1);
    assert_eq!(p.components[0].prefix, "embed-modal");
    assert!(bot_config::hook_index("messageDelete", "embeds").is_some());
    let golden = pure();
    let ids = &golden["misc"]["customIds"];
    assert_eq!(bot_config::custom_id!("embed-modal", "new", "rules").ok().as_deref(), ids[0].as_str());
    assert_eq!(bot_config::custom_id!("embed-modal", "new", "a".repeat(32)).ok().as_deref(), ids[1].as_str());
    assert_eq!(bot_config::custom_id!("embed-modal", "cm1234567890abcdefghijklm").ok().as_deref(), ids[2].as_str());
    assert!(ids[1].as_str().map(str::len).unwrap_or(999) <= 100);
    let parsed = bot_config::parse_custom_id("embed-modal:new:rules");
    assert_eq!(parsed.params, vec!["new", "rules"]);
}

#[test]
fn no_literal_custom_ids_or_console_in_the_crate() {
    for f in ["src/embed_service.rs", "src/commands/embed.rs", "src/plugin.rs"] {
        let text = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f)).unwrap_or_default();
        assert!(!text.contains("println!"), "{f}: println");
        assert!(!text.contains("\"embed-modal:"), "{f}: literal customId");
    }
}

// ── 2. Names, URLs, emoji (against the JS) ────────────────────────────────────

#[test]
fn normalize_embed_name_matches_the_js() {
    let g = pure();
    for case in g["names"].as_array().into_iter().flatten() {
        let input = s(&case[0]);
        assert_eq!(svc::normalize_embed_name(input).map(Value::from).unwrap_or(Value::Null), case[1], "{input:?}");
    }
    // The verified cases of the JS test.
    assert_eq!(svc::normalize_embed_name("My Rules!").as_deref(), Some("my-rules"));
    assert_eq!(svc::normalize_embed_name("  Welcome  Greeting ").as_deref(), Some("welcome-greeting"));
    assert_eq!(svc::normalize_embed_name("Ünïcode").as_deref(), Some("unicode"));
    assert_eq!(svc::normalize_embed_name("--x--").as_deref(), Some("x"));
    assert_eq!(svc::normalize_embed_name("🎉"), None);
    assert_eq!(svc::normalize_embed_name(&"a".repeat(40)), Some("a".repeat(32)));
    for n in svc::PRESET_NAMES {
        assert_eq!(svc::normalize_embed_name(n).as_deref(), Some(n));
    }
}

#[test]
fn is_clear_word() {
    for w in ["none", "CLEAR", " remove ", "-"] {
        assert!(svc::is_clear_word(w), "{w}");
    }
    for w in ["nope", ""] {
        assert!(!svc::is_clear_word(w), "{w}");
    }
}

fn outcome(r: Result<String, EmbedEditError>) -> Value {
    match r {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "err": e.message, "code": e.code.as_str() }),
    }
}

#[test]
fn url_checks_match_the_js() {
    let g = pure();
    for case in g["imageUrls"].as_array().into_iter().flatten() {
        assert_eq!(outcome(svc::check_image_url(s(&case[0]))), case[1], "image {:?}", case[0]);
    }
    for case in g["linkUrls"].as_array().into_iter().flatten() {
        assert_eq!(outcome(svc::check_link_url(s(&case[0]), 2048)), case[1], "link {:?}", case[0]);
        assert_eq!(outcome(svc::check_link_url(s(&case[0]), 512)), case[2], "link512 {:?}", case[0]);
    }
}

#[test]
fn emoji_syntax_matches_the_js() {
    let g = pure();
    for case in g["emoji"].as_array().into_iter().flatten() {
        assert_eq!(Value::Bool(svc::is_emoji_syntax(s(&case[0]))), case[1], "{:?}", case[0]);
    }
    for case in g["buttonEmoji"].as_array().into_iter().flatten() {
        assert_eq!(outcome(svc::parse_button_emoji(s(&case[0]), |_| true)), case[1], "{:?}", case[0]);
    }
    let e = svc::parse_button_emoji("<:x:123456789012345678>", |_| false).err().map(|e| e.message).unwrap_or_default();
    assert!(e.contains("servers I'm in"));
    assert_eq!(svc::parse_button_emoji("<:x:123456789012345678>", |id| id == "123456789012345678").ok().as_deref(), Some("<:x:123456789012345678>"));
}

#[test]
fn validate_for_save_matches_the_js() {
    let g = pure();
    for case in g["validate"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::validate_for_save(&case[0])), case[1], "{}", case[0]);
    }
    let long = json!({ "content": "hi", "buttons": [{ "label": "L", "url": format!("https://a.b/{}", "a".repeat(510)) }] });
    let errors = svc::validate_for_save(&long);
    let mut unique = errors.clone();
    unique.dedup();
    assert_eq!(unique.len(), errors.len(), "errors are deduplicated");
}

// ── 3. Mutators ───────────────────────────────────────────────────────────────

fn change(args: &Value, key: &str) -> Change<String> {
    match args.get(key) {
        None => Change::Keep,
        Some(Value::Null) => Change::Clear,
        Some(v) => Change::Set(s(v).to_string()),
    }
}

fn opt_i(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn replay(fn_name: &str, tpl: &Value, args: &[Value]) -> Result<Value, EmbedEditError> {
    let a0 = args.first().cloned().unwrap_or(Value::Null);
    let a1 = args.get(1).cloned().unwrap_or(Value::Null);
    match fn_name {
        "addField" => svc::add_field(tpl, s(&a0["title"]), s(&a0["value"]), a0["inline"].as_bool().unwrap_or(false), opt_i(&a0, "position")),
        "editField" => {
            let edit = FieldEdit {
                title: a1.get("title").map(|v| s(v).to_string()),
                value: a1.get("value").map(|v| s(v).to_string()),
                inline: a1.get("inline").and_then(Value::as_bool),
                position: opt_i(&a1, "position"),
            };
            svc::edit_field(tpl, a0.as_i64().unwrap_or(0), &edit)
        }
        "removeField" => svc::remove_field(tpl, a0.as_i64().unwrap_or(0)),
        "addButton" => svc::add_button(tpl, s(&a0["label"]), s(&a0["url"]), a0.get("emoji").and_then(Value::as_str), opt_i(&a0, "position")),
        "removeButton" => svc::remove_button(tpl, a0.as_i64().unwrap_or(0)),
        "setAuthor" => svc::set_author(tpl, a0.get("text").and_then(Value::as_str), &change(&a0, "iconUrl"), &change(&a0, "url")),
        "setFooter" => svc::set_footer(tpl, a0.get("text").and_then(Value::as_str), &change(&a0, "iconUrl"), a0.get("timestamp").and_then(Value::as_bool)),
        "setImages" => Ok(svc::set_images(tpl, &change(&a0, "image"), &change(&a0, "thumbnail"))),
        "setColor" => Ok(svc::set_color(tpl, a0.as_u64().map(|c| c as u32))),
        "setTitleUrl" => Ok(svc::set_title_url(tpl, a0.as_str())),
        other => panic!("unknown mutator {other}"),
    }
}

#[test]
fn mutators_match_the_js() {
    let g = pure();
    for case in g["mutators"].as_array().into_iter().flatten() {
        let name = s(&case["fn"]);
        let args = case["args"].as_array().cloned().unwrap_or_default();
        let got = replay(name, &case["tpl"], &args);
        match (&got, case.get("ok")) {
            (Ok(t), Some(expected)) => {
                assert_eq!(t, expected, "{name} {args:?}");
                assert_eq!(normalize_template(t), case["normalized"], "{name} {args:?} normalized");
            }
            (Err(e), None) => assert_eq!(json!(e.message), case["err"], "{name} {args:?}"),
            _ => panic!("{name} {args:?}: Rust {got:?}, JS {case}"),
        }
    }
}

#[test]
fn mutators_never_change_their_argument_and_cap_at_25() {
    let base = empty_template();
    let t = svc::add_field(&base, "A", "B", false, None).expect("added");
    assert_eq!(t["embed"]["fields"], json!([{ "name": "A", "value": "B", "inline": false }]));
    assert_eq!(base, empty_template());
    let mut full = empty_template();
    for i in 0..25 {
        full = svc::add_field(&full, &format!("T{i}"), "v", false, None).expect("added");
    }
    assert!(svc::add_field(&full, "x", "y", false, None).is_err());
    let mut buttons = empty_template();
    for i in 0..25 {
        buttons = svc::add_button(&buttons, &format!("B{i}"), "https://a.b", None, None).expect("added");
    }
    let e = svc::add_button(&buttons, "x", "https://a.b", None, None).err().map(|e| e.message).unwrap_or_default();
    assert!(e.contains("at most 25"), "{e}");
    let one = svc::add_field(&empty_template(), "only", "v", false, None).expect("added");
    assert!(!svc::validate_for_save(&svc::remove_field(&one, 1).expect("removed")).is_empty(), "an empty template is refused");
}

// ── 4. Import, presets, hints ─────────────────────────────────────────────────

#[test]
fn parse_import_payload_matches_the_js() {
    let g = pure();
    for case in g["imports"].as_array().into_iter().flatten() {
        let text = s(&case[0]);
        let r = svc::parse_import_payload(text);
        let expected = &case[1];
        assert_eq!(json!(r.errors), expected["errors"], "{text:.80}");
        assert_eq!(json!(r.warnings), expected["warnings"], "{text:.80}");
        assert_eq!(r.template.unwrap_or(Value::Null), expected["template"], "{text:.80}");
    }
}

#[test]
fn welcome_presets_match_the_js() {
    let g = pure();
    let check = |rust: Vec<(&str, Value)>, js: &Value| {
        let js = js.as_array().cloned().unwrap_or_default();
        assert_eq!(rust.len(), js.len());
        for ((name, tpl), j) in rust.iter().zip(js) {
            assert_eq!(*name, s(&j["name"]));
            assert_eq!(tpl, &j["template"], "{name}");
            assert!(svc::validate_for_save(tpl).is_empty(), "{name}");
        }
    };
    check(svc::build_welcome_presets(None, &[]), &g["presets"]["default"]);
    check(svc::build_welcome_presets(Some("https://discord.gg/abc"), &["1".into(), "2".into()]), &g["presets"]["invite"]);
    let many: Vec<String> = (0..200u64).map(|i| (100_000_000_000_000_000 + i).to_string()).collect();
    check(svc::build_welcome_presets(None, &many), &g["presets"]["many"]);
    let presets = svc::build_welcome_presets(None, &many);
    assert!(presets[3].1["embed"]["description"].as_str().map(|d| d.encode_utf16().count()).unwrap_or(9999) <= 4096);
    assert!(!json!(svc::build_welcome_presets(None, &[])).to_string().contains("{user"), "no {{user…}} tokens");
    assert!(!svc::build_welcome_presets(None, &[])[3].1.to_string().contains("Moderators"));
}

#[test]
fn hints_keys_and_mentions_match_the_js() {
    let g = pure();
    let m = &g["misc"];
    assert_eq!(svc::embed_cache_key("900000000000000001", "my-rules"), s(&m["cacheKey"]));
    let errors: Vec<String> = ["a", "b", "a"].iter().map(|s| s.to_string()).chain((0..12).map(|i| format!("e{i}"))).collect();
    assert_eq!(svc::format_errors(&errors), s(&m["formatErrors"]));
    assert_eq!(svc::publish_hint(None, None, "a"), s(&m["publishHint"][0]));
    assert_eq!(svc::publish_hint(Some("5"), Some("6"), "rules"), s(&m["publishHint"][1]));
    assert_eq!(svc::publish_hint(Some("5"), None, "r"), s(&m["publishHint"][2]));
    for case in m["roleMentions"].as_array().into_iter().flatten().take(3) {
        assert_eq!(json!(svc::extract_role_mention_ids(Some(s(&case[0])))), case[1]);
    }
    let many: String = (0..120u64).map(|i| format!("<@&{}>", 100_000_000_000_000_000 + i)).collect::<Vec<_>>().join(" ");
    assert_eq!(json!(svc::extract_role_mention_ids(Some(&many)).len()), m["roleMentions"][3][1]);
    let colors: Vec<u32> = [Some(0xff0000), None, Some(-1), Some(0x100_0000), Some(0)].into_iter().map(svc::resolve_default_color_of).collect();
    assert_eq!(json!(colors), m["defaultColor"]);
    assert_eq!(json!(svc::mention_policy(Some("<@&123456789012345678>"), MentionMode::None, None, None)), m["mentionNone"]);
    assert_eq!(json!(svc::mention_policy(Some("<@&123456789012345678>"), MentionMode::Publish, None, None)), m["mentionNoChannel"]);
    let ctx = bot_util::placeholder_parser::RenderContext::default();
    let tpl = json!({ "content": "Hi {server}", "embed": { "title": "{nope} and {server.name}", "description": "{nope} {random:1:5}" }, "buttons": [{ "label": "{btn}", "url": "https://a.b" }] });
    assert_eq!(json!(svc::collect_unknown_tokens(&tpl, &ctx)), m["unknownNoCtx"][0]);
    let thirty = json!({ "content": (0..30).map(|i| format!("{{x{i}}}")).collect::<Vec<_>>().join(" ") });
    assert_eq!(json!(svc::collect_unknown_tokens(&thirty, &ctx).len()), m["unknownNoCtx"][1]);
    let note_tpl = json!({ "content": "a {nope} {zip}", "embed": { "fields": [{ "name": "{f}", "value": "{v}" }], "author": { "name": "{au}" }, "footer": { "text": "{fo}" } } });
    assert_eq!(svc::unknown_variables_note(&note_tpl, &ctx), s(&m["unknownNoCtx"][2]));
    // With a guild, {server…} is known and only {nope} is reported (the JS test).
    let with_guild = bot_util::placeholder_parser::RenderContext {
        guild: Some(bot_util::placeholder_parser::PlaceholderGuild { id: "1".into(), name: Some("Test Guild".into()), ..Default::default() }),
        ..Default::default()
    };
    let tpl = json!({ "content": "Hi {server}", "embed": { "title": "{nope} and {server.name}", "description": "{nope} {random:1:5}" }, "buttons": [] });
    assert_eq!(svc::collect_unknown_tokens(&tpl, &with_guild), vec!["{nope}"]);
    assert!(svc::unknown_variables_note(&tpl, &with_guild).contains("Unknown variables are shown as typed: `{nope}`"));
    assert_eq!(svc::unknown_variables_note(&json!({ "content": "ok {server}" }), &with_guild), "");
    // The forms and the draft file.
    let new_id = bot_config::custom_id!("embed-modal", "new", "rules").unwrap_or_default();
    assert_eq!(template_modal_json(&new_id, Some("New embed: rules"), &empty_template(), false).ok(), Some(m["modals"][0].clone()));
    let example = svc::parse_import_payload(s(&g["imports"][0][0])).template.unwrap_or(Value::Null);
    assert_eq!(template_modal_json("embed-modal:cid", Some("Edit embed: rules"), &example, true).ok(), Some(m["modals"][1].clone()));
    let draft = svc::draft_attachment(&json!({ "content": "keep me", "embed": { "title": "T" } }));
    assert_eq!(draft.filename, "embed-draft.json");
    assert_eq!(String::from_utf8_lossy(&draft.data), s(&m["draft"]));
}

#[test]
fn render_saved_embed_matches_the_js() {
    let g = pure();
    let expected = &g["misc"]["render"];
    let settings: bot_db::GuildSettings = settings_row("1");
    let settings = bot_db::GuildSettings { embed_color: Some(0x112233), ..settings };
    let tpl = json!({ "content": "Hello <@&123456789012345678>", "embed": { "title": "Welcome", "fields": [{ "name": "a", "value": "b" }] }, "buttons": [{ "label": "L", "url": "https://a.b", "emoji": "🎉" }] });
    let r = svc::render_saved_embed(&tpl, &Default::default(), Some(&settings), &MentionPolicy::none()).expect("renders");
    let out = r.to_json();
    assert_eq!(out["content"], expected["content"]);
    assert_eq!(out["embeds"], expected["embeds"]);
    assert_eq!(out["components"], expected["components"]);
    assert!(svc::render_saved_embed(&json!({ "content": null }), &Default::default(), None, &MentionPolicy::none()).is_none());
    let default = svc::render_saved_embed(&json!({ "embed": { "title": "x" } }), &Default::default(), None, &MentionPolicy::none()).expect("renders");
    assert_eq!(default.embeds[0]["color"], bot_config::colors::INFO);
}

// ── 5. Fakes: a guild in the serenity cache, a Discord that records ───────────

const OWNER: &str = "900000000000000001";
const ADMIN: &str = "900000000000000010";
const MEMBER: &str = "900000000000000011";
const MOD: &str = "900000000000000013";
const PINGER: &str = "900000000000000014";
const PING_ROLE: &str = "820000000000000009";
const READ_ONLY: &str = "900000000000000506";
const ROLE_NAMED: &str = "900000000000000507";
const BOT_ROLE: &str = "820000000000000001";
const ADMIN_ROLE: &str = "820000000000000002";
const MEMBER_ROLE: &str = "820000000000000003";
const R1: &str = "123456789012345678";
const R2: &str = "223456789012345678";
const CH: &str = "900000000000000500";
const NO_VIEW: &str = "900000000000000502";
const NO_BOT: &str = "900000000000000503";
const EMPTY_NAME: &str = "900000000000000504";
const OPEN: &str = "900000000000000801";
const MADE: &str = "900000000000000800";
const VOICE: &str = "900000000000000505";

fn bot_perms() -> P {
    P::VIEW_CHANNEL
        | P::SEND_MESSAGES
        | P::EMBED_LINKS
        | P::READ_MESSAGE_HISTORY
        | P::MANAGE_CHANNELS
        | P::CREATE_INSTANT_INVITE
        | P::SEND_MESSAGES_IN_THREADS
        | P::CREATE_PUBLIC_THREADS
        | P::CREATE_PRIVATE_THREADS
}

/// The test guild: the bot (all it needs), an admin (Manage Server), a plain member (View Channel), two roles to
/// ping (`R1` not mentionable, `R2` mentionable) and channels for each case.
fn fake_guild(g: &str, bot: P, with_made: bool) -> FakeGuild {
    let mut fg = FakeGuild::new(g, OWNER)
        .role(BOT_ROLE, 3, bot)
        .role(ADMIN_ROLE, 2, P::MANAGE_GUILD | P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role(MEMBER_ROLE, 1, P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role_with(R1, 1, P::empty(), |r| r["mentionable"] = json!(false))
        .role_with(R2, 1, P::empty(), |r| r["mentionable"] = json!(true))
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .member(ADMIN, &[ADMIN_ROLE])
        .member(MEMBER, &[MEMBER_ROLE])
        .member(MOD, &[ADMIN_ROLE, MEMBER_ROLE])
        .role(PING_ROLE, 1, P::MENTION_EVERYONE | P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .member(PINGER, &[PING_ROLE])
        .channel(READ_ONLY, 0, "read-only", &[(MEMBER_ROLE, false, P::empty(), P::SEND_MESSAGES)])
        .channel(ROLE_NAMED, 0, &format!("<@&{R1}>"), &[])
        .member(OWNER, &[])
        .channel(CH, 0, "general", &[])
        .channel(NO_VIEW, 0, "secret", &[(MEMBER_ROLE, false, P::empty(), P::VIEW_CHANNEL)])
        .channel(NO_BOT, 0, "nobot", &[(BOT_ROLE, false, P::empty(), P::SEND_MESSAGES | P::EMBED_LINKS)])
        .channel(EMPTY_NAME, 0, "", &[])
        .channel(OPEN, 0, "open", &[(g, false, P::SEND_MESSAGES, P::empty())])
        .channel(VOICE, 2, "voice", &[]);
    if with_made {
        fg = fg.channel(MADE, 0, "welcome", &[]);
    }
    fg
}

fn channel_json(g: &str, id: &str, name: &str) -> GuildChannel {
    serde_json::from_value(json!({
        "id": id, "type": 0, "guild_id": g, "name": name, "position": 0, "permission_overwrites": [],
        "nsfw": false, "parent_id": null, "topic": null, "last_message_id": null, "rate_limit_per_user": 0
    }))
    .expect("channel JSON")
}

/// A Discord that records what the bot sends, edits and deletes (the JS tests' fake channels).
#[derive(Default)]
struct FakeDiscord {
    app: OnceLock<App>,
    sent: Mutex<Vec<(String, Value)>>,
    messages: Mutex<HashMap<String, (String, String)>>,
    edits: Mutex<Vec<(String, Value)>>,
    deleted: Mutex<Vec<String>>,
    sends: AtomicUsize,
    /// The nth send (1-based) fails with this API code.
    fail_send: Mutex<Option<(usize, u64)>>,
    fail_edit: Mutex<Option<u64>>,
    created: Mutex<Option<GuildChannel>>,
    create_spec: Mutex<Option<WelcomeChannelSpec>>,
    /// Channels Discord has but the cache does not (`guild.channels.fetch` finds them over REST).
    remote: Mutex<Vec<GuildChannel>>,
}

impl FakeDiscord {
    fn install(app: &App) -> Arc<Self> {
        let f = Arc::new(Self::default());
        let _ = f.app.set(app.clone());
        svc::set_discord_ops(app, f.clone());
        f
    }

    fn sent(&self) -> Vec<(String, Value)> {
        self.sent.lock().map(|v| v.clone()).unwrap_or_default()
    }

    fn put_message(&self, id: &str, channel: &str, author: &str) {
        if let Ok(mut m) = self.messages.lock() {
            m.insert(id.to_string(), (channel.to_string(), author.to_string()));
        }
    }
}

fn payload_json(p: &Payload) -> Value {
    serde_json::to_value(p.to_message()).unwrap_or(Value::Null)
}

#[async_trait]
impl EmbedDiscord for FakeDiscord {
    async fn send(&self, guild_id: GuildId, channel_id: ChannelId, payload: &Payload) -> Result<PostedMessage, DiscordFailure> {
        let n = self.sends.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((at, code)) = *self.fail_send.lock().unwrap_or_else(|e| e.into_inner())
            && (at == 0 || at == n)
        {
            return Err(DiscordFailure::api(code, "Missing Permissions"));
        }
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).push((channel_id.to_string(), payload_json(payload)));
        let id = format!("80000000000{n:07}");
        self.put_message(&id, &channel_id.to_string(), TEST_BOT_ID);
        Ok(PostedMessage { id: id.clone(), channel_id: channel_id.to_string(), author_id: TEST_BOT_ID.into(), url: format!("https://discord.com/channels/{guild_id}/{channel_id}/{id}") })
    }

    async fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<GuildChannel, DiscordFailure> {
        self.app
            .get()
            .and_then(|a| cached_channel(a, guild_id, channel_id))
            .or_else(|| self.remote.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.id == channel_id).cloned())
            .ok_or_else(|| DiscordFailure::api(10003, "Unknown Channel"))
    }

    async fn fetch_message(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) -> Result<PostedMessage, DiscordFailure> {
        let m = self.messages.lock().unwrap_or_else(|e| e.into_inner()).get(&message_id.to_string()).cloned();
        match m {
            Some((c, author)) if c == channel_id.to_string() => Ok(PostedMessage {
                id: message_id.to_string(),
                channel_id: c,
                author_id: author,
                url: format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}"),
            }),
            _ => Err(DiscordFailure::api(10008, "Unknown Message")),
        }
    }

    async fn edit_message(&self, _channel_id: ChannelId, message_id: MessageId, payload: &Payload) -> Result<(), DiscordFailure> {
        if let Some(code) = *self.fail_edit.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(DiscordFailure::api(code, "Thread is archived"));
        }
        let edit = serde_json::to_value(payload.to_message_edit()).unwrap_or(Value::Null);
        self.edits.lock().unwrap_or_else(|e| e.into_inner()).push((message_id.to_string(), edit));
        Ok(())
    }

    async fn delete_message(&self, _channel_id: ChannelId, message_id: MessageId) -> Result<(), DiscordFailure> {
        let removed = self.messages.lock().unwrap_or_else(|e| e.into_inner()).remove(&message_id.to_string());
        if removed.is_none() {
            return Err(DiscordFailure::api(10008, "Unknown Message"));
        }
        self.deleted.lock().unwrap_or_else(|e| e.into_inner()).push(message_id.to_string());
        Ok(())
    }

    async fn create_invite(&self, _channel_id: ChannelId, _reason: &str) -> Result<String, DiscordFailure> {
        Ok("https://discord.gg/madeup".into())
    }

    async fn create_channel(&self, _guild_id: GuildId, spec: &WelcomeChannelSpec) -> Result<GuildChannel, DiscordFailure> {
        *self.create_spec.lock().unwrap_or_else(|e| e.into_inner()) = Some(spec.clone());
        self.created.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| DiscordFailure::other("must not be called"))
    }
}

fn member_of(app: &App, g: &str, id: &str) -> Member {
    let guild = cached_guild(app, GuildId::new(g.parse().unwrap_or(1))).expect("guild cached");
    guild.members.get(&UserId::new(id.parse().unwrap_or(1))).cloned().expect("member cached")
}

fn gid(g: &str) -> GuildId {
    GuildId::new(g.parse().unwrap_or(1))
}

fn cid(c: &str) -> ChannelId {
    ChannelId::new(c.parse().unwrap_or(1))
}

// ── 6. Offline: the hook filter, the mention policy, refusals before any lookup ──

#[tokio::test]
async fn is_embed_delete_candidate() {
    use bot_core::hooks::events::MessageDelete;
    let g = "955014000000000090";
    let t = TestApp::new().with_guilds([g]);
    let partial = MessageDelete { guild_id: gid(g), message_id: MessageId::new(5), channel_id: cid(CH), message: None, partial: true, channel: None };
    assert!(svc::is_embed_delete_candidate(&t, &partial), "uncached: ask the DB");
    let mine = bot_core::testing::fake_message("5", g, CH, TEST_BOT_ID, "x");
    let theirs = bot_core::testing::fake_message("6", g, CH, MEMBER, "x");
    let cached = |m| MessageDelete { guild_id: gid(g), message_id: MessageId::new(5), channel_id: cid(CH), message: Some(m), partial: false, channel: None };
    assert!(!svc::is_embed_delete_candidate(&t, &cached(mine.clone())), "no connection: the bot's id is unknown");
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[fake_guild(g, bot_perms(), false)]);
    assert!(svc::is_embed_delete_candidate(&t, &cached(mine)));
    assert!(!svc::is_embed_delete_candidate(&t, &cached(theirs)));
}

#[tokio::test]
async fn mention_policy_only_pingable_roles_get_through() {
    let g = "955014000000000091";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[fake_guild(g, bot_perms(), false)]);
    let guild = cached_guild(&t, gid(g)).expect("cached");
    let channel = cached_channel(&t, gid(g), cid(CH)).expect("cached");
    let text = format!("<@&{R1}> hi <@&{R1}> <@&{R2}> @everyone @here");
    let member = member_of(&t, g, MEMBER);
    let policy = svc::mention_policy(Some(&text), MentionMode::Publish, Some((&guild, &channel)), svc::member_key(Some(&member)));
    assert_eq!(json!(policy), json!({ "parse": ["users"], "roles": [R2] }));
    let r = svc::resolve_publish_mentions(&guild, Some(&text), &channel, svc::member_key(Some(&member)));
    assert_eq!(r.refused, vec![R1]);
    // Mention Everyone in the channel lets every role through.
    let fg = fake_guild(g, bot_perms(), false).role("820000000000000009", 1, P::MENTION_EVERYONE | P::VIEW_CHANNEL).member("900000000000000012", &["820000000000000009"]);
    let t2 = TestApp::new().with_guilds([g]).with_fake_guilds(&[fg]);
    let guild2 = cached_guild(&t2, gid(g)).expect("cached");
    let m2 = member_of(&t2, g, "900000000000000012");
    let open = svc::resolve_publish_mentions(&guild2, Some(&text), &channel, svc::member_key(Some(&m2)));
    let mut roles = open.allowed_mentions.roles.clone().unwrap_or_default();
    roles.sort();
    assert_eq!(roles, vec![R1, R2]);
    assert!(open.refused.is_empty());
    let none = svc::resolve_publish_mentions(&guild, Some("no roles"), &channel, None);
    assert_eq!((json!(none.allowed_mentions), none.refused.len()), (json!({ "parse": ["users"], "roles": [] }), 0));
}

fn admin_slash(g: &str) -> FakeInteraction {
    FakeInteraction::slash("embed").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD).channel(CH)
}

#[tokio::test]
async fn an_invalid_name_is_answered_before_any_lookup() {
    let g = "955014000000000092";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[fake_guild(g, bot_perms(), false)]);
    t.install(registry());
    route_interaction(&t, admin_slash(g).subcommand("create").option("name", option_type::STRING, json!("🎉")).build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert!(calls[0].ephemeral());
    assert_eq!(calls[0].embed_title(), Some("❌  Invalid Name"));

    t.responses.clear();
    route_interaction(&t, admin_slash(g).group("field", "add").option("name", option_type::STRING, json!("!!!")).build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Invalid Name"));

    t.responses.clear();
    let mut ac = FakeInteraction::autocomplete("embed").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD);
    ac = ac.group("field", "edit").option("name", option_type::STRING, json!("")).focused("index", "");
    route_interaction(&t, ac.build()).await;
    assert_eq!(t.responses.names(), vec!["respond"]);
    assert!(t.responses.calls()[0].choices().is_empty());
}

#[test]
fn reply_texts_and_choices() {
    use feat_embeds::commands::embed::{field_list_body, index_choices, roles_note, welcome_ready_text};
    let tpl = svc::add_field(&svc::add_field(&empty_template(), "F0b", "V", true, None).expect("ok"), "two", "x", false, None).expect("ok");
    assert_eq!(field_list_body(&tpl), "**1.** F0b · inline\nV\n\n**2.** two\nx");
    assert_eq!(field_list_body(&empty_template()), "This embed has no fields. Add one with `/embed field add`.");
    let c: Vec<Value> = index_choices(&tpl, Some("field"), "f0").into_iter().map(|c| c.value).collect();
    assert_eq!(c, vec![json!(1)]);
    let b = svc::add_button(&empty_template(), "Site", "https://example.com", Some("🌐"), None).expect("ok");
    let names: Vec<Option<String>> = index_choices(&b, Some("button"), "").into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec![Some("1. 🌐 Site — https://example.com".to_string())]);
    assert_eq!(roles_note(&[], "5"), "");
    assert!(roles_note(&[R1.to_string()], "5").contains(&format!("in <#5>): <@&{R1}>")));
    let text = welcome_ready_text("5", true, &["welcome-greeting".into()], &[], Some(&svc::InviteInfo { source: "created", url: None }), Some("Tip"));
    assert_eq!(
        text,
        "Posted the greeting, rules, links and staff embeds in <#5>. I created the channel; members can read but not write there.\nEdit them like any saved embed (for example `/embed edit name:welcome-rules` or `/embed button add name:welcome-links`), then run `/embed update` to refresh the posts.\nCreated: `welcome-greeting`.\nI created a permanent invite for the links embed.\nTip"
    );
}

// ── 7. Database ───────────────────────────────────────────────────────────────

const ADMIN_ID: &str = ADMIN;

struct Db {
    t: DbTestApp,
    g: &'static str,
}

impl Db {
    async fn new(g: &'static str) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([g]);
        t.reset_guilds(&[g]).await;
        Some(Self { t, g })
    }

    async fn with_guild(g: &'static str, bot: P, with_made: bool) -> Option<(Self, Arc<FakeDiscord>)> {
        let db = Self::new(g).await?;
        db.t.set_discord(offline_discord_as(&[fake_guild(g, bot, with_made).json()]));
        let fake = FakeDiscord::install(&db.t);
        Some((db, fake))
    }

    async fn get(&self, name: &str) -> Option<bot_db::SavedEmbed> {
        svc::get_saved_embed(self.t.db(), self.g, name).await.expect("query")
    }

    async fn create(&self, name: &str, tpl: Value) -> bot_db::SavedEmbed {
        svc::create_saved_embed(&self.t, self.g, name, &tpl, ADMIN_ID).await.expect("created")
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn edit_err(e: anyhow::Error) -> EmbedEditError {
    e.downcast_ref::<EmbedEditError>().cloned().unwrap_or_else(|| panic!("not an EmbedEditError: {e}"))
}

#[tokio::test]
async fn db_create_ensures_settings_and_rejects_duplicates_and_bad_templates() {
    let Some(db) = Db::new("955014000000000001").await else { return };
    let row = db.create("rules", json!({ "content": "hello", "embed": { "title": "Rules" } })).await;
    assert_eq!((row.guild_id.as_str(), row.name.as_str(), row.message_id.clone()), (db.g, "rules", None));
    assert!(bot_db::is_cuid(&row.id));
    let n: i64 = bot_db::sqlx::query_scalar(r#"SELECT COUNT(*) FROM "GuildSettings" WHERE "guildId" = $1"#).bind(db.g).fetch_one(db.t.db()).await.expect("count");
    assert_eq!(n, 1);
    let taken = edit_err(svc::create_saved_embed(&db.t, db.g, "rules", &json!({ "content": "x" }), ADMIN).await.expect_err("taken"));
    assert_eq!(taken.code, EditCode::Taken);
    let bad = edit_err(svc::create_saved_embed(&db.t, db.g, "empty", &empty_template(), ADMIN).await.expect_err("invalid"));
    assert_eq!(bad.code, EditCode::Invalid);
    assert!(!bad.errors.is_empty());
    assert!(db.get("empty").await.is_none());
    assert_eq!(db.get("rules").await.map(|r| r.id), Some(row.id));
    assert!(svc::count_saved_embeds(db.t.db(), db.g).await.expect("count") >= 1);
    assert_eq!(db.t.cache().get(&svc::embed_cache_key(db.g, "rules")).await, Some(normalize_template(&row.template)), "write-through");
    db.done().await;
}

async fn fill(t: &DbTestApp, g: &str, n: usize, prefix: &str) {
    let sql = format!(
        r#"INSERT INTO "SavedEmbed" ("id", "guildId", "name", "template", "createdBy", "createdAt", "updatedAt")
           SELECT 'fill' || $1 || '-' || i, $1, $3 || i, '{{"content":"hello","embed":null,"buttons":[]}}'::jsonb, '1', {now}, {now} FROM generate_series(0, $2 - 1) AS i"#,
        now = bot_db::SQL_NOW
    );
    t.settings().ensure_guild_settings(g).await.expect("settings");
    bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(sql)).bind(g).bind(n as i32).bind(prefix).execute(t.db()).await.expect("filled");
}

#[tokio::test]
async fn db_create_enforces_the_per_guild_cap() {
    let Some(db) = Db::new("955014000000000002").await else { return };
    fill(&db.t, db.g, svc::SAVED_EMBEDS_LIMIT, "e").await;
    let e = edit_err(svc::create_saved_embed(&db.t, db.g, "one-too-many", &json!({ "content": "x" }), ADMIN).await.expect_err("limit"));
    assert_eq!(e.code, EditCode::Limit);
    assert_eq!(e.message, "This server already has 500 saved embeds, the maximum. Delete one with `/embed delete` first.");
    let rows = svc::list_saved_embeds(db.t.db(), db.g).await.expect("list");
    assert_eq!(rows.len(), svc::SAVED_EMBEDS_LIMIT);
    assert_eq!(rows.iter().take(2).map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["e0", "e1"]);
    db.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_mutate_serialises_concurrent_edits_and_replace_reports_not_found() {
    let Some(db) = Db::new("955014000000000003").await else { return };
    db.create("mut", json!({ "content": "hello", "embed": { "title": "T" } })).await;
    let (a, b, c) = tokio::join!(
        svc::mutate_saved_embed(&db.t, db.g, "mut", |t| svc::add_field(t, "A", "1", false, None)),
        svc::mutate_saved_embed(&db.t, db.g, "mut", |t| svc::add_field(t, "B", "2", false, None)),
        svc::mutate_saved_embed(&db.t, db.g, "mut", |t| svc::add_button(t, "L", "https://a.b", None, None)),
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    let row = db.get("mut").await.expect("row");
    let tpl = normalize_template(&row.template);
    let mut names: Vec<&str> = tpl["embed"]["fields"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["A", "B"]);
    assert_eq!(tpl["buttons"].as_array().map(Vec::len), Some(1));
    assert_eq!(edit_err(svc::mutate_saved_embed(&db.t, db.g, "nope-nope", |t| Ok(t.clone())).await.expect_err("missing")).code, EditCode::NotFound);
    assert_eq!(edit_err(svc::mutate_saved_embed(&db.t, db.g, "mut", |_| Ok(empty_template())).await.expect_err("empty")).code, EditCode::Invalid);
    assert!(svc::mutate_saved_embed(&db.t, db.g, "mut", |t| svc::add_button(t, "x", "ftp://nope", None, None)).await.is_err());
    assert_eq!(normalize_template(&db.get("mut").await.expect("row").template), tpl, "a rejected mutation changes nothing");
    let gone = edit_err(svc::replace_template(&db.t, db.g, "cnotarealid000000000000000", json!({ "content": "x" })).await.expect_err("gone"));
    assert_eq!(gone.code, EditCode::NotFound);
    assert_eq!(gone.message, "That saved embed no longer exists. See `/embed list`.");
    let replaced = svc::replace_template(&db.t, db.g, &row.id, json!({ "content": "replaced" })).await.expect("replaced");
    assert_eq!(normalize_template(&replaced.template)["content"], "replaced");
    assert!(replaced.updated_at >= row.updated_at);
    db.done().await;
}

#[tokio::test]
async fn db_search_is_case_insensitive_ordered_and_never_fails() {
    let Some(db) = Db::new("955014000000000004").await else { return };
    for n in ["search-b", "search-a", "Search-c-extra"] {
        db.create(&svc::normalize_embed_name(n).unwrap_or_default(), json!({ "content": "hello" })).await;
    }
    let found = svc::search_saved_embeds(db.t.db(), db.g, "SEARCH", 25).await;
    assert_eq!(found.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["search-a", "search-b", "search-c-extra"]);
    let keys: Vec<String> = json!(found[0]).as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    assert_eq!(keys, vec!["id", "name", "channelId", "messageId"]);
    assert_eq!(svc::search_saved_embeds(db.t.db(), db.g, "search", 2).await.len(), 2);
    assert!(svc::search_saved_embeds(db.t.db(), db.g, "zzzzzz-none", 25).await.is_empty());
    assert!(svc::search_saved_embeds(db.t.db(), db.g, "", 25).await.len() >= 3, "an empty query lists");
    assert!(svc::search_saved_embeds(db.t.db(), "955014999999999999", "search", 25).await.is_empty(), "other guilds see nothing");
    db.done().await;
}

#[tokio::test]
async fn db_get_saved_embed_template_normalises_the_name_and_never_fails() {
    let Some(db) = Db::new("955014000000000005").await else { return };
    db.create("my-rules", json!({ "content": "the rules" })).await;
    db.t.cache().del(&[svc::embed_cache_key(db.g, "my-rules")]).await;
    assert_eq!(svc::get_saved_embed_template(&db.t, db.g, "My Rules").await.map(|t| t["content"].clone()), Some(json!("the rules")));
    assert_eq!(svc::get_saved_embed_template(&db.t, db.g, "my-rules").await.map(|t| t["content"].clone()), Some(json!("the rules")));
    assert!(svc::get_saved_embed_template(&db.t, db.g, "does-not-exist").await.is_none());
    assert_eq!(db.t.cache().get(&svc::embed_cache_key(db.g, "does-not-exist")).await, Some(json!({ "__none": true })), "a miss is cached");
    assert!(svc::get_saved_embed_template(&db.t, db.g, "🎉").await.is_none());
    assert!(svc::get_saved_embed_template(&db.t, "", "my-rules").await.is_none());
    db.done().await;
}

#[tokio::test]
async fn db_the_message_delete_hook_clears_the_message_id_only() {
    let Some(db) = Db::new("955014000000000006").await else { return };
    let row = db.create("hooked", json!({ "content": "hello" })).await;
    svc::set_published(db.t.db(), &row.id, Some(CH), Some("800000000000099001")).await.expect("published");
    svc::on_embed_message_delete(db.t.db(), db.g, "800000000000099999").await;
    assert_eq!(db.get("hooked").await.and_then(|r| r.message_id).as_deref(), Some("800000000000099001"), "another message: untouched");
    svc::on_embed_message_delete(db.t.db(), "955014999999999999", "800000000000099001").await;
    assert_eq!(db.get("hooked").await.and_then(|r| r.message_id).as_deref(), Some("800000000000099001"), "another guild: untouched");
    // Through the hook runner (always: even with the module off).
    db.t.install(registry());
    db.t.settings().update_settings(db.g, SettingsPatch::new().set("embedsEnabled", false)).await.expect("off");
    db.t.run_hooks(MessageDelete {
        guild_id: gid(db.g),
        message_id: MessageId::new(800000000000099001),
        channel_id: cid(CH),
        message: None,
        partial: true,
        channel: None,
    })
    .await;
    let after = db.get("hooked").await.expect("row");
    assert_eq!(after.message_id, None);
    assert_eq!(after.channel_id.as_deref(), Some(CH), "the channel stays as a hint");
    db.done().await;
}

#[tokio::test]
async fn db_set_published_swallows_a_deleted_row_and_delete_reports_a_gone_row() {
    let Some((db, fake)) = Db::with_guild("955014000000000007", bot_perms(), false).await else { return };
    let row = db.create("doomed", json!({ "content": "hello" })).await;
    fake.put_message("800000000000000777", CH, TEST_BOT_ID);
    svc::set_published(db.t.db(), &row.id, Some(CH), Some("800000000000000777")).await.expect("published");
    let published = db.get("doomed").await.expect("row");
    let first = svc::delete_saved_embed_and_message(&db.t, gid(db.g), &published, true).await.expect("deleted");
    assert_eq!((first.deleted, first.message_deleted), (true, true));
    assert_eq!(fake.deleted.lock().map(|d| d.clone()).unwrap_or_default(), vec!["800000000000000777"]);
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&db.t, "800000000000000777").as_deref(), Some("embeds"));
    assert!(db.get("doomed").await.is_none());
    assert_eq!(db.t.cache().get(&svc::embed_cache_key(db.g, "doomed")).await, Some(json!({ "__none": true })));
    let again = svc::delete_saved_embed_and_message(&db.t, gid(db.g), &published, true).await.expect("ran");
    assert_eq!((again.deleted, again.message_deleted), (false, false));
    svc::set_published(db.t.db(), &row.id, Some(CH), Some("whatever")).await.expect("a deleted row is not an error");
    let keep = db.create("kept-msg", json!({ "content": "hello" })).await;
    let no_msg = svc::delete_saved_embed_and_message(&db.t, gid(db.g), &keep, true).await.expect("deleted");
    assert_eq!((no_msg.deleted, no_msg.message_deleted), (true, false));
    // A message that is already gone counts as deleted.
    let gone = db.create("gone-msg", json!({ "content": "hello" })).await;
    svc::set_published(db.t.db(), &gone.id, Some(CH), Some("800000000000000778")).await.expect("published");
    let gone = db.get("gone-msg").await.expect("row");
    let r = svc::delete_saved_embed_and_message(&db.t, gid(db.g), &gone, true).await.expect("deleted");
    assert_eq!((r.deleted, r.message_deleted), (true, true));
    db.done().await;
}

#[tokio::test]
async fn db_publish_posts_tracks_reports_refused_roles_and_replaced_copies() {
    let Some((db, fake)) = Db::with_guild("955014000000000008", bot_perms(), false).await else { return };
    let member = member_of(&db.t, db.g, MEMBER);
    let tpl = json!({ "content": format!("ping <@&{R1}>"), "embed": { "title": "T" }, "buttons": [{ "label": "L", "url": "https://a.b" }] });
    let row = db.create("pub", tpl).await;
    let g = gid(db.g);
    assert_eq!(svc::publish_saved_embed(&db.t, &row, g, cid(NO_VIEW), Some(&member), None).await.expect("ran"), PublishOutcome::MemberNoAccess);
    match svc::publish_saved_embed(&db.t, &row, g, cid(NO_BOT), Some(&member), None).await.expect("ran") {
        PublishOutcome::Channel { mut missing, .. } => {
            missing.sort();
            assert_eq!(missing, vec!["EmbedLinks", "SendMessages"]);
        }
        other => panic!("{other:?}"),
    }
    let ok = svc::publish_saved_embed(&db.t, &row, g, cid(CH), Some(&member), None).await.expect("ran");
    let PublishOutcome::Posted { message, previous, refused } = ok else { panic!("{ok:?}") };
    assert_eq!(refused, vec![R1]);
    assert_eq!(previous, None);
    let sent = fake.sent();
    assert_eq!(mentions(&sent[0].1["allowed_mentions"]), json!({ "parse": ["users"], "roles": [] }));
    assert_eq!(sent[0].1["embeds"].as_array().map(Vec::len), Some(1));
    assert_eq!(sent[0].1["components"].as_array().map(Vec::len), Some(1));
    let tracked = db.get("pub").await.expect("row");
    assert_eq!((tracked.message_id.as_deref(), tracked.channel_id.as_deref()), (Some(message.id.as_str()), Some(CH)));

    let second = svc::publish_saved_embed(&db.t, &tracked, g, cid(CH), Some(&member), None).await.expect("ran");
    let PublishOutcome::Posted { previous, .. } = second else { panic!("{second:?}") };
    assert_eq!(previous, Some((CH.to_string(), message.id.clone())));
    assert!(fake.messages.lock().map(|m| m.contains_key(&message.id)).unwrap_or(false), "the previous message is not deleted");

    *fake.fail_send.lock().unwrap_or_else(|e| e.into_inner()) = Some((0, 50035));
    assert!(matches!(svc::publish_saved_embed(&db.t, &tracked, g, cid(CH), Some(&member), None).await.expect("ran"), PublishOutcome::Discord { .. }));
    *fake.fail_send.lock().unwrap_or_else(|e| e.into_inner()) = Some((0, 50013));
    assert!(matches!(svc::publish_saved_embed(&db.t, &tracked, g, cid(CH), Some(&member), None).await.expect("ran"), PublishOutcome::Channel { .. }));
    *fake.fail_send.lock().unwrap_or_else(|e| e.into_inner()) = None;

    let empty = db.create("pub-empty", json!({ "content": "{channel.name}" })).await;
    assert_eq!(svc::publish_saved_embed(&db.t, &empty, g, cid(EMPTY_NAME), Some(&member), None).await.expect("ran"), PublishOutcome::Empty);
    // test/review-automations.test.js: Send Messages is needed too, and with access a vanished row is NOTFOUND.
    assert_eq!(svc::publish_saved_embed(&db.t, &row, g, cid(READ_ONLY), Some(&member), None).await.expect("ran"), PublishOutcome::MemberNoAccess);
    let ghost = bot_db::SavedEmbed { id: "cnotarealid000000000000000".into(), ..row.clone() };
    let e = svc::publish_saved_embed(&db.t, &ghost, g, cid(CH), Some(&member), None).await.expect_err("gone");
    assert_eq!(edit_err(e).code, EditCode::NotFound, "with access it gets past the check");
    // A role that is mentionable pings.
    let r2 = db.create("pub-r2", json!({ "content": format!("<@&{R2}>") })).await;
    svc::publish_saved_embed(&db.t, &r2, g, cid(CH), Some(&member), None).await.expect("ran");
    assert_eq!(fake.sent().last().map(|s| mentions(&s.1["allowed_mentions"])), Some(json!({ "parse": ["users"], "roles": [R2] })));
    db.done().await;
}

/// The JS had the option's resolved channel object: a channel the cache does not hold yet (just created, a thread)
/// is fetched, not refused as "Channel not found"; so is `welcome-channel channel:`.
#[tokio::test]
async fn db_publish_and_welcome_fetch_a_channel_the_cache_does_not_hold() {
    const REMOTE: &str = "955014000000000777";
    let Some((db, fake)) = Db::with_guild("955014000000000031", bot_perms(), false).await else { return };
    fake.remote.lock().unwrap_or_else(|e| e.into_inner()).push(channel_json(db.g, REMOTE, "remote"));
    let member = member_of(&db.t, db.g, MEMBER);
    let row = db.create("far", json!({ "content": "hello" })).await;
    let out = svc::publish_saved_embed(&db.t, &row, gid(db.g), cid(REMOTE), Some(&member), None).await.expect("ran");
    assert!(matches!(out, PublishOutcome::Posted { .. }), "{out:?}");
    assert_eq!(fake.sent().last().map(|s| s.0.clone()), Some(REMOTE.to_string()));
    let r = svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, Some(REMOTE), &[])).await.expect("ran");
    assert!(matches!(r, WelcomeOutcome::Ready { ref channel_id, .. } if channel_id == REMOTE), "{r:?}");
    // Still refused when Discord does not know the channel either.
    let gone = svc::publish_saved_embed(&db.t, &row, gid(db.g), cid("955014000000000778"), Some(&member), None).await.expect("ran");
    assert!(matches!(gone, PublishOutcome::Channel { .. }), "{gone:?}");
    db.done().await;
}

/// `test/sec-dashboard.test.js`: a channel named after a role mention never adds a role ping; a role the admin typed
/// is allowed or refused by the usual rule.
#[tokio::test]
async fn db_a_channel_name_never_adds_a_role_ping() {
    let Some((db, fake)) = Db::with_guild("955014000000000018", bot_perms(), false).await else { return };
    let g = gid(db.g);
    let (pinger, plain) = (member_of(&db.t, db.g, PINGER), member_of(&db.t, db.g, MEMBER));
    let row = db.create("news", json!({ "content": "New in {channel.name}" })).await;
    for m in [&pinger, &plain] {
        let PublishOutcome::Posted { refused, .. } = svc::publish_saved_embed(&db.t, &row, g, cid(ROLE_NAMED), Some(m), None).await.expect("ran") else { panic!() };
        assert!(refused.is_empty(), "the admin typed no role, so nothing is refused either");
        let sent = fake.sent().last().cloned().expect("sent").1;
        assert_eq!(sent["allowed_mentions"]["roles"], json!([]), "no role may be pinged");
        let content = s(&sent["content"]).to_string();
        assert!(content.starts_with("New in <"), "{content}");
        assert!(svc::extract_role_mention_ids(Some(&content)).is_empty(), "the name no longer reads as a role mention");
    }
    let tpl = json!({ "content": format!("Ping <@&{R1}> about {{channel.name}}") });
    let guild = cached_guild(&db.t, g).expect("cached");
    let channel = cached_channel(&db.t, g, cid(ROLE_NAMED)).expect("cached");
    let content = tpl["content"].as_str();
    let allowed = svc::mention_policy(content, MentionMode::Publish, Some((&guild, &channel)), svc::member_key(Some(&pinger)));
    assert_eq!(json!(allowed), json!({ "parse": ["users"], "roles": [R1] }));
    let refused = svc::mention_policy(content, MentionMode::Publish, Some((&guild, &channel)), svc::member_key(Some(&plain)));
    assert_eq!(refused.roles, Some(Vec::new()));
    assert_eq!(svc::mention_policy(content, MentionMode::None, Some((&guild, &channel)), svc::member_key(Some(&pinger))), MentionPolicy::none());
    let typed = db.create("typed", tpl).await;
    let PublishOutcome::Posted { refused, .. } = svc::publish_saved_embed(&db.t, &typed, g, cid(CH), Some(&plain), None).await.expect("ran") else { panic!() };
    assert_eq!(refused, vec![R1]);
    db.done().await;
}

#[tokio::test]
async fn db_update_edits_in_place_and_heals_deleted_messages_and_channels() {
    let Some((db, fake)) = Db::with_guild("955014000000000009", bot_perms(), false).await else { return };
    let member = member_of(&db.t, db.g, ADMIN);
    let g = gid(db.g);
    let row = db.create("upd", json!({ "embed": { "title": "Old" } })).await;
    assert!(matches!(svc::update_published_embed(&db.t, &row, g, Some(&member), None).await.expect("ran"), UpdateOutcome::NotPublished { .. }));
    let PublishOutcome::Posted { message, .. } = svc::publish_saved_embed(&db.t, &row, g, cid(CH), Some(&member), None).await.expect("ran") else { panic!() };
    svc::mutate_saved_embed(&db.t, db.g, "upd", |t| Ok(svc::set_images(&svc::set_title_url(t, Some("https://a.b")), &Change::Keep, &Change::Set("https://a.b/t.png".into()))))
        .await
        .expect("mutated");
    let fresh = db.get("upd").await.expect("row");
    let done = svc::update_published_embed(&db.t, &fresh, g, Some(&member), None).await.expect("ran");
    assert!(matches!(done, UpdateOutcome::Updated { .. }), "{done:?}");
    let edit = fake.edits.lock().map(|e| e.last().cloned()).unwrap_or_default().expect("edited").1;
    assert_eq!(edit["content"], "", "an empty content clears old text");
    assert_eq!(mentions(&edit["allowed_mentions"]), json!({ "parse": [] }));
    assert_eq!(edit["embeds"][0]["url"], "https://a.b");
    assert!(edit["components"].is_array());

    // The message is gone.
    fake.messages.lock().map(|mut m| m.remove(&message.id)).ok();
    assert_eq!(svc::update_published_embed(&db.t, &fresh, g, Some(&member), None).await.expect("ran"), UpdateOutcome::MessageGone);
    let draft = db.get("upd").await.expect("row");
    assert_eq!((draft.message_id.clone(), draft.channel_id.as_deref()), (None, Some(CH)));
    assert!(matches!(svc::update_published_embed(&db.t, &draft, g, Some(&member), None).await.expect("ran"), UpdateOutcome::NotPublished { .. }));

    // The channel is gone (REST 10003 on a cache miss).
    svc::set_published(db.t.db(), &row.id, Some("900000000000009999"), Some("800000000000099777")).await.expect("published");
    assert_eq!(svc::update_published_embed(&db.t, &row, g, Some(&member), None).await.expect("ran"), UpdateOutcome::ChannelGone);
    let cleared = db.get("upd").await.expect("row");
    assert_eq!((cleared.channel_id.clone(), cleared.message_id.clone()), (None, None));

    // Someone else's message is never edited.
    fake.put_message("800000000000055555", CH, MEMBER);
    svc::set_published(db.t.db(), &row.id, Some(CH), Some("800000000000055555")).await.expect("published");
    let before = fake.edits.lock().map(|e| e.len()).unwrap_or(0);
    assert_eq!(svc::update_published_embed(&db.t, &row, g, Some(&member), None).await.expect("ran"), UpdateOutcome::MessageGone);
    assert_eq!(fake.edits.lock().map(|e| e.len()).unwrap_or(0), before);

    // An archived thread, a member who cannot see the channel, and a missing bot permission.
    svc::publish_saved_embed(&db.t, &row, g, cid(CH), Some(&member), None).await.expect("ran");
    *fake.fail_edit.lock().unwrap_or_else(|e| e.into_inner()) = Some(50083);
    assert_eq!(svc::update_published_embed(&db.t, &row, g, Some(&member), None).await.expect("ran"), UpdateOutcome::Archived);
    *fake.fail_edit.lock().unwrap_or_else(|e| e.into_inner()) = None;
    fake.put_message("800000000000066666", NO_BOT, TEST_BOT_ID);
    svc::set_published(db.t.db(), &row.id, Some(NO_BOT), Some("800000000000066666")).await.expect("published");
    match svc::update_published_embed(&db.t, &row, g, Some(&member), None).await.expect("ran") {
        UpdateOutcome::NoAccess { missing } => assert!(missing.contains(&"EmbedLinks".to_string())),
        other => panic!("{other:?}"),
    }
    assert!(db.get("upd").await.and_then(|r| r.message_id).is_some(), "nothing is cleared when only a permission is missing");
    svc::set_published(db.t.db(), &row.id, Some(NO_VIEW), Some("800000000000066667")).await.expect("published");
    let plain = member_of(&db.t, db.g, MEMBER);
    assert_eq!(svc::update_published_embed(&db.t, &row, g, Some(&plain), None).await.expect("ran"), UpdateOutcome::MemberNoAccess);
    db.done().await;
}

fn form(custom_id: &str, g: &str, user: &str, role: &str, values: &[(&str, &str)]) -> serenity::all::Interaction {
    let mut all: Vec<(&str, &str)> = vec![("tpl-content", ""), ("tpl-title", ""), ("tpl-description", ""), ("tpl-color", ""), ("tpl-footer", "")];
    for (k, v) in values {
        if let Some(slot) = all.iter_mut().find(|(id, _)| id == k) {
            slot.1 = v;
        }
    }
    FakeInteraction::modal(custom_id, &all).guild(g).user(user).roles(&[role]).channel(CH).build()
}

fn attachment_names(body: &Value) -> Vec<String> {
    body["attachments"].as_array().into_iter().flatten().filter_map(|a| a["filename"].as_str().map(String::from)).collect()
}

#[tokio::test]
async fn db_the_form_creates_edits_and_keeps_what_the_form_does_not_hold() {
    let Some((db, _fake)) = Db::with_guild("955014000000000010", bot_perms(), false).await else { return };
    db.t.install(registry());
    let new_id = bot_config::custom_id!("embed-modal", "new", "formed").unwrap_or_default();
    route_interaction(&db.t, form(&new_id, db.g, ADMIN, ADMIN_ROLE, &[("tpl-title", "Hello {nope}"), ("tpl-description", "Body"), ("tpl-color", "#ff0000")])).await;
    assert_eq!(db.t.responses.names(), vec!["deferReply", "editReply"]);
    let calls = db.t.responses.calls();
    assert!(!calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Embed Created"));
    assert!(calls[1].embed_description().unwrap_or("").contains("{nope}"));
    assert_eq!(calls[1].body["embeds"][0]["fields"][0]["name"], "Variables");
    let row = db.get("formed").await.expect("row");
    assert_eq!(normalize_template(&row.template)["embed"]["color"], 0xff0000);

    // Add parts the form does not hold, then edit through the form: they survive.
    svc::mutate_saved_embed(&db.t, db.g, "formed", |t| {
        let t = svc::add_field(t, "F", "v", false, None)?;
        let t = svc::add_button(&t, "B", "https://a.b", None, None)?;
        svc::set_author(&t, Some("Auth"), &Change::Keep, &Change::Keep)
    })
    .await
    .expect("mutated");
    svc::set_published(db.t.db(), &row.id, Some(CH), Some("800000000000099500")).await.expect("published");
    let edit_id = bot_config::custom_id!("embed-modal", row.id).unwrap_or_default();
    db.t.responses.clear();
    route_interaction(&db.t, form(&edit_id, db.g, ADMIN, ADMIN_ROLE, &[("tpl-content", "Text"), ("tpl-title", "New title")])).await;
    let reply = db.t.responses.calls().last().cloned().expect("answered");
    assert_eq!(reply.embed_title(), Some("✅  Embed Saved"));
    assert!(reply.embed_description().unwrap_or("").contains("/embed update name:formed"), "the publish hint is shown");
    let tpl = normalize_template(&db.get("formed").await.expect("row").template);
    assert_eq!(tpl["content"], "Text");
    assert_eq!(tpl["embed"]["title"], "New title");
    assert!(tpl["embed"].get("description").is_none(), "an empty input clears that part");
    assert_eq!(tpl["embed"]["fields"].as_array().map(Vec::len), Some(1));
    assert_eq!(tpl["embed"]["author"]["name"], "Auth");
    assert_eq!(tpl["buttons"].as_array().map(Vec::len), Some(1));
    assert!(tpl["embed"].get("color").is_none(), "a blank colour input clears it");

    // A rejected form hands the typed text back as a draft file.
    db.t.responses.clear();
    route_interaction(&db.t, form(&edit_id, db.g, ADMIN, ADMIN_ROLE, &[("tpl-content", "keep me"), ("tpl-color", "not-a-colour")])).await;
    let bad = db.t.responses.calls().last().cloned().expect("answered");
    assert_eq!(bad.embed_title(), Some("❌  Invalid Embed"));
    assert_eq!(attachment_names(&bad.body), vec!["embed-draft.json"]);
    assert_eq!(normalize_template(&db.get("formed").await.expect("row").template)["content"], "Text", "nothing was saved");

    // Name taken: the draft is kept for the admin.
    db.t.responses.clear();
    route_interaction(&db.t, form(&new_id, db.g, ADMIN, ADMIN_ROLE, &[("tpl-content", "again")])).await;
    let taken = db.t.responses.calls().last().cloned().expect("answered");
    assert_eq!(taken.embed_title(), Some("❌  Name Taken"));
    assert_eq!(attachment_names(&taken.body), vec!["embed-draft.json"]);

    // Deleted while the form was open.
    db.t.responses.clear();
    let orphan_id = bot_config::custom_id!("embed-modal", "cnotarealid000000000000000").unwrap_or_default();
    route_interaction(&db.t, form(&orphan_id, db.g, ADMIN, ADMIN_ROLE, &[("tpl-content", "late")])).await;
    let orphan = db.t.responses.calls().last().cloned().expect("answered");
    assert_eq!(orphan.embed_title(), Some("❌  Embed Not Found"));
    assert_eq!(attachment_names(&orphan.body), vec!["embed-draft.json"]);

    // An empty create form and a non-admin submitter.
    db.t.responses.clear();
    let blank = bot_config::custom_id!("embed-modal", "new", "blank").unwrap_or_default();
    route_interaction(&db.t, form(&blank, db.g, ADMIN, ADMIN_ROLE, &[])).await;
    assert_eq!(db.t.responses.calls().last().and_then(|c| c.embed_title().map(String::from)).as_deref(), Some("❌  Invalid Embed"));
    assert!(db.get("blank").await.is_none());
    db.t.responses.clear();
    let sneaky = bot_config::custom_id!("embed-modal", "new", "sneaky").unwrap_or_default();
    route_interaction(&db.t, form(&sneaky, db.g, MEMBER, MEMBER_ROLE, &[("tpl-content", "x")])).await;
    assert_eq!(db.t.responses.names(), vec!["reply"]);
    assert_eq!(db.t.responses.calls()[0].embed_title(), Some("❌  Insufficient Permissions"));
    assert!(db.t.responses.calls()[0].ephemeral());
    assert!(db.get("sneaky").await.is_none());
    db.done().await;
}

fn welcome_args(db: &Db, create: bool, channel: Option<&str>, mod_roles: &[&str]) -> WelcomeArgs {
    let member = member_of(&db.t, db.g, ADMIN);
    let settings: bot_db::GuildSettings = settings_row(db.g);
    let settings = bot_db::GuildSettings { mod_role_ids: mod_roles.iter().map(|s| s.to_string()).collect(), ..settings };
    WelcomeArgs {
        guild_id: gid(db.g),
        user: member.user.clone(),
        member: Some(member),
        interaction_channel_id: cid(CH),
        create,
        channel_id: channel.map(cid),
        settings: Some(Arc::new(settings)),
    }
}

#[tokio::test]
async fn db_welcome_channel_creates_a_read_only_channel_posts_four_presets_and_never_overwrites() {
    let Some((db, fake)) = Db::with_guild("955014000000000011", bot_perms(), true).await else { return };
    *fake.created.lock().unwrap_or_else(|e| e.into_inner()) = Some(channel_json(db.g, MADE, "welcome"));
    let r = svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, None, &[R1, "999999999999999999"])).await.expect("ran");
    let WelcomeOutcome::Ready { channel_id, created_channel, created_presets, invite, .. } = r else { panic!("{r:?}") };
    assert_eq!(channel_id, MADE);
    assert!(created_channel);
    assert_eq!(created_presets, svc::PRESET_NAMES.map(String::from).to_vec());
    assert_eq!(invite.map(|i| i.source), Some("created"));
    let spec = fake.create_spec.lock().map(|s| s.clone()).unwrap_or_default().expect("created");
    assert_eq!((spec.name.as_str(), spec.position), ("welcome", 0));
    assert!(spec.everyone_deny.contains(P::SEND_MESSAGES));
    assert_eq!(spec.topic, "Welcome to Test Guild!");
    assert_eq!(spec.bot_id.to_string(), TEST_BOT_ID);
    let sent = fake.sent();
    assert_eq!(sent.len(), 4);
    for (_, p) in &sent {
        assert_eq!(mentions(&p["allowed_mentions"]), json!({ "parse": [] }));
    }
    let team = sent[3].1["embeds"][0]["description"].as_str().unwrap_or("").to_string();
    assert!(team.contains(&format!("<@&{R1}>")));
    assert!(!team.contains("999999999999999999"), "unknown roles are left out");
    assert_eq!(sent[2].1["components"].as_array().map(Vec::len), Some(1), "the links preset carries the invite button");
    for name in svc::PRESET_NAMES {
        let row = db.get(name).await.expect("row");
        assert_eq!(row.channel_id.as_deref(), Some(MADE));
        assert!(row.message_id.is_some());
    }

    // An admin edits a preset; running it again elsewhere reuses (not overwrites) every preset.
    svc::mutate_saved_embed(&db.t, db.g, "welcome-rules", |t| Ok(svc::set_color(t, Some(0x00ff00)))).await.expect("mutated");
    let again = svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, None, &[])).await.expect("ran");
    assert_eq!(again, WelcomeOutcome::Already { channel_id: MADE.into() });
    let reused = svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, Some(OPEN), &[])).await.expect("ran");
    let WelcomeOutcome::Ready { created_presets, reused_presets, tip, .. } = reused else { panic!("{reused:?}") };
    assert!(created_presets.is_empty());
    assert_eq!(reused_presets.len(), 4);
    assert!(tip.unwrap_or_default().contains("make it read-only"));
    let sent = fake.sent();
    assert_eq!(sent[5].0, OPEN);
    assert_eq!(sent[5].1["embeds"][0]["color"], 0x00ff00, "the edited preset was posted as edited");

    // The interaction's channel (create: false), and a channel that is not a text channel.
    let here = svc::set_up_welcome_channel(&db.t, welcome_args(&db, false, None, &[])).await.expect("ran");
    assert!(matches!(here, WelcomeOutcome::Ready { ref channel_id, .. } if channel_id == CH));
    let mut voice = welcome_args(&db, false, None, &[]);
    voice.interaction_channel_id = cid(VOICE);
    assert_eq!(svc::set_up_welcome_channel(&db.t, voice).await.expect("ran"), WelcomeOutcome::WrongChannel);

    // A failing send reports what was posted.
    *fake.fail_send.lock().unwrap_or_else(|e| e.into_inner()) = Some((fake.sends.load(Ordering::SeqCst) + 3, 50013));
    let partial = svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, Some(CH), &[])).await.expect("ran");
    let WelcomeOutcome::Partial { posted, unposted, .. } = partial else { panic!("{partial:?}") };
    assert_eq!(posted, vec!["welcome-greeting", "welcome-rules"]);
    assert_eq!(unposted, vec!["welcome-links", "welcome-team"]);
    db.done().await;
}

#[tokio::test]
async fn db_welcome_channel_refuses_without_permissions_or_room() {
    let poor = P::VIEW_CHANNEL | P::SEND_MESSAGES;
    let Some((db, _fake)) = Db::with_guild("955014000000000012", poor, false).await else { return };
    match svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, None, &[])).await.expect("ran") {
        WelcomeOutcome::BotPerms { missing } => assert!(missing.contains(&"ManageChannels".to_string())),
        other => panic!("{other:?}"),
    }
    fill(&db.t, db.g, svc::SAVED_EMBEDS_LIMIT - 2, "fill").await;
    let out = svc::set_up_welcome_channel(&db.t, welcome_args(&db, true, None, &[])).await.expect("ran");
    assert_eq!(out, WelcomeOutcome::Limit { needed: 4, count: svc::SAVED_EMBEDS_LIMIT as i64 - 2 });
    db.done().await;
}

// ── 8. Every subcommand through the router ────────────────────────────────────

struct Run<'a> {
    db: &'a Db,
}

impl Run<'_> {
    async fn go(&self, i: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
        self.db.t.responses.clear();
        route_interaction(&self.db.t, i.build()).await;
        self.db.t.responses.calls()
    }

    fn slash(&self) -> FakeInteraction {
        admin_slash(self.db.g)
    }
}

fn last_title(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.iter().rev().find_map(|c| c.embed_title().map(String::from)).unwrap_or_default()
}

fn last_desc(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.iter().rev().find_map(|c| c.embed_description().map(String::from)).unwrap_or_default()
}

const S: u8 = option_type::STRING;
const I: u8 = option_type::INTEGER;
const B: u8 = option_type::BOOLEAN;

#[tokio::test]
async fn db_embed_command_flows() {
    let Some((db, fake)) = Db::with_guild("955014000000000013", bot_perms(), false).await else { return };
    db.t.install(registry());
    let run = Run { db: &db };
    db.create("flow", json!({ "content": "hi {nope}", "embed": { "title": "Flow" } })).await;

    let c = run.go(run.slash().group("field", "add").option("name", S, json!("Flow")).option("title", S, json!("F1")).option("value", S, json!("V1")).option("inline", B, json!(true))).await;
    assert_eq!(last_title(&c), "✅  Field Added");
    assert!(last_desc(&c).starts_with("Added field 1 to **flow**."));
    assert!(last_desc(&c).contains("Unknown variables are shown as typed: `{nope}`"));
    let c = run.go(run.slash().group("field", "add").option("name", S, json!("flow")).option("title", S, json!("F0")).option("value", S, json!("V0")).option("position", I, json!(1))).await;
    assert!(last_desc(&c).starts_with("Added field 1"));
    let c = run.go(run.slash().group("field", "edit").option("name", S, json!("flow")).option("index", I, json!(1))).await;
    assert_eq!(last_title(&c), "❌  Nothing To Change");
    let c = run.go(run.slash().group("field", "edit").option("name", S, json!("flow")).option("index", I, json!(1)).option("position", I, json!(2)).option("title", S, json!("F0b"))).await;
    assert!(last_desc(&c).starts_with("Updated field 2"));
    let c = run.go(run.slash().group("field", "remove").option("name", S, json!("flow")).option("index", I, json!(1))).await;
    assert!(last_desc(&c).starts_with("Removed field 1 (\"F1\")"));
    let c = run.go(run.slash().group("field", "remove").option("name", S, json!("flow")).option("index", I, json!(9))).await;
    assert_eq!(last_title(&c), "❌  Could Not Save");
    assert!(last_desc(&c).contains("There is no field 9"));
    let c = run.go(run.slash().group("field", "list").option("name", S, json!("flow"))).await;
    assert!(last_desc(&c).contains("F0b"));
    assert_eq!(c.last().map(|x| x.body["embeds"][0]["footer"]["text"].clone()), Some(json!("1/25 fields")));
    assert_eq!(last_title(&c), "Fields of flow");

    let c = run.go(run.slash().group("button", "add").option("name", S, json!("flow")).option("label", S, json!("Site")).option("url", S, json!("https://example.com")).option("emoji", S, json!("🌐"))).await;
    assert_eq!(last_title(&c), "✅  Button Added");
    let c = run.go(run.slash().group("button", "add").option("name", S, json!("flow")).option("label", S, json!("Bad")).option("url", S, json!("ftp://nope"))).await;
    assert_eq!(last_title(&c), "❌  Could Not Save");
    let c = run.go(run.slash().group("button", "add").option("name", S, json!("flow")).option("label", S, json!("Bad")).option("url", S, json!("https://a.b")).option("emoji", S, json!("abc"))).await;
    assert!(last_desc(&c).contains("valid emoji"));
    let c = run.go(run.slash().group("button", "remove").option("name", S, json!("flow")).option("index", I, json!(1))).await;
    assert_eq!(last_title(&c), "✅  Button Removed");

    let c = run.go(run.slash().subcommand("color").option("name", S, json!("flow")).option("color", S, json!("blurple"))).await;
    assert!(last_desc(&c).contains("#5865F2"));
    let c = run.go(run.slash().subcommand("color").option("name", S, json!("flow")).option("color", S, json!("default"))).await;
    assert!(last_desc(&c).contains("server default colour"));
    let c = run.go(run.slash().subcommand("color").option("name", S, json!("flow")).option("color", S, json!("zzz"))).await;
    assert_eq!(last_title(&c), "❌  Unknown Colour");
    let c = run.go(run.slash().subcommand("image").option("name", S, json!("flow"))).await;
    assert_eq!(last_title(&c), "❌  Nothing To Change");
    let c = run.go(run.slash().subcommand("image").option("name", S, json!("flow")).option("image_url", S, json!("https://a.b/i.png")).option("thumbnail_url", S, json!("{server.icon}"))).await;
    assert_eq!(last_title(&c), "✅  Images Updated");
    let c = run.go(run.slash().subcommand("image").option("name", S, json!("flow")).option("image_url", S, json!("javascript:alert(1)"))).await;
    assert_eq!(last_title(&c), "❌  Could Not Save");
    let c = run.go(run.slash().subcommand("author").option("name", S, json!("flow")).option("text", S, json!("Me")).option("icon_url", S, json!("{user.avatar}"))).await;
    assert_eq!(last_title(&c), "✅  Author Updated");
    let c = run.go(run.slash().subcommand("author").option("name", S, json!("flow")).option("text", S, json!("none"))).await;
    assert_eq!(last_title(&c), "✅  Author Removed");
    let c = run.go(run.slash().subcommand("footer").option("name", S, json!("flow")).option("text", S, json!("Foot")).option("timestamp", B, json!(true))).await;
    assert_eq!(last_title(&c), "✅  Footer Updated");
    assert!(last_desc(&c).contains("The timestamp is on."));
    let c = run.go(run.slash().subcommand("title-url").option("name", S, json!("flow")).option("url", S, json!("https://a.b/t"))).await;
    assert_eq!(last_title(&c), "✅  Title Link Updated");
    let c = run.go(run.slash().subcommand("title-url").option("name", S, json!("flow")).option("url", S, json!("ftp://a.b"))).await;
    assert_eq!(last_title(&c), "❌  Could Not Save");

    let c = run.go(run.slash().subcommand("preview").option("name", S, json!("flow"))).await;
    assert_eq!(c[0].method, "deferReply");
    assert!(c[0].ephemeral());
    let preview = c.iter().find(|x| x.method == "editReply").expect("preview");
    assert_eq!(preview.embed_title(), Some("Flow"));
    assert_eq!(preview.body["embeds"][0]["footer"]["text"], "Foot");
    assert_eq!(mentions(&preview.body["allowed_mentions"]), json!({ "parse": [] }));
    assert!(c.iter().any(|x| x.method == "followUp" && x.ephemeral() && x.embed_title() == Some("⚠️  Unknown Variables")));
    let c = run.go(run.slash().subcommand("preview").option("name", S, json!("missing-embed"))).await;
    assert_eq!(last_title(&c), "❌  Embed Not Found");

    let c = run.go(run.slash().subcommand("publish").option("name", S, json!("flow")).option("channel", option_type::CHANNEL, json!(CH))).await;
    assert_eq!(last_title(&c), "✅  Embed Published");
    assert!(last_desc(&c).contains(&format!("Posted **flow** in <#{CH}>. [Jump to message](https://discord.com/channels/{}/{CH}/", db.g)));
    assert_eq!(fake.sent().len(), 1);
    let c = run.go(run.slash().group("field", "add").option("name", S, json!("flow")).option("title", S, json!("F9")).option("value", S, json!("V9"))).await;
    assert!(last_desc(&c).contains(&format!("It is published in <#{CH}>")));
    let c = run.go(run.slash().subcommand("update").option("name", S, json!("flow"))).await;
    assert_eq!(last_title(&c), "✅  Embed Updated");
    db.create("search-a", json!({ "content": "x" })).await;
    let c = run.go(run.slash().subcommand("update").option("name", S, json!("search-a"))).await;
    assert_eq!(last_title(&c), "⚠️  Not Published");
    let c = run.go(run.slash().subcommand("publish").option("name", S, json!("flow")).option("channel", option_type::CHANNEL, json!(NO_BOT))).await;
    assert_eq!(last_title(&c), "❌  Missing Permissions");
    assert_eq!(last_desc(&c), format!("I need **Send Messages, Embed Links** in <#{NO_BOT}> to post there."));

    let c = run.go(run.slash().subcommand("export").option("name", S, json!("flow"))).await;
    let exported = c.iter().find(|x| x.method == "editReply").expect("export");
    assert_eq!(attachment_names(&exported.body), vec!["embed-flow.json"]);
    assert_eq!(exported.embed_title(), Some("✅  Embed Exported"));
    let json_text = bot_core::custom_embed::template_to_json(&normalize_template(&db.get("flow").await.expect("row").template));
    assert_eq!(svc::parse_import_payload(&json_text).template, Some(normalize_template(&db.get("flow").await.expect("row").template)));

    let c = run.go(run.slash().subcommand("import").option("name", S, json!("flow"))).await;
    assert_eq!(last_title(&c), "❌  Nothing To Import");
    let mut both = run.slash().subcommand("import").option("name", S, json!("flow")).option("json", S, json!("{}")).option("file", option_type::ATTACHMENT, json!("700000000000000001"));
    add_attachment(&mut both, "700000000000000001", 1);
    let c = run.go(both).await;
    assert_eq!(last_title(&c), "❌  Pick One");
    let c = run.go(run.slash().subcommand("import").option("name", S, json!("imported")).option("json", S, json!("{ nope"))).await;
    assert_eq!(last_title(&c), "❌  Invalid JSON");
    let c = run.go(run.slash().subcommand("import").option("name", S, json!("imported")).option("json", S, json!(json_text))).await;
    assert_eq!(last_title(&c), "✅  Embed Imported");
    let c = run.go(run.slash().subcommand("import").option("name", S, json!("imported")).option("json", S, json!(json_text))).await;
    assert_eq!(last_title(&c), "❌  Name Taken");
    assert!(last_desc(&c).contains("overwrite:true"));
    let c = run.go(run.slash().subcommand("import").option("name", S, json!("imported")).option("json", S, json!(r#"{"content":"replaced"}"#)).option("overwrite", B, json!(true))).await;
    assert_eq!(last_title(&c), "✅  Embed Replaced");
    assert_eq!(normalize_template(&db.get("imported").await.expect("row").template)["content"], "replaced");
    let mut big = run.slash().subcommand("import").option("name", S, json!("big")).option("file", option_type::ATTACHMENT, json!("700000000000000002"));
    add_attachment(&mut big, "700000000000000002", 99_999);
    let c = run.go(big).await;
    assert_eq!(last_title(&c), "❌  File Too Large");

    // create / edit answer with a form; a taken name and a missing embed answer ephemerally.
    let c = run.go(run.slash().subcommand("create").option("name", S, json!("Brand New"))).await;
    assert_eq!(c[0].method, "showModal");
    assert_eq!(c[0].body["custom_id"], "embed-modal:new:brand-new");
    assert_eq!(c[0].body["title"], "New embed: brand-new");
    assert!(db.get("brand-new").await.is_none(), "nothing is written before the form is submitted");
    let c = run.go(run.slash().subcommand("create").option("name", S, json!("flow"))).await;
    assert_eq!(last_title(&c), "❌  Name Taken");
    assert!(c[0].ephemeral());
    let c = run.go(run.slash().subcommand("edit").option("name", S, json!("flow"))).await;
    assert_eq!(c[0].method, "showModal");
    let flow = db.get("flow").await.expect("row");
    assert_eq!(c[0].body["custom_id"], format!("embed-modal:{}", flow.id));
    let c = run.go(run.slash().subcommand("edit").option("name", S, json!("nothing-here"))).await;
    assert_eq!(last_title(&c), "❌  Embed Not Found");
    svc::mutate_saved_embed(&db.t, db.g, "flow", |t| {
        let mut t = t.clone();
        t["embed"]["description"] = json!("d".repeat(4096));
        Ok(t)
    })
    .await
    .expect("mutated");
    let c = run.go(run.slash().subcommand("edit").option("name", S, json!("flow"))).await;
    assert_eq!(last_title(&c), "❌  Too Long For The Form");
    assert!(last_desc(&c).contains("description"));

    let c = run.go(run.slash().subcommand("delete").option("name", S, json!("ghost-embed"))).await;
    assert_eq!(last_title(&c), "❌  Embed Not Found");

    // autocomplete
    let ac = FakeInteraction::autocomplete("embed").guild(db.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD).subcommand("preview").focused("name", "FLO");
    let c = run.go(ac).await;
    assert_eq!(c[0].choices(), vec![json!({ "name": "flow · published", "value": "flow" })]);
    let ac = FakeInteraction::autocomplete("embed")
        .guild(db.g)
        .user(ADMIN)
        .roles(&[ADMIN_ROLE])
        .permissions(P::MANAGE_GUILD)
        .group("field", "edit")
        .option("name", S, json!("Flow"))
        .focused("index", "f0");
    let c = run.go(ac).await;
    let choices = c[0].choices();
    assert_eq!(choices.iter().map(|x| x["value"].clone()).collect::<Vec<_>>(), vec![json!(1)]);
    assert!(choices[0]["name"].as_str().unwrap_or("").starts_with("1. F0b"));
    db.done().await;
}

fn add_attachment(i: &mut FakeInteraction, id: &str, size: u64) {
    let j = i.json_mut();
    j["data"]["resolved"] = json!({ "attachments": { id: {
        "id": id, "filename": "embed.json", "size": size, "url": "http://127.0.0.1:1/embed.json", "proxy_url": "http://127.0.0.1:1/embed.json"
    } } });
}

#[tokio::test]
async fn db_embed_list_shows_published_and_draft_embeds_and_pages_after_15() {
    let Some((db, _fake)) = Db::with_guild("955014000000000014", bot_perms(), false).await else { return };
    db.t.install(registry());
    let run = Run { db: &db };
    let c = run.go(run.slash().subcommand("list")).await;
    assert_eq!(last_title(&c), "ℹ️  No Saved Embeds");
    db.create("a-draft", json!({ "content": "hello" })).await;
    let b = db.create("b-live", json!({ "content": "hello" })).await;
    svc::set_published(db.t.db(), &b.id, Some(CH), Some("800000000000000123")).await.expect("published");
    let c = run.go(run.slash().subcommand("list")).await;
    assert_eq!(c.iter().map(|x| x.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"]);
    let page = &c[1].body;
    assert!(page["components"].as_array().map(Vec::is_empty).unwrap_or(true), "a single page has no buttons");
    let text = page["embeds"][0]["description"].as_str().unwrap_or("").to_string();
    assert!(text.contains("`a-draft` · 📝 draft · edited <t:"), "{text}");
    assert!(text.contains(&format!("`b-live` · 📤 <#{CH}> [jump](https://discord.com/channels/{}/{CH}/800000000000000123)", db.g)));
    assert!(page["embeds"][0]["title"].as_str().unwrap_or("").contains("Saved embeds (2/500)"));
    for n in 0..15 {
        db.create(&format!("c-{n:02}"), json!({ "content": "hello" })).await;
    }
    let c = run.go(run.slash().subcommand("list")).await;
    let first = &c[1].body;
    assert_eq!(first["components"].as_array().map(Vec::len), Some(1), "more than 15 rows: Prev/Stop/Next");
    assert_eq!(first["embeds"][0]["description"].as_str().unwrap_or("").split('\n').count(), 15);
    db.done().await;
}

async fn run_delete(db: &Db, yes: bool, delete_message: Option<bool>, name: &str) -> Vec<bot_core::interaction::RecordedCall> {
    db.t.responses.clear();
    let mut i = admin_slash(db.g).subcommand("delete").option("name", S, json!(name));
    if let Some(d) = delete_message {
        i = i.option("delete_message", B, json!(d));
    }
    let app = db.t.app.clone();
    let task = tokio::spawn(async move { route_interaction(&app, i.build()).await });
    for _ in 0..200 {
        if !db.t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = db.t.responses.calls().into_iter().find(|c| c.method == "reply").expect("prompt");
    let buttons = prompt.body["components"][0]["components"].as_array().cloned().unwrap_or_default();
    let id = s(&buttons[if yes { 0 } else { 1 }]["custom_id"]).to_string();
    let click = FakeInteraction::button(&id).guild(db.g).user(ADMIN).roles(&[ADMIN_ROLE]).message_id("1000000000000000100").build();
    route_interaction(&db.t, click).await;
    let _ = task.await;
    db.t.responses.calls()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_embed_delete_asks_first_then_removes_the_row_and_optionally_the_message() {
    let Some((db, fake)) = Db::with_guild("955014000000000015", bot_perms(), false).await else { return };
    db.t.install(registry());
    let member = member_of(&db.t, db.g, ADMIN);
    let row = db.create("to-delete", json!({ "content": "hello", "embed": { "title": "Bye" } })).await;
    let PublishOutcome::Posted { message, .. } = svc::publish_saved_embed(&db.t, &row, gid(db.g), cid(CH), Some(&member), None).await.expect("ran") else { panic!() };

    let c = run_delete(&db, false, Some(true), "to-delete").await;
    assert_eq!(c[0].method, "reply");
    assert!(c[0].ephemeral());
    assert!(c[0].embed_description().unwrap_or("").contains("published message will be deleted too"));
    assert!(db.get("to-delete").await.is_some(), "cancelled: nothing deleted");
    assert!(!c.iter().any(|x| x.method == "followUp"));

    let c = run_delete(&db, true, Some(true), "to-delete").await;
    let done = c.iter().find(|x| x.method == "followUp").expect("result");
    assert_eq!(done.embed_title(), Some("✅  Embed Deleted"));
    assert!(done.embed_description().unwrap_or("").contains("published message was deleted too"));
    assert!(!done.ephemeral(), "the result is public");
    assert!(db.get("to-delete").await.is_none());
    assert_eq!(fake.deleted.lock().map(|d| d.clone()).unwrap_or_default(), vec![message.id]);

    // Keep the message: the row goes, the post stays.
    let row2 = db.create("to-delete-2", json!({ "content": "hello" })).await;
    let PublishOutcome::Posted { message: m2, .. } = svc::publish_saved_embed(&db.t, &row2, gid(db.g), cid(CH), Some(&member), None).await.expect("ran") else { panic!() };
    let c = run_delete(&db, true, None, "to-delete-2").await;
    assert!(c[0].embed_description().unwrap_or("").contains("stays but will no longer be tracked"));
    assert!(db.get("to-delete-2").await.is_none());
    assert!(fake.messages.lock().map(|m| m.contains_key(&m2.id)).unwrap_or(false));

    // An admin who cannot see the channel it is published in may not delete it.
    let row3 = db.create("hidden", json!({ "content": "hello" })).await;
    svc::set_published(db.t.db(), &row3.id, Some(NO_VIEW), Some("800000000000044444")).await.expect("published");
    db.t.responses.clear();
    let i = FakeInteraction::slash("embed").guild(db.g).user(MOD).roles(&[ADMIN_ROLE, MEMBER_ROLE]).permissions(P::MANAGE_GUILD).channel(CH);
    route_interaction(&db.t, i.subcommand("delete").option("name", S, json!("hidden")).build()).await;
    let calls = db.t.responses.calls();
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["reply"]);
    assert_eq!(calls[0].embed_title(), Some("❌  No Access"));
    assert_eq!(calls[0].embed_description().unwrap_or(""), format!("**hidden** is published in <#{NO_VIEW}>. You need **View Channel** in <#{NO_VIEW}> to do that."));
    assert!(db.get("hidden").await.is_some());
    db.done().await;
}

#[tokio::test]
async fn db_welcome_channel_through_the_command() {
    let Some((db, fake)) = Db::with_guild("955014000000000016", bot_perms(), false).await else { return };
    db.t.install(registry());
    let run = Run { db: &db };
    let c = run.go(run.slash().subcommand("welcome-channel").option("channel", option_type::CHANNEL, json!(OPEN))).await;
    assert_eq!(last_title(&c), "✅  Welcome Channel Ready");
    let d = last_desc(&c);
    assert!(d.starts_with(&format!("Posted the greeting, rules, links and staff embeds in <#{OPEN}>.\nEdit them")), "{d}");
    assert!(d.contains("Created: `welcome-greeting`, `welcome-rules`, `welcome-links`, `welcome-team`."));
    assert!(d.contains("I created a permanent invite for the links embed."));
    assert!(d.ends_with("make it read-only if you want a MEE6-style welcome channel."));
    assert_eq!(fake.sent().len(), 4);
    let c = run.go(run.slash().subcommand("welcome-channel").option("channel", option_type::CHANNEL, json!(NO_BOT))).await;
    assert_eq!(last_title(&c), "❌  Missing Permissions");
    assert_eq!(last_desc(&c), format!("I need **Send Messages, Embed Links** in <#{NO_BOT}> to post there."));
    let c = run.go(run.slash().subcommand("welcome-channel").option("create", B, json!(false)).channel(VOICE)).await;
    assert_eq!(last_title(&c), "❌  Wrong Channel");
    db.done().await;
}

// ── 9. Raw SQL ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn db_raw_sql_runs() {
    let Some(db) = Db::new("955014000000000017").await else { return };
    let mut tx = db.t.db().begin().await.expect("tx");
    for (name, sql) in svc::RAW_SQL {
        let q = bot_db::sqlx::query(sql).bind(db.g);
        let q = match name {
            "searchSavedEmbeds:all" => q.bind(25_i64),
            "searchSavedEmbeds" => q.bind("%x%").bind(25_i64),
            _ => q,
        };
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!sql.contains("now()"), "{name}: bare now()");
    }
    tx.rollback().await.expect("rollback");
    db.done().await;
}

// ── 10. Cross-language ────────────────────────────────────────────────────────

const JS: &str = "rust/fixtures/gen/feat-embeds-js.mjs";

#[tokio::test]
async fn js_writes_rust_reads() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else { return };
    let g = "955014000000000020";
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let tpl = json!({
        "content": "Hey {user}, here are the rules!",
        "embed": { "title": "📜 Rules", "description": "**1.** Be nice.", "color": 5793266, "author": { "name": "{server}", "iconUrl": "{server.icon}" },
            "footer": { "text": "Thanks" }, "thumbnail": "{server.icon}", "fields": [{ "name": "Contact", "value": "Ask a moderator", "inline": true }], "timestamp": true },
        "buttons": [{ "label": "Website", "url": "https://example.com", "emoji": "🌐" }]
    });
    let js = run_node(JS, &["create", g, "js-rules", &tpl.to_string(), ADMIN]);
    assert!(js.get("error").is_none(), "{js}");
    let js_row = &js["row"];
    run_node(JS, &["publish", s(&js_row["id"]), CH, "800000000000000321"]);
    run_node(JS, &["create", g, "a_b", r#"{"content":"x"}"#, ADMIN]);
    run_node(JS, &["create", g, "axb", r#"{"content":"x"}"#, ADMIN]);

    let row = svc::get_saved_embed(t.db(), g, "js-rules").await.expect("query").expect("row");
    assert_eq!(row.id, s(&js_row["id"]));
    assert_eq!((row.channel_id.as_deref(), row.message_id.as_deref()), (Some(CH), Some("800000000000000321")));
    assert_eq!(normalize_template(&row.template), js["template"]);
    assert_eq!(serde_json::to_value(&row).ok().and_then(|v| v.get("createdAt").cloned()), Some(js_row["createdAt"].clone()), "same timestamp text");
    // The JS wrote the cache through: the Rust bot answers from Redis even after the row is gone.
    let cached = svc::get_saved_embed_template(&t, g, "JS Rules").await.expect("template");
    assert_eq!(cached, js["template"]);
    bot_db::sqlx::query(r#"DELETE FROM "SavedEmbed" WHERE "id" = $1"#).bind(&row.id).execute(t.db()).await.expect("deleted");
    assert_eq!(svc::get_saved_embed_template(&t, g, "js-rules").await, Some(js["template"].clone()), "from the JS cache entry");
    // Search and list read the JS rows the same way (`_` is a wildcard in both).
    let ours: Vec<String> = svc::search_saved_embeds(t.db(), g, "a_b", 25).await.into_iter().map(|r| r.name).collect();
    let theirs: Vec<String> = run_node(JS, &["search", g, "a_b", "25"]).as_array().into_iter().flatten().filter_map(|r| r["name"].as_str().map(String::from)).collect();
    assert_eq!(ours, theirs);
    assert_eq!(ours, vec!["a_b", "axb"]);
    let list = svc::list_saved_embeds(t.db(), g).await.expect("list");
    assert_eq!(json!(list), run_node(JS, &["list", g]));
    assert_eq!(json!(svc::count_saved_embeds(t.db(), g).await.expect("count")), run_node(JS, &["count", g]));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn rust_writes_js_reads() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else { return };
    let g = "955014000000000021";
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let row = svc::create_saved_embed(&t, g, "rust-rules", &json!({ "content": "from rust", "embed": { "title": "T", "color": 255 } }), ADMIN).await.expect("created");
    let saved = svc::mutate_saved_embed(&t, g, "rust-rules", |tpl| {
        let tpl = svc::add_field(tpl, "F", "V", true, None)?;
        let tpl = svc::add_button(&tpl, "Go", "https://a.b", Some("🎉"), None)?;
        svc::set_footer(&tpl, Some("foot"), &Change::Set("https://a.b/i.png".into()), Some(true))
    })
    .await
    .expect("mutated");
    svc::set_published(t.db(), &row.id, Some(CH), Some("800000000000000654")).await.expect("published");
    let ours = normalize_template(&saved.template);

    let back = run_node(JS, &["get", g, "rust-rules"]);
    assert!(back.get("error").is_none(), "{back}");
    assert_eq!(back["template"], ours, "the JS reads the Rust template as written");
    assert_eq!(back["row"]["channelId"], CH);
    assert_eq!(back["row"]["messageId"], "800000000000000654");
    let mine = svc::get_saved_embed(t.db(), g, "rust-rules").await.expect("query").expect("row");
    let mut ours_row = serde_json::to_value(&mine).unwrap_or(Value::Null);
    let mut theirs_row = back["row"].clone();
    for r in [&mut ours_row, &mut theirs_row] {
        r["template"] = normalize_template(&r["template"]);
    }
    assert_eq!(ours_row, theirs_row, "the row reads the same through Prisma and Rust");
    // The cache entry the Rust bot wrote through is what the JS reads.
    assert_eq!(run_node(JS, &["cache", g, "rust-rules"]), ours);
    bot_db::sqlx::query(r#"DELETE FROM "SavedEmbed" WHERE "id" = $1"#).bind(&row.id).execute(t.db()).await.expect("deleted");
    assert_eq!(run_node(JS, &["template", g, "rust-rules"]), ours, "the JS answers from the Rust cache entry");

    // JS edits a Rust row; the Rust bot reads the JS edit.
    let r2 = svc::create_saved_embed(&t, g, "shared", &json!({ "content": "x" }), ADMIN).await.expect("created");
    let js = run_node(JS, &["mutate", g, "shared", r#"{"title":"JS","value":"field"}"#]);
    assert!(js.get("error").is_none(), "{js}");
    let tpl = svc::get_saved_embed_template(&t, g, "shared").await.expect("template");
    assert_eq!(tpl["embed"]["fields"][0]["name"], "JS");
    assert_eq!(tpl, js["template"]);
    // A Rust delete leaves a miss marker the JS honours; the JS hook body drafts a Rust-published row.
    let r3 = svc::create_saved_embed(&t, g, "drafted", &json!({ "content": "x" }), ADMIN).await.expect("created");
    svc::set_published(t.db(), &r3.id, Some(CH), Some("800000000000000777")).await.expect("published");
    run_node(JS, &["ondelete", g, "800000000000000777"]);
    assert_eq!(svc::get_saved_embed(t.db(), g, "drafted").await.expect("q").and_then(|r| r.message_id), None);
    svc::delete_saved_embed_and_message(&t, gid(g), &r2, false).await.expect("deleted");
    assert_eq!(run_node(JS, &["cache", g, "shared"]), json!({ "__none": true }));
    assert_eq!(run_node(JS, &["template", g, "shared"]), Value::Null);
    t.reset_guilds(&[g]).await;
}
