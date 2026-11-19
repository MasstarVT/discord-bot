//! Port of `test/bot-joins.test.js`, the `/help` and `/module` parts of `test/registry.test.js`, `test/settings.test.js`
//! and the `/info` part of `test/mod-utilities.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: the payloads and the plugin against `commands.json` / `plugins.json`; `/help` and `/module` over the full
//! bot's 79 commands against the JS output (`rust/fixtures/golden/feat-general/cases.json`, from
//! `rust/fixtures/gen/feat-general-golden.mjs`); the `/settings` and `/info` builders; the inviter lookup against a fake
//! Discord REST; the commands through the router.
//! With `TEST_DATABASE_URL`: `BotJoin` and the settings in both cross-language directions, the custom-command group.

use std::sync::Arc;

use bot_core::command::{CommandDef, SlashCommand, option_type};
use bot_core::hooks::events::GuildReadyReason;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, load_golden, run_node};
use bot_core::AppConfig;
use bot_db::GuildSettings;
use feat_general::bot_join_service::{self as joins, Inviter};
use feat_general::commands::{help, info, module, owner, settings};
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions};

const OURS: [&str; 5] = ["help", "info", "module", "owner", "settings"];

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_general::register(&mut reg);
    reg
}

fn cases() -> Value {
    load_golden("feat-general/cases.json")
}

fn drop_nulls(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| !x.is_null());
        for x in o.values_mut() {
            drop_nulls(x);
        }
    }
}

/// An embed as compared: no timestamp, no nulls, `inline: false` where discord.js left it out.
fn norm(v: &Value) -> Value {
    let mut v = v.clone();
    drop_nulls(&mut v);
    if let Some(o) = v.as_object_mut() {
        o.remove("timestamp");
        o.remove("type");
        o.retain(|_, x| !x.is_null());
        if let Some(fields) = o.get_mut("fields").and_then(Value::as_array_mut) {
            for f in fields {
                if f.get("inline").is_none() {
                    f["inline"] = json!(false);
                }
            }
        }
        if let Some(fields) = o.get("fields").and_then(Value::as_array)
            && fields.is_empty()
        {
            o.remove("fields");
        }
    }
    v
}

fn ej(e: &serenity::builder::CreateEmbed) -> Value {
    norm(&help::embed_json(e))
}

fn level(n: &Value) -> PermissionLevel {
    PermissionLevel::from_u8(n.as_u64().unwrap_or(0) as u8).unwrap_or(PermissionLevel::Member)
}

/// Every command of the bot: ours, and the other 74 as `commands.json` describes them.
fn full_defs() -> Vec<CommandDef> {
    let reg = registry();
    let mut out = Vec::new();
    for c in load_golden("commands.json").as_array().cloned().unwrap_or_default() {
        let name = c["name"].as_str().unwrap_or_default();
        if let Some(def) = reg.command_named(name) {
            out.push(def.clone());
            continue;
        }
        let data = SlashCommand::from_payload(&c["payload"]).expect("payload parses");
        let category: &'static str = Box::leak(c["category"].as_str().unwrap_or_default().to_string().into_boxed_str());
        let module = c["module"].as_str().map(|m| ModuleKey::from_name(m).expect("module key"));
        let mut def = CommandDef::new(category, data, module, |_| async { Ok(()) }).gated(c["gated"].as_bool().unwrap_or(true)).permission_level(level(&c["permissionLevel"]));
        for (k, v) in c["subcommandPermissions"].as_object().cloned().unwrap_or_default() {
            def = def.subcommand_permission(k, level(&v));
        }
        if c["allowInDMs"].as_bool() == Some(true) {
            def = def.allow_in_dms();
        }
        if c["hasAutocomplete"].as_bool() == Some(true) {
            def = def.autocomplete(|_| async { Ok(()) });
        }
        out.push(def);
    }
    out
}

fn full_commands() -> Vec<Arc<CommandDef>> {
    full_defs().into_iter().map(Arc::new).collect()
}

fn base_json() -> Value {
    load_golden("mockdb/baseline.json")["GuildSettings"]["min"].clone()
}

fn row_of(v: &Value) -> GuildSettings {
    serde_json::from_value(v.clone()).expect("a GuildSettings row")
}

fn with_flags(on: bool) -> Value {
    let mut v = base_json();
    for (_, flag) in bot_config::module_flags() {
        v[flag] = json!(on);
    }
    v
}

/// The settings rows of the golden script, by name.
fn settings_named(name: &str) -> Option<GuildSettings> {
    match name {
        "none" => None,
        "base" => Some(row_of(&base_json())),
        "allOn" => Some(row_of(&with_flags(true))),
        "allOff" => Some(row_of(&with_flags(false))),
        "colored" => {
            let mut v = base_json();
            v["embedColor"] = json!(0x123456);
            Some(row_of(&v))
        }
        other => panic!("unknown settings {other}"),
    }
}

fn choices_json(choices: &[bot_core::interaction_helpers::Choice]) -> Value {
    Value::Array(
        bot_core::interaction_helpers::autocomplete_list(choices).into_iter().map(|(name, value)| json!({ "name": name, "value": value })).collect(),
    )
}

// ── Golden declarations ───────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &OURS);
    bot_core::testing::assert_plugin_matches_golden(&reg, "bot-joins");
}

#[test]
fn the_full_bot_registry_reads_back_as_commands_json() {
    let golden = load_golden("commands.json");
    let defs = full_defs();
    assert_eq!(defs.len(), 79);
    for (def, c) in defs.iter().zip(golden.as_array().into_iter().flatten()) {
        assert_eq!(def.payload(), c["payload"], "{}", def.name());
        let meta = def.metadata();
        for k in ["category", "module", "gated", "permissionLevel", "subcommandPermissions", "allowInDMs", "hasAutocomplete"] {
            assert_eq!(meta[k], c[k], "{} {k}", def.name());
        }
    }
}

// ── /help ─────────────────────────────────────────────────────────────────────

#[test]
fn help_groups_and_pages_equal_the_js_for_the_full_bot() {
    let cmds = full_commands();
    let all = cases();
    let groups_cases = all["help"]["groups"].as_array().cloned().unwrap_or_default();
    assert_eq!(groups_cases.len(), 20);
    for c in groups_cases {
        let s = settings_named(c["settings"].as_str().unwrap_or_default());
        let lvl = level(&c["level"]);
        let (groups, count) = help::build_groups(&cmds, lvl, s.as_ref());
        let at = format!("{} level {}", c["settings"], c["level"]);
        assert_eq!(serde_json::to_value(&groups).unwrap_or_default(), c["groups"], "{at}");
        assert_eq!(json!(count), c["count"], "{at}");
        let pages: Vec<Value> = help::build_pages(&groups, s.as_ref(), lvl, count).iter().map(ej).collect();
        let want: Vec<Value> = c["pages"].as_array().into_iter().flatten().map(norm).collect();
        assert_eq!(pages, want, "{at}");
    }
}

#[test]
fn help_command_details_equal_the_js_for_every_command() {
    let cmds = full_commands();
    let mut seen = 0;
    for c in cases()["help"]["show"].as_array().cloned().unwrap_or_default() {
        let s = settings_named(c["settings"].as_str().unwrap_or_default());
        let lvl = level(&c["level"]);
        let name = c["name"].as_str().unwrap_or_default();
        let wanted = help::wanted_name(&format!(" /{} ", name.to_uppercase()));
        assert_eq!(wanted, name);
        let embed = cmds.iter().find(|d| d.name() == wanted).and_then(|d| help::command_embed(d, lvl, s.as_ref()));
        match embed {
            Some(e) => assert_eq!(ej(&e), norm(&c["embed"]), "{name} at {}", c["level"]),
            None => assert_eq!(c["content"], json!("No such command."), "{name} at {}", c["level"]),
        }
        seen += 1;
    }
    assert_eq!(seen, 400);
}

#[test]
fn help_autocomplete_equals_the_js() {
    let cmds = full_commands();
    for c in cases()["help"]["autocomplete"].as_array().cloned().unwrap_or_default() {
        let choices = help::help_choices(&cmds, level(&c["level"]), None, c["focused"].as_str().unwrap_or_default());
        assert_eq!(choices_json(&choices), c["choices"], "{} {}", c["level"], c["focused"]);
    }
}

#[test]
fn help_visibility_ported_from_registry_test() {
    // /help lists only what the caller may use and never hides a command because settings are missing.
    let command = |name: &str, module: Option<ModuleKey>| CommandDef::new("utility", SlashCommand::new(name, "d"), module, |_| async { Ok(()) });
    let cmds: Vec<Arc<CommandDef>> = vec![
        Arc::new(command("hello", None)),
        Arc::new(command("slots", Some(ModuleKey::Games))),
        Arc::new(command("ban", None).permission_level(PermissionLevel::Moderator)),
        Arc::new(command("config", None).permission_level(PermissionLevel::Admin)),
        Arc::new(command("mixed", None).subcommand_permission("setup", PermissionLevel::Admin)),
    ];
    let names: Vec<Value> = help::help_choices(&cmds, PermissionLevel::Member, None, "").into_iter().map(|c| c.value).collect();
    assert_eq!(names, vec![json!("hello"), json!("mixed"), json!("slots")]);
    // A config command of a module that is off is shown to admins only, in the setup group.
    let off = settings_named("allOff");
    let cfg = command("gamecfg", Some(ModuleKey::Games)).gated(false).permission_level(PermissionLevel::Admin);
    assert_eq!(help::visibility(&cfg, PermissionLevel::Admin, off.as_ref()), help::Visibility::Setup);
    assert_eq!(help::visibility(&cfg, PermissionLevel::Moderator, off.as_ref()), help::Visibility::Hide);
    assert_eq!(help::visibility(&cmds[1], PermissionLevel::Owner, off.as_ref()), help::Visibility::Hide);
    assert_eq!(help::visibility(&cmds[1], PermissionLevel::Member, None), help::Visibility::Show);
}

#[test]
fn help_custom_command_helpers_equal_the_js() {
    let all = cases();
    for c in all["helpers"]["prefixes"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(help::custom_prefix_of(&c["p"])), c["out"], "prefix {}", c["p"]);
    }
    for c in all["helpers"]["lines"].as_array().cloned().unwrap_or_default() {
        let line = help::custom_command_line(c["name"].as_str().unwrap_or_default(), c["description"].as_str(), c["prefix"].as_str());
        assert_eq!(json!(line), c["out"]);
    }
    // registry.test.js: lines by prefix, off, invalid prefixes, viewers.
    let tags = vec![
        help::HelpTag { name: "rules".into(), description: Some("Server rules".into()), ..Default::default() },
        help::HelpTag { name: "faq".into(), description: None, ..Default::default() },
    ];
    let mut on = base_json();
    on["customCommandsEnabled"] = json!(true);
    let lines = |prefix: &str| {
        let mut v = on.clone();
        v["customPrefix"] = json!(prefix);
        help::custom_group_from(&tags, &row_of(&v), None).map(|g| g.lines)
    };
    assert_eq!(lines("!"), Some(vec!["`!rules` — Server rules".to_string(), "`!faq` — custom command".to_string()]));
    assert_eq!(lines(">>").map(|l| l[0].clone()), Some("`>>rules` — Server rules".to_string()));
    assert_eq!(lines(""), Some(vec!["`/tag use name:rules` — Server rules".to_string(), "`/tag use name:faq` — custom command".to_string()]));
    assert_eq!(lines("two words").map(|l| l[0].clone()), Some("`!rules` — Server rules".to_string()));
    let roles = vec![
        help::HelpTag { name: "rules".into(), ..Default::default() },
        help::HelpTag { name: "staff".into(), allow_role_ids: vec!["r-staff".into()], ..Default::default() },
    ];
    let row = row_of(&on);
    assert_eq!(help::custom_group_from(&roles, &row, Some(&[])).map(|g| g.lines), Some(vec!["`!rules` — custom command".to_string()]));
    assert_eq!(help::custom_group_from(&roles, &row, Some(&["r-staff".to_string()])).map(|g| g.lines.len()), Some(2));
    assert_eq!(help::custom_group_from(&roles, &row, None).map(|g| g.lines.len()), Some(2));
    assert_eq!(help::custom_group_from(&[], &row, None), None);
    // tagRolesAllow (custom-commands.test.js): deny wins, an empty allow list allows, no member matches nothing.
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(help::tag_roles_allow(&[], &[], None));
    assert!(!help::tag_roles_allow(&s(&["a"]), &[], None));
    assert!(!help::tag_roles_allow(&s(&["a"]), &s(&["b"]), Some(&s(&["a", "b"]))));
    assert!(help::tag_roles_allow(&s(&["a", "c"]), &s(&["b"]), Some(&s(&["c"]))));
    assert_eq!(help::custom_command_line("x", Some(&"d".repeat(300)), None).encode_utf16().count(), "`/tag use name:x` — ".encode_utf16().count() + 100);
}

#[tokio::test]
async fn help_through_the_router_lists_the_full_bot() {
    const G: &str = "930000000000000001";
    let t = TestApp::new().with_guilds([G]);
    let mut reg = Registry::new();
    for def in full_defs() {
        reg.command(def);
    }
    t.install(reg);
    let mut row = row_of(&base_json());
    row.guild_id = G.into();
    t.settings_store.put(row);
    route_interaction(&t, FakeInteraction::slash("help").guild(G).permissions(Permissions::MANAGE_GUILD).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].method, "deferReply");
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].method, "editReply");
    let want = cases()["help"]["groups"].as_array().cloned().unwrap_or_default().into_iter().find(|c| c["settings"] == "base" && c["level"] == 2).unwrap_or_default();
    let mut page = norm(&calls[1].body["embeds"][0]);
    let mut first = norm(&want["pages"][0]);
    // The paginator adds the page indicator to the footer.
    page.as_object_mut().map(|o| o.remove("footer"));
    first.as_object_mut().map(|o| o.remove("footer"));
    assert_eq!(page, first);
    assert_eq!(calls[1].body["embeds"][0]["footer"]["text"], json!("74 commands available to you • Page 1/2"));

    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("help").guild(G).option("command", option_type::STRING, json!("/MODULE")).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].content(), Some("No such command."), "a member cannot see /module");
    t.responses.clear();
    route_interaction(
        &t,
        FakeInteraction::slash("help").guild(G).permissions(Permissions::MANAGE_GUILD).option("command", option_type::STRING, json!("module")).build(),
    )
    .await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("/module"));

    t.responses.clear();
    route_interaction(&t, FakeInteraction::autocomplete("help").guild(G).focused("command", "ban").build()).await;
    assert_eq!(t.responses.calls()[0].choices(), Vec::<Value>::new(), "/ban is for moderators");
    t.responses.clear();
    route_interaction(&t, FakeInteraction::autocomplete("help").guild(G).permissions(Permissions::BAN_MEMBERS).focused("command", "ban").build()).await;
    assert_eq!(t.responses.calls()[0].choices(), vec![json!({ "name": "/ban", "value": "ban" }), json!({ "name": "/softban", "value": "softban" }), json!({ "name": "/unban", "value": "unban" })]);
}

// ── /module ───────────────────────────────────────────────────────────────────

#[test]
fn module_list_and_autocomplete_equal_the_js() {
    let all = cases();
    for c in all["module"]["list"].as_array().cloned().unwrap_or_default() {
        let s = settings_named(c["settings"].as_str().unwrap_or_default()).expect("a row");
        assert_eq!(ej(&module::module_list_embed(&s)), norm(&c["embed"]), "{}", c["settings"]);
    }
    for c in all["module"]["autocomplete"].as_array().cloned().unwrap_or_default() {
        let s = settings_named(c["settings"].as_str().unwrap_or_default());
        let choices = module::module_choices(s.as_ref(), c["sub"].as_str(), c["focused"].as_str().unwrap_or_default());
        assert_eq!(choices_json(&choices), c["choices"], "{} {} {}", c["settings"], c["sub"], c["focused"]);
    }
    for c in all["module"]["resolve"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(module::resolve_module_key(c["input"].as_str().unwrap_or_default())), c["out"], "{}", c["input"]);
    }
    // registry.test.js: with no settings, the first 25 keys in MODULE_NAMES order and no state suffix.
    let none = module::module_choices(None, Some("enable"), "");
    assert_eq!(none.len(), 25);
    assert_eq!(none.iter().map(|c| c.value.clone()).collect::<Vec<_>>(), bot_config::MODULE_NAMES[..25].iter().map(|k| json!(k)).collect::<Vec<_>>());
    assert_eq!(module::module_choices(None, Some("disable"), "AUDIT").iter().map(|c| c.value.clone()).collect::<Vec<_>>(), vec![json!("logging")]);
}

/// A guild the ROLE options resolve from (Discord always sends the resolved role).
fn rg(id: &str) -> FakeGuild {
    FakeGuild::new(id, "930000000000000099")
}

fn admin_slash(name: &str, sub: &str, guild: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(guild).permissions(Permissions::MANAGE_GUILD).subcommand(sub)
}

#[tokio::test]
async fn module_enable_disable_and_list_through_the_router() {
    const G: &str = "930000000000000002";
    let t = TestApp::new().with_guilds([G]);
    t.install(registry());
    t.cache_backend.put_raw(&format!("voice:start:{G}:1"), "1", None);
    route_interaction(&t, admin_slash("module", "disable", G).option("module", option_type::STRING, json!(" Levels ")).build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!calls[0].ephemeral(), "public, like the JS deferReply()");
    assert_eq!(calls[1].embed_title(), Some("✅  Module Disabled"));
    assert_eq!(calls[1].embed_description(), Some("**Levels** disabled. Its data is kept."));
    assert_eq!(t.settings_store.row(G).map(|r| r.xp_enabled), Some(false));
    assert!(t.cache_backend.raw(&format!("voice:start:{G}:1")).is_none(), "open voice sessions are dropped");

    t.responses.clear();
    route_interaction(&t, admin_slash("module", "enable", G).option("module", option_type::STRING, json!("polls")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("**Polls** enabled. Configure it with `/poll`."));
    t.responses.clear();
    route_interaction(&t, admin_slash("module", "enable", G).option("module", option_type::STRING, json!("nope")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Unknown Module"));
    t.responses.clear();
    route_interaction(&t, admin_slash("module", "list", G).build()).await;
    let body = &t.responses.calls()[1].body["embeds"][0];
    assert_eq!(body["title"], json!("🧩 Server Modules"));
    assert!(body["fields"][0]["value"].as_str().unwrap_or_default().contains("📊 **Polls** — ✅ On · `/poll`"));

    // Members are refused by the router (ADMIN).
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("module").guild(G).subcommand("list").build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);

    // The settings store is down: list says so, a toggle says Update Failed.
    t.settings_store.set_available(false);
    t.settings().invalidate_settings(G).await;
    t.responses.clear();
    route_interaction(&t, admin_slash("module", "list", G).build()).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title().map(String::from)), Some("❌  Temporarily Unavailable".into()));
}

// ── /owner ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn owner_names_the_owner_or_says_none_is_set() {
    const G: &str = "930000000000000003";
    let t = TestApp::new().with_guilds([G]);
    t.install(registry());
    route_interaction(&t, FakeInteraction::slash("owner").guild(G).build()).await;
    let call = &t.responses.calls()[0];
    assert_eq!(call.method, "reply");
    assert!(call.ephemeral());
    assert_eq!(call.embed_title(), Some("❌  No Owner Set"));
    assert_eq!(call.embed_description(), Some("This bot has no owner configured (`BOT_OWNER_ID`)."));

    let owned = TestApp::with_config(AppConfig { bot_owner_id: Some(" 123456789012345678 ".into()), ..Default::default() }).with_guilds([G]);
    owned.install(registry());
    route_interaction(&owned, FakeInteraction::slash("owner").guild(G).build()).await;
    let call = &owned.responses.calls()[0];
    assert!(!call.ephemeral());
    assert_eq!(call.content(), Some("👑 <@123456789012345678> is the owner of this bot."));
    assert_eq!(call.body["allowed_mentions"]["users"], json!(["123456789012345678"]));
    assert_eq!(owner::owner_id_of(Some("12345")), None);
    assert_eq!(owner::owner_id_of(Some("abcdefghijklmnopqr")), None);
}

// ── /settings ─────────────────────────────────────────────────────────────────

#[test]
fn settings_parsers_equal_the_js() {
    let all = cases();
    for c in all["settings"]["parseColor"].as_array().cloned().unwrap_or_default() {
        let got = match settings::parse_color_input(c["input"].as_str().unwrap_or_default()) {
            None => json!({ "ok": false }),
            Some(r) => json!({ "ok": true, "color": r.color, "adjusted": r.adjusted }),
        };
        assert_eq!(got, c["out"], "{}", c["input"]);
    }
    for c in all["settings"]["suggest"].as_array().cloned().unwrap_or_default() {
        let got = settings::suggest_colors(c["q"].as_str().unwrap_or_default(), c["current"].as_i64());
        let got: Vec<Value> = got.iter().map(|ch| json!({ "name": ch.name, "value": ch.value })).collect();
        assert_eq!(Value::Array(got), c["out"], "{} {}", c["q"], c["current"]);
    }
    for c in all["settings"]["prefix"].as_array().cloned().unwrap_or_default() {
        let got = match settings::validate_prefix(c["input"].as_str().unwrap_or_default()) {
            Ok(p) if p.is_empty() => json!({ "ok": true, "prefix": "", "off": true }),
            Ok(p) => json!({ "ok": true, "prefix": p }),
            Err(e) => json!({ "ok": false, "code": e.code() }),
        };
        assert_eq!(got, c["out"], "{:?}", c["input"]);
    }
}

#[test]
fn settings_overview_equals_the_js() {
    let all = cases();
    let now = all["settings"]["now"].as_i64().unwrap_or_default();
    let base = base_json();
    let r1 = "800000000000000001";
    let r2 = "800000000000000002";
    let row = |name: &str| -> Value {
        let mut v = base.clone();
        match name {
            "base" => {}
            "custom" => {
                for (k, x) in [
                    ("timezone", json!("America/New_York")),
                    ("embedColor", json!(0xff0000)),
                    ("customPrefix", json!("")),
                    ("customCommandsEnabled", json!(true)),
                    ("modRoleIds", json!([r1, r2, r1])),
                    ("urbanNsfwOnly", json!(false)),
                ] {
                    v[k] = x;
                }
            }
            "invalidTz" => {
                v["timezone"] = json!("Mars/Base`x");
                v["customPrefix"] = json!("?");
                v["modRoleIds"] = json!([r2]);
            }
            "allOn" => {
                v = with_flags(true);
                v["modRoleIds"] = json!([r1]);
            }
            "allOff" => v = with_flags(false),
            other => panic!("{other}"),
        }
        v
    };
    let overview = all["settings"]["overview"].as_array().cloned().unwrap_or_default();
    assert_eq!(overview.len(), 10);
    for c in overview {
        let s = row_of(&row(c["settings"].as_str().unwrap_or_default()));
        let roles: Vec<String> = c["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
        let embed = settings::build_overview_embed(&s, c["guildName"].as_str(), &|id| roles.iter().any(|r| r == id), now);
        assert_eq!(ej(&embed), norm(&c["embed"]), "{} {}", c["settings"], c["guildName"]);
    }
}

#[test]
fn settings_timezone_errors_and_helpers() {
    // test/settings.test.js: validation errors echo the input safely.
    assert_eq!(settings::shown("`a\\b`"), "ab");
    assert_eq!(settings::shown("   "), "(empty)");
    assert_eq!(settings::shown(&"x".repeat(80)).encode_utf16().count(), 64);
    let (title, text) = settings::tz_error(bot_util::timezones::ZoneErrorCode::Unknown, "Mars/`Base`");
    assert_eq!(title, "Unknown Timezone");
    assert!(text.starts_with("`Mars/Base` is not a timezone I know."));
    assert_eq!(settings::tz_error(bot_util::timezones::ZoneErrorCode::Offset, "+5").0, "Pick a City");
    assert_eq!(settings::tz_error(bot_util::timezones::ZoneErrorCode::Abbreviation, "EST").0, "Ambiguous Timezone");
    assert_eq!(settings::normalize_time_zone("europe/london"), Ok("Europe/London".into()));
    // The zone box of timezone-role add: the role name's zone first.
    let now = 1_782_000_000_000;
    let choices = settings::suggest_timezone_role_zones("", Some("CST"), now);
    assert_eq!(choices[0].value, json!("America/Chicago"));
    assert!(choices[0].name.as_deref().unwrap_or_default().ends_with(" · from the role name"));
    assert_eq!(choices.iter().filter(|c| c.value == json!("America/Chicago")).count(), 1);
    assert_ne!(settings::suggest_timezone_role_zones("tok", Some("CST"), now)[0].value, json!("America/Chicago"));
    // Autocomplete: nothing for other options.
    assert!(settings::settings_choices(None, Some("prefix"), "prefix", "", None, None, now).is_empty());
    assert!(settings::settings_choices(Some("timezone-role"), Some("remove"), "role", "", None, None, now).is_empty());
    assert_eq!(settings::settings_choices(None, Some("color"), "value", "", None, None, now)[0].value, json!("reset"));
    // The timezone-role list: deleted roles named, the tail with the server zone.
    let mut v = base_json();
    v["timezoneRoles"] = json!([{ "roleId": "800000000000000001", "zone": "America/Chicago" }, { "roleId": "800000000000000009", "zone": "Asia/Tokyo" }]);
    let e = help::embed_json(&settings::timezone_role_list_embed(&row_of(&v), &|id| id == "800000000000000001", now));
    let d = e["description"].as_str().unwrap_or_default();
    assert_eq!(e["title"], json!("ℹ️  Timezone Roles (2/25)"));
    assert!(d.starts_with("• <@&800000000000000001> → `America/Chicago` (UTC-05:00) · "));
    assert!(d.contains("• deleted role `800000000000000009` → `Asia/Tokyo` (UTC+09:00)"));
    assert!(d.ends_with("Members without one of these roles use `UTC` (the server timezone) unless they saved their own with `/timezone set`. When a member has several, the highest role wins."));
}

#[tokio::test]
async fn settings_validation_errors_are_ephemeral_and_sent_before_any_defer() {
    const G: &str = "930000000000000004";
    let t = TestApp::new().with_guilds([G]);
    t.install(registry());
    let cases: Vec<(FakeInteraction, &str)> = vec![
        (admin_slash("settings", "timezone", G).option("zone", option_type::STRING, json!("+05:30")), "❌  Pick a City"),
        (admin_slash("settings", "timezone", G).option("zone", option_type::STRING, json!("EST")), "❌  Ambiguous Timezone"),
        (admin_slash("settings", "color", G).option("value", option_type::STRING, json!("`nope`")), "❌  Invalid Colour"),
        (admin_slash("settings", "prefix", G).option("prefix", option_type::STRING, json!("a b")), "❌  Invalid Prefix"),
        (
            FakeInteraction::slash("settings").guild(G).permissions(Permissions::MANAGE_GUILD).group("timezone-role", "add").option("role", option_type::ROLE, json!(G)).resolve_from(&rg(G)),
            "❌  Pick a Role",
        ),
    ];
    for (i, want) in cases {
        t.responses.clear();
        route_interaction(&t, i.build()).await;
        let calls = t.responses.calls();
        assert_eq!(t.responses.names(), vec!["reply"], "{want}");
        assert!(calls[0].ephemeral());
        assert_eq!(calls[0].embed_title(), Some(want));
    }
    let row = t.settings_store.row(G);
    assert!(row.is_none_or(|r| r.timezone == "UTC" && r.custom_prefix == "!" && r.embed_color.is_none()), "nothing was written");
    let text = t.responses.calls()[0].embed_description().map(String::from);
    assert_eq!(text.as_deref(), Some("Pick a timezone role such as @CST, not @everyone. To set the timezone for everyone else, use `/settings timezone`."));
}

#[tokio::test]
async fn settings_writes_answer_no_change_and_show_the_result() {
    const G: &str = "930000000000000005";
    let t = TestApp::new().with_guilds([G]);
    t.install(registry());
    let run = |i: FakeInteraction| {
        let t = &t;
        async move {
            t.responses.clear();
            route_interaction(t, i.build()).await;
            t.responses.calls()
        }
    };
    let calls = run(admin_slash("settings", "color", G).option("value", option_type::STRING, json!("#000"))).await;
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"]);
    assert_eq!(calls[1].embed_title(), Some("✅  Embed Colour Updated"));
    assert!(calls[1].embed_description().unwrap_or_default().contains("**#010101** was saved instead"));
    assert_eq!(calls[1].body["embeds"][1]["color"], json!(0x010101), "the preview shows the new colour");
    assert_eq!(calls[1].body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(t.settings_store.row(G).and_then(|r| r.embed_color), Some(0x010101));
    let calls = run(admin_slash("settings", "color", G).option("value", option_type::STRING, json!("black"))).await;
    assert_eq!(calls[1].embed_title(), Some("ℹ️  No Change"));
    assert_eq!(calls[1].embed_description(), Some("The embed colour is already **#010101**."));
    let calls = run(admin_slash("settings", "color", G).option("value", option_type::STRING, json!("reset"))).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Embed Colour Reset"));
    assert_eq!(t.settings_store.row(G).and_then(|r| r.embed_color), None);

    let calls = run(admin_slash("settings", "prefix", G).option("prefix", option_type::STRING, json!("OFF"))).await;
    assert_eq!(calls[1].embed_description(), Some("Prefix commands are off. /tag use still works."));
    assert_eq!(t.settings_store.row(G).map(|r| r.custom_prefix), Some(String::new()));
    let calls = run(admin_slash("settings", "prefix", G).option("prefix", option_type::STRING, json!("?"))).await;
    assert!(calls[1].embed_description().unwrap_or_default().ends_with("Enable it with `/module enable customcommands`."));

    let calls = run(admin_slash("settings", "timezone", G).option("zone", option_type::STRING, json!("asia/tokyo"))).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Timezone Updated"));
    let d = calls[1].embed_description().unwrap_or_default().to_string();
    assert!(d.starts_with("The server timezone is now **Asia/Tokyo** (UTC+09:00).\nLocal time there: **"));
    assert!(d.contains("Previously: `UTC`."));
    let calls = run(admin_slash("settings", "timezone", G).option("zone", option_type::STRING, json!("Asia/Tokyo"))).await;
    assert_eq!(calls[1].embed_description(), Some("The server timezone is already **Asia/Tokyo** (UTC+09:00)."));

    let calls = run(admin_slash("settings", "urban", G).option("nsfw-only", option_type::BOOLEAN, json!(false))).await;
    assert!(calls[1].embed_description().unwrap_or_default().starts_with("/search urban now works in every channel."));
    let calls = run(admin_slash("settings", "urban", G).option("nsfw-only", option_type::BOOLEAN, json!(false))).await;
    assert_eq!(calls[1].embed_description(), Some("/search urban already works in every channel."));

    let role = "800000000000000077";
    let tz_add = |zone: Option<&str>| {
        let mut i = FakeInteraction::slash("settings").guild(G).permissions(Permissions::MANAGE_GUILD).group("timezone-role", "add").option("role", option_type::ROLE, json!(role));
        if let Some(z) = zone {
            i = i.option("zone", option_type::STRING, json!(z));
        }
        i.resolve_from(&rg(G))
    };
    let calls = run(tz_add(None)).await;
    assert_eq!(calls[0].embed_title(), Some("❌  Which Timezone?"), "a default role name names no zone");
    let calls = run(tz_add(Some("CST"))).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Timezone Role Saved"));
    assert!(calls[1].embed_description().unwrap_or_default().starts_with(&format!("Members with <@&{role}> get **America/Chicago (UTC-")));
    let saved = t.settings_store.row(G).and_then(|r| r.timezone_roles);
    assert_eq!(saved, Some(json!([{ "roleId": role, "zone": "America/Chicago" }])));
    let calls = run(tz_add(Some("Europe/Paris"))).await;
    assert!(calls[1].embed_description().unwrap_or_default().contains("Previously: `America/Chicago`."));
    let calls = run(FakeInteraction::slash("settings").guild(G).permissions(Permissions::MANAGE_GUILD).group("timezone-role", "list")).await;
    assert_eq!(calls[1].embed_title(), Some("ℹ️  Timezone Roles (1/25)"));
    let remove = || FakeInteraction::slash("settings").guild(G).permissions(Permissions::MANAGE_GUILD).group("timezone-role", "remove").option("role", option_type::ROLE, json!(role)).resolve_from(&rg(G));
    let calls = run(remove()).await;
    assert_eq!(calls[1].embed_title(), Some("✅  Timezone Role Removed"));
    let calls = run(remove()).await;
    assert_eq!(calls[1].embed_title(), Some("❌  Not Saved"));
    assert_eq!(calls[1].embed_description(), Some(format!("<@&{role}> is not a timezone role.").as_str()));

    let calls = run(admin_slash("settings", "show", G)).await;
    let e = &calls[1].body["embeds"][0];
    assert_eq!(e["title"], json!("⚙️ Server Settings"));
    assert_eq!(e["description"], json!("General settings for **this server**."));
    assert_eq!(e["fields"][2]["value"], json!("`?`\nCustom commands are off · `/module enable customcommands`"));

    // Autocomplete reads the memo only.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::autocomplete("settings").guild(G).permissions(Permissions::MANAGE_GUILD).subcommand("timezone").focused("zone", "").build()).await;
    let choices = t.responses.calls()[0].choices();
    assert_eq!(choices[0], json!({ "name": "Asia/Tokyo (UTC+09:00) · current", "value": "Asia/Tokyo" }));

    // The database is down: show says Settings Unavailable, a write says Could Not Save.
    t.settings_store.set_available(false);
    t.settings().invalidate_settings(G).await;
    let calls = run(admin_slash("settings", "show", G)).await;
    assert_eq!(calls[1].embed_title(), Some("❌  Settings Unavailable"));
    let calls = run(admin_slash("settings", "color", G).option("value", option_type::STRING, json!("red"))).await;
    assert_eq!(calls.last().and_then(|c| c.embed_title().map(String::from)), Some("❌  Could Not Save".into()));
}

#[tokio::test]
async fn settings_timezone_role_limit_and_live_roles() {
    const G: &str = "930000000000000006";
    const OWNER: &str = "930000000000000099";
    let mut guild = FakeGuild::new(G, OWNER).member(OWNER, &[]);
    for i in 0..26 {
        guild = guild.role(&format!("8100000000000000{i:02}"), 1, Permissions::empty());
    }
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild]);
    t.install(registry());
    let links: Vec<Value> = (0..25).map(|i| json!({ "roleId": format!("8100000000000000{i:02}"), "zone": "UTC" })).chain([json!({ "roleId": "819999999999999999", "zone": "UTC" })]).collect();
    let mut v = base_json();
    v["guildId"] = json!(G);
    v["timezoneRoles"] = Value::Array(links);
    t.settings_store.put(row_of(&v));
    let add = FakeInteraction::slash("settings").guild(G).user(OWNER).group("timezone-role", "add").option("role", option_type::ROLE, json!("810000000000000025")).option("zone", option_type::STRING, json!("UTC"));
    route_interaction(&t, add.build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("❌  Not Saved"));
    assert_eq!(calls[1].embed_description(), Some("You can have at most 25 timezone roles. Remove one first."));
    // Replacing a linked role is always allowed; the link to a deleted role is dropped on the write.
    t.responses.clear();
    let replace = FakeInteraction::slash("settings").guild(G).user(OWNER).group("timezone-role", "add").option("role", option_type::ROLE, json!("810000000000000000")).option("zone", option_type::STRING, json!("Asia/Tokyo"));
    route_interaction(&t, replace.build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Timezone Role Saved"));
    let saved = t.settings_store.row(G).and_then(|r| r.timezone_roles).unwrap_or_default();
    assert_eq!(saved.as_array().map(Vec::len), Some(25));
    assert_eq!(saved[24], json!({ "roleId": "810000000000000000", "zone": "Asia/Tokyo" }));
    assert!(!saved.to_string().contains("819999999999999999"));
}

// ── /info ─────────────────────────────────────────────────────────────────────

fn perms_of(names: &Value) -> Permissions {
    names.as_array().into_iter().flatten().filter_map(|n| n.as_str().and_then(bot_core::permissions::flag)).fold(Permissions::empty(), |a, b| a | b)
}

#[test]
fn info_helpers_equal_the_js() {
    let all = cases();
    let i = &all["info"];
    for c in i["keyPermissions"].as_array().cloned().unwrap_or_default() {
        let bits: u64 = c["bits"].as_str().unwrap_or("0").parse().unwrap_or(0);
        assert_eq!(json!(info::key_permission_names(Permissions::from_bits_retain(bits))), c["out"], "{}", c["bits"]);
    }
    for c in i["roleMentions"].as_array().cloned().unwrap_or_default() {
        let roles: Vec<(String, i64)> =
            c["roles"].as_array().into_iter().flatten().map(|r| (r["id"].as_str().unwrap_or_default().to_string(), r["position"].as_i64().unwrap_or(0))).collect();
        let out = info::format_role_mentions(&roles, c["guildId"].as_str().unwrap_or_default(), c["max"].as_u64().unwrap_or(1024) as usize);
        assert_eq!(json!(out), c["out"]);
    }
    for c in i["features"].as_array().cloned().unwrap_or_default() {
        assert_eq!(json!(info::is_public_feature(c["f"].as_str().unwrap_or_default())), c["out"], "{}", c["f"]);
    }
    let ch = info::count_channels(&[0, 5, 2, 13, 15, 16, 4, 11, 12, 10, 14]);
    assert_eq!(json!({ "text": ch.text, "voice": ch.voice, "forum": ch.forum, "category": ch.category }), i["channels"]);
    // Every key permission exists in discord.js.
    assert!(info::KEY_PERMISSIONS.iter().all(|k| bot_core::permissions::is_flag(k)));
}

#[test]
fn info_embeds_equal_the_js() {
    let all = cases();
    let i = &all["info"];
    for c in i["server"].as_array().cloned().unwrap_or_default() {
        let v = &c["view"];
        let s = |k: &str| v[k].as_str().map(String::from);
        let view = info::ServerView {
            id: s("id").unwrap_or_default(),
            name: s("name").unwrap_or_default(),
            description: s("description"),
            owner_id: s("ownerId").unwrap_or_default(),
            created_ms: v["createdMs"].as_i64().unwrap_or(0),
            icon_url: s("iconUrl"),
            banner_url: s("bannerUrl"),
            channel_types: v["channelTypes"].as_array().into_iter().flatten().map(|t| t.as_u64().unwrap_or(0) as u8).collect(),
            emojis_animated: v["emojisAnimated"].as_array().into_iter().flatten().map(|a| a.as_bool().unwrap_or(false)).collect(),
            role_count: v["roleCount"].as_u64().unwrap_or(0) as usize,
            premium_tier: v["premiumTier"].as_u64().unwrap_or(0) as u8,
            premium_subscription_count: v["premiumSubscriptionCount"].as_u64(),
            verification_level: v["verificationLevel"].as_u64().unwrap_or(0) as u8,
            explicit_content_filter: v["explicitContentFilter"].as_u64().unwrap_or(0) as u8,
            mfa_level: v["mfaLevel"].as_u64().unwrap_or(0) as u8,
            preferred_locale: s("preferredLocale").unwrap_or_default(),
            vanity_url_code: s("vanityUrlCode"),
            features: v["features"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect(),
            member_count: v["memberCount"].as_u64().unwrap_or(0),
        };
        let counts = info::GuildCounts {
            at: 0,
            members: c["counts"]["members"].as_f64().unwrap_or(0.0),
            online: c["counts"]["online"].as_f64(),
            stickers: c["counts"]["stickers"].as_u64().map(|n| n as usize),
        };
        let st = settings_named(c["settings"].as_str().unwrap_or_default());
        assert_eq!(ej(&info::build_server_embed(&view, &counts, st.as_ref())), norm(&c["embed"]), "{}", view.id);
    }
    for c in i["role"].as_array().cloned().unwrap_or_default() {
        let v = &c["view"];
        let t = &v["tags"];
        let view = info::RoleView {
            id: v["id"].as_str().unwrap_or_default().into(),
            name: v["name"].as_str().unwrap_or_default().into(),
            unicode_emoji: v["unicodeEmoji"].as_str().map(String::from),
            primary_color: v["primaryColor"].as_u64().unwrap_or(0) as u32,
            secondary_color: v["secondaryColor"].as_u64().map(|n| n as u32),
            icon_url: v["iconUrl"].as_str().map(String::from),
            position: v["position"].as_i64().unwrap_or(0),
            created_ms: v["createdMs"].as_i64().unwrap_or(0),
            hoist: v["hoist"].as_bool().unwrap_or(false),
            mentionable: v["mentionable"].as_bool().unwrap_or(false),
            managed: v["managed"].as_bool().unwrap_or(false),
            tags: t.is_object().then(|| info::RoleTagsView {
                bot_id: t["botId"].as_str().map(String::from),
                premium_subscriber_role: t["premiumSubscriberRole"].as_bool().unwrap_or(false),
                subscription_listing_id: t["subscriptionListingId"].as_str().map(String::from),
                guild_connections: t["guildConnections"].as_bool().unwrap_or(false),
                integration_id: t["integrationId"].as_str().map(String::from),
            }),
            permissions: perms_of(&v["permissions"]),
        };
        let counts = vec![("31".to_string(), 12.0), ("33".to_string(), 1_234_567.0)];
        let fetched = if c["mode"] == "fail" { Err(()) } else { Ok(counts.as_slice()) };
        let text = info::role_member_text(view.id == "9100", c["memberCount"].as_u64().unwrap_or(0), fetched, &view.id, c["cachedHolders"].as_u64().unwrap_or(0) as usize);
        let embed = info::build_role_embed("9100", c["roleCount"].as_u64().unwrap_or(0) as usize, &view, &text, None);
        assert_eq!(ej(&embed), norm(&c["embed"]), "{}", view.id);
    }
    for c in i["user"].as_array().cloned().unwrap_or_default() {
        let u = &c["user"];
        let user = info::UserView {
            id: u["id"].as_str().unwrap_or_default().into(),
            username: u["username"].as_str().unwrap_or_default().into(),
            global_name: u["globalName"].as_str().map(String::from),
            bot: u["bot"].as_bool().unwrap_or(false),
            created_ms: u["createdMs"].as_i64().unwrap_or(0),
            avatar_url: u["avatarUrl"].as_str().unwrap_or_default().into(),
            flags: c["flagBits"].as_str().and_then(|b| b.parse().ok()),
        };
        let member = c["member"].as_object().map(|_| {
            let m = &c["member"];
            info::MemberView {
                display_name: m["displayName"].as_str().unwrap_or_default().into(),
                display_color: m["displayColor"].as_u64().unwrap_or(0) as u32,
                avatar_url: m["avatarUrl"].as_str().unwrap_or_default().into(),
                joined_ms: m["joinedMs"].as_i64(),
                premium_since_ms: m["premiumSinceMs"].as_i64(),
                timeout_until_ms: m["timeoutUntilMs"].as_i64(),
                roles: m["roles"].as_array().into_iter().flatten().map(|r| (r[0].as_str().unwrap_or_default().to_string(), r[1].as_i64().unwrap_or(0))).collect(),
                permissions: perms_of(&m["permissions"]),
                level: level(&c["level"]),
                is_guild_owner: m["isGuildOwner"].as_bool().unwrap_or(false),
            }
        });
        let embed = info::build_user_embed("9005", &user, member.as_ref(), c["viewerIsModerator"].as_bool().unwrap_or(false), 1_800_000_000_000, None);
        assert_eq!(ej(&embed), norm(&c["embed"]), "{}", user.id);
    }
}

#[test]
fn info_access_labels() {
    assert_eq!(info::access_level_label(PermissionLevel::Owner, true), "Bot Owner");
    assert_eq!(info::access_level_label(PermissionLevel::Admin, true), "Server Owner");
    assert_eq!(info::access_level_label(PermissionLevel::Admin, false), "Admin");
    assert_eq!(info::access_level_label(PermissionLevel::Moderator, false), "Moderator");
    assert_eq!(info::access_level_label(PermissionLevel::Member, false), "Member");
    assert_eq!(info::feature_label("ANIMATED_ICON"), "Animated Icon");
}

#[tokio::test]
async fn info_through_the_router_with_a_cached_guild_and_fake_rest() {
    const G: &str = "930000000000000007";
    const OWNER: &str = "930000000000000098";
    const MEMBER: &str = "930000000000000097";
    let guild = FakeGuild::new(G, OWNER)
        .name("Info *Test*")
        .features(&["COMMUNITY", "PIN_PERMISSION_MIGRATION_COMPLETE"])
        .role_with("820000000000000001", 3, Permissions::KICK_MEMBERS | Permissions::MANAGE_MESSAGES, |r| {
            r["name"] = json!("Mods");
            r["colors"] = json!({ "primary_color": 0xff0000, "secondary_color": 0x00ff00, "tertiary_color": null });
            r["color"] = json!(0xff0000);
        })
        .role("820000000000000002", 1, Permissions::empty())
        .member(OWNER, &[])
        .member(MEMBER, &["820000000000000001", "820000000000000002"])
        .bot_member(TEST_BOT_ID, &[])
        .channel("820000000000000010", 0, "general", &[])
        .channel("820000000000000011", 2, "voice", &[]);
    let rest = FakeRest::new();
    rest.on("GET", "/guilds/*", 200, json!({ "id": G, "approximate_member_count": 1234, "approximate_presence_count": 77, "stickers": [{ "id": "1" }] }));
    rest.on("GET", "/guilds/*/roles/member-counts", 200, json!({ "820000000000000001": 12 }));
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(std::slice::from_ref(&guild), &rest);
    t.install(registry());

    route_interaction(&t, FakeInteraction::slash("info").guild(G).user(MEMBER).subcommand("server").build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!calls[0].ephemeral(), "/info replies publicly");
    assert_eq!(calls[1].body["allowed_mentions"]["parse"], json!([]));
    let e = &calls[1].body["embeds"][0];
    assert_eq!(e["title"], json!("Info *Test*"));
    let field = |e: &Value, name: &str| e["fields"].as_array().into_iter().flatten().find(|f| f["name"] == name).map(|f| f["value"].clone());
    assert_eq!(field(e, "👥 Members"), Some(json!("1,234 total\n🟢 77 online")));
    assert_eq!(field(e, "💬 Channels"), Some(json!("Text: 1\nVoice: 1\nForum: 0\nCategories: 0")));
    assert_eq!(field(e, "🎭 Roles"), Some(json!("2")));
    assert_eq!(field(e, "✨ Features"), Some(json!("Community")));
    let guild_calls = rest.calls_to("GET", "/guilds/*").len();
    assert_eq!(rest.calls_to("GET", "/guilds/*")[0].query_param("with_counts"), Some("true"));
    // The counts are memoised for 60 s.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("info").guild(G).user(MEMBER).subcommand("server").build()).await;
    assert_eq!(rest.calls_to("GET", "/guilds/*").len(), guild_calls);
    t.clock.advance(60_001);
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("info").guild(G).user(MEMBER).subcommand("server").build()).await;
    assert_eq!(rest.calls_to("GET", "/guilds/*").len(), guild_calls + 1);

    t.responses.clear();
    route_interaction(
        &t,
        FakeInteraction::slash("info").guild(G).user(MEMBER).subcommand("role").option("role", option_type::ROLE, json!("820000000000000001")).resolve_from(&guild).build(),
    )
    .await;
    let e = &t.responses.calls()[1].body["embeds"][0];
    assert_eq!(e["title"], json!("Mods"));
    assert_eq!(e["color"], json!(0xff0000));
    assert_eq!(field(e, "🎨 Colour"), Some(json!("#FF0000 → #00FF00")));
    assert_eq!(field(e, "👥 Members"), Some(json!("12")));
    assert_eq!(field(e, "📊 Position"), Some(json!("2 of 2")), "discord.js's rank, not the raw position 3");
    assert_eq!(field(e, "🔑 Key permissions"), Some(json!("Manage Messages, Kick Members")));

    t.responses.clear();
    route_interaction(
        &t,
        FakeInteraction::slash("info").guild(G).user(OWNER).subcommand("user").option("member", option_type::USER, json!(MEMBER)).resolve_from(&guild).build(),
    )
    .await;
    let e = &t.responses.calls()[1].body["embeds"][0];
    assert_eq!(e["footer"]["text"], json!(format!("User ID: {MEMBER}")));
    assert_eq!(e["color"], json!(0xff0000));
    assert_eq!(field(e, "🎭 Roles [2]"), Some(json!("<@&820000000000000001> <@&820000000000000002>")));
    assert_eq!(field(e, "🛡️ Access level"), Some(json!("Moderator")));
    assert_eq!(field(e, "🔑 Key permissions"), Some(json!("Manage Messages, Kick Members")));

    // The invoker themself, the guild owner.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("info").guild(G).user(OWNER).subcommand("user").resolve_from(&guild).build()).await;
    let e = &t.responses.calls()[1].body["embeds"][0];
    assert_eq!(field(e, "🛡️ Access level"), Some(json!("Server Owner")));
    assert_eq!(field(e, "🔑 Key permissions"), Some(json!("Administrator (all permissions)")));

    // A failing count falls back to the cached holders.
    rest.fail("GET", "/guilds/*/roles/member-counts", 500, 0, "boom");
    t.clock.advance(60_001);
    t.responses.clear();
    route_interaction(
        &t,
        FakeInteraction::slash("info").guild(G).user(MEMBER).subcommand("role").option("role", option_type::ROLE, json!("820000000000000002")).resolve_from(&guild).build(),
    )
    .await;
    let e = &t.responses.calls()[1].body["embeds"][0];
    assert_eq!(field(e, "👥 Members"), Some(json!("1 (cached members only)")));

    // Errors: no role option is an internal error, answered as Info Unavailable.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("info").guild(G).user(MEMBER).subcommand("role").build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Info Unavailable"));
}

#[tokio::test]
async fn info_outside_a_cached_guild_says_server_only() {
    const G: &str = "930000000000000008";
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[FakeGuild::new("930000000000000009", "930000000000000098")]);
    t.install(registry());
    route_interaction(&t, FakeInteraction::slash("info").guild(G).subcommand("server").build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(calls[1].embed_title(), Some("❌  Server Only"));
}

// ── bot joins ─────────────────────────────────────────────────────────────────

const BOT: &str = TEST_BOT_ID;
const OTHER_BOT: &str = "100000000000000009";
const ALICE: &str = "200000000000000001";
const BOB: &str = "200000000000000002";

fn user(id: &str, name: &str) -> Value {
    json!({ "id": id, "username": name, "discriminator": "0", "avatar": null })
}

fn audit(entries: &[(&str, &str)]) -> Value {
    let users: Vec<Value> = entries.iter().map(|(_, u)| user(u, if *u == ALICE { "alice" } else { "bob" })).collect();
    json!({
        "entries": entries.iter().enumerate().map(|(i, (target, by))| json!({ "id": format!("{}", 300 + i), "action_type": 28, "target_id": target, "user_id": by })).collect::<Vec<_>>(),
        "users": users,
        "integrations": [], "webhooks": [], "threads": [], "application_commands": [], "audit_log_entries": [], "auto_moderation_rules": [], "guild_scheduled_events": []
    })
}

fn integration(app: Option<&str>, account: Option<&str>, by: Option<(&str, &str)>) -> Value {
    json!({
        "id": "1", "name": "x", "type": "discord", "enabled": true,
        "application": app.map(|a| json!({ "id": a })),
        "account": account.map_or(json!({ "id": "0", "name": "x" }), |a| json!({ "id": a, "name": "bot" })),
        "user": by.map(|(id, name)| user(id, name)),
    })
}

fn known(id: &str, name: &str, source: &str) -> Inviter {
    Inviter { inviter_id: Some(id.into()), inviter_name: Some(name.into()), source: Some(source.into()) }
}

#[test]
fn the_audit_log_and_the_integration_name_the_inviter() {
    let logs = audit(&[(OTHER_BOT, BOB), (BOT, ALICE), (BOT, BOB)]);
    assert_eq!(joins::inviter_from_audit(&logs, BOT), Some(known(ALICE, "alice", "audit")), "the newest entry of this bot");
    assert_eq!(joins::inviter_from_audit(&audit(&[(OTHER_BOT, BOB)]), BOT), None);
    let mut no_executor = audit(&[(BOT, ALICE)]);
    no_executor["entries"][0]["user_id"] = Value::Null;
    assert_eq!(joins::inviter_from_audit(&no_executor, BOT), None);
    let legacy = json!({ "entries": [{ "target_id": BOT, "user_id": ALICE }], "users": [{ "id": ALICE, "username": "al", "discriminator": "0042" }] });
    assert_eq!(joins::inviter_from_audit(&legacy, BOT).and_then(|i| i.inviter_name), Some("al#0042".into()));
    let unnamed = json!({ "entries": [{ "target_id": BOT, "user_id": ALICE }], "users": [] });
    assert_eq!(joins::inviter_from_audit(&unnamed, BOT), Some(Inviter { inviter_id: Some(ALICE.into()), inviter_name: None, source: Some("audit".into()) }));

    let list = json!([integration(Some(OTHER_BOT), None, Some((BOB, "bob"))), integration(Some(BOT), None, Some((ALICE, "alice")))]);
    assert_eq!(joins::inviter_from_integrations(&list, BOT, BOT), Some(known(ALICE, "alice", "integration")));
    let by_account = json!([integration(None, Some(BOT), Some((BOB, "bob")))]);
    assert_eq!(joins::inviter_from_integrations(&by_account, BOT, "999"), Some(known(BOB, "bob", "integration")));
    assert_eq!(joins::inviter_from_integrations(&json!([integration(Some(BOT), None, None)]), BOT, BOT), None, "no user");
}

fn joins_guild(id: &str, perms: Permissions) -> FakeGuild {
    FakeGuild::new(id, "930000000000000090").role("830000000000000001", 1, perms).bot_member(TEST_BOT_ID, &["830000000000000001"])
}

#[tokio::test]
async fn find_inviter_asks_discord_only_with_the_permission() {
    let rest = FakeRest::new();
    let g = GuildId::new(930000000000000020);
    rest.on("GET", "/guilds/*/audit-logs", 200, audit(&[(OTHER_BOT, BOB), (BOT, ALICE)]));
    rest.on("GET", "/guilds/*/integrations", 200, json!([integration(Some(BOT), None, Some((BOB, "bob")))]));
    let http = rest.http();
    let both = Some(Permissions::VIEW_AUDIT_LOG | Permissions::MANAGE_GUILD);
    assert_eq!(joins::find_inviter_with(&http, g, both, BOT, BOT).await, known(ALICE, "alice", "audit"));
    let calls = rest.calls();
    assert_eq!(calls.len(), 1, "the integration is not needed");
    assert_eq!(calls[0].query_param("action_type"), Some("28"));
    assert_eq!(calls[0].query_param("limit"), Some("10"));

    rest.clear_calls();
    assert_eq!(joins::find_inviter_with(&http, g, Some(Permissions::MANAGE_GUILD), BOT, BOT).await, known(BOB, "bob", "integration"));
    assert_eq!(rest.calls().len(), 1, "no audit-log call without View Audit Log");
    // Administrator implies both.
    rest.clear_calls();
    assert_eq!(joins::find_inviter_with(&http, g, Some(Permissions::ADMINISTRATOR), BOT, BOT).await.source.as_deref(), Some("audit"));

    rest.clear_calls();
    assert_eq!(joins::find_inviter_with(&http, g, Some(Permissions::empty()), BOT, BOT).await, Inviter::unknown());
    assert_eq!(joins::find_inviter_with(&http, g, None, BOT, BOT).await, Inviter::unknown());
    assert!(rest.calls().is_empty());

    rest.fail("GET", "/guilds/*/audit-logs", 403, 50013, "Missing Permissions");
    rest.fail("GET", "/guilds/*/integrations", 403, 50013, "Missing Permissions");
    assert_eq!(joins::find_inviter_with(&http, g, both, BOT, BOT).await, Inviter::unknown(), "a failing fetch is unknown");
}

// ── Database ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn startup_records_a_guild_once_and_a_join_replaces_the_row() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    const G: &str = "930000000000000030";
    const BLIND: &str = "930000000000000031";
    const LATER: &str = "930000000000000037";
    let t = DbTestApp::new(&url).await.with_guilds([G, BLIND, LATER]);
    t.reset_guilds(&[G, BLIND, LATER]).await;
    let rest = FakeRest::new();
    rest.on("GET", "/guilds/*/audit-logs", 200, audit(&[(BOT, ALICE)]));
    t.set_discord(rest.discord(&[joins_guild(G, Permissions::VIEW_AUDIT_LOG).json(), joins_guild(BLIND, Permissions::empty()).json(), joins_guild(LATER, Permissions::MANAGE_GUILD).json()]));
    let gid = GuildId::new(G.parse().unwrap_or(1));
    joins::record_bot_join_with_lag(&t, gid, GuildReadyReason::Startup, 0).await;
    let map = joins::list_bot_joins(&t.pool, &[G.to_string(), "1".to_string()]).await.expect("list");
    assert_eq!(map.keys().cloned().collect::<Vec<String>>(), vec![G.to_string()]);
    let row = &map[G];
    assert_eq!((row.inviter_id.as_deref(), row.inviter_name.as_deref(), row.source.as_deref()), (Some(ALICE), Some("alice"), Some("audit")));
    assert_eq!(row.joined_at.timestamp_millis(), 1_735_689_600_000, "the guild's joinedAt");
    assert_eq!(rest.calls().len(), 1);

    joins::record_bot_join_with_lag(&t, gid, GuildReadyReason::Available, 0).await;
    assert_eq!(rest.calls().len(), 1, "a known guild is not looked up again");

    // Kicked and added again by someone else.
    rest.on("GET", "/guilds/*/audit-logs", 200, audit(&[(BOT, BOB)]));
    joins::record_bot_join_with_lag(&t, gid, GuildReadyReason::Join, 0).await;
    let map = joins::list_bot_joins(&t.pool, &[G.to_string()]).await.expect("list");
    assert_eq!(map[G].inviter_id.as_deref(), Some(BOB));

    // Unknown is stored too, and the next startup tries again.
    let blind = GuildId::new(BLIND.parse().unwrap_or(1));
    joins::record_bot_join_with_lag(&t, blind, GuildReadyReason::Startup, 0).await;
    let map = joins::list_bot_joins(&t.pool, &[BLIND.to_string()]).await.expect("list");
    assert_eq!(map[BLIND].inviter_id, None);
    assert_eq!(map[BLIND].joined_at.timestamp_millis(), 1_735_689_600_000);
    let before = rest.calls().len();
    joins::record_bot_join_with_lag(&t, blind, GuildReadyReason::Startup, 0).await;
    assert_eq!(rest.calls().len(), before, "still no permission: no REST call");
    assert!(joins::list_bot_joins(&t.pool, &[]).await.expect("empty").is_empty());

    // Unknown at first (no integration yet); the next startup looks again and finds it.
    rest.on("GET", "/guilds/*/integrations", 200, json!([]));
    let later = GuildId::new(LATER.parse().unwrap_or(1));
    joins::record_bot_join_with_lag(&t, later, GuildReadyReason::Startup, 0).await;
    assert_eq!(joins::list_bot_joins(&t.pool, &[LATER.to_string()]).await.expect("list")[LATER].inviter_id, None);
    rest.on("GET", "/guilds/*/integrations", 200, json!([integration(Some(BOT), None, Some((ALICE, "alice")))]));
    joins::record_bot_join_with_lag(&t, later, GuildReadyReason::Startup, 0).await;
    let row = joins::list_bot_joins(&t.pool, &[LATER.to_string()]).await.expect("list")[LATER].clone();
    assert_eq!((row.inviter_id.as_deref(), row.source.as_deref()), (Some(ALICE), Some("integration")));

    // The plugin runs it on guildReady.
    t.reset_guilds(&[G]).await;
    t.install(registry());
    t.run_hooks(GuildReady { guild_id: gid, reason: GuildReadyReason::Startup }).await;
    let map = joins::list_bot_joins(&t.pool, &[G.to_string()]).await.expect("list");
    assert_eq!(map[G].inviter_id.as_deref(), Some(BOB));
    t.reset_guilds(&[G, BLIND, LATER]).await;
}

#[tokio::test]
async fn js_and_rust_read_each_others_bot_joins() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    const RUST: &str = "930000000000000032";
    const JS: &str = "930000000000000033";
    let t = DbTestApp::new(&url).await.with_guilds([RUST, JS]);
    t.reset_guilds(&[RUST, JS]).await;
    let rest = FakeRest::new();
    rest.on("GET", "/guilds/*/audit-logs", 200, audit(&[(BOT, ALICE)]));
    t.set_discord(rest.discord(&[joins_guild(RUST, Permissions::VIEW_AUDIT_LOG).json()]));
    joins::record_bot_join_with_lag(&t, GuildId::new(RUST.parse().unwrap_or(1)), GuildReadyReason::Startup, 0).await;
    let js = run_node("rust/fixtures/gen/feat-general-js.mjs", &["bot-list", &json!([RUST]).to_string()]);
    assert_eq!(js, json!({ RUST: { "inviterId": ALICE, "inviterName": "alice", "source": "audit", "joinedAt": "2025-01-01T00:00:00.000Z" } }));

    let js = run_node("rust/fixtures/gen/feat-general-js.mjs", &["bot-record", JS, BOB, "bob#0001"]);
    assert_eq!(js[JS]["inviterId"], json!(BOB), "{js}");
    let map = joins::list_bot_joins(&t.pool, &[JS.to_string()]).await.expect("list");
    assert_eq!(serde_json::to_value(&map[JS]).unwrap_or_default(), js[JS]);
    t.reset_guilds(&[RUST, JS]).await;
}

#[tokio::test]
async fn js_and_rust_read_each_others_settings() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    const RUST: &str = "930000000000000034";
    const JS: &str = "930000000000000035";
    let t = DbTestApp::new(&url).await.with_guilds([RUST, JS]);
    t.reset_guilds(&[RUST, JS]).await;
    t.install(registry());
    let role = "840000000000000001";
    for i in [
        admin_slash("settings", "color", RUST).option("value", option_type::STRING, json!("#5865F2")),
        admin_slash("settings", "prefix", RUST).option("prefix", option_type::STRING, json!("off")),
        admin_slash("settings", "timezone", RUST).option("zone", option_type::STRING, json!("europe/paris")),
        admin_slash("settings", "urban", RUST).option("nsfw-only", option_type::BOOLEAN, json!(false)),
        FakeInteraction::slash("settings").guild(RUST).permissions(Permissions::MANAGE_GUILD).group("timezone-role", "add").option("role", option_type::ROLE, json!(role)).option("zone", option_type::STRING, json!("CST")).resolve_from(&rg(RUST)),
        admin_slash("module", "disable", RUST).option("module", option_type::STRING, json!("leveling")),
    ] {
        route_interaction(&t, i.build()).await;
    }
    let js = run_node("rust/fixtures/gen/feat-general-js.mjs", &["settings-get", RUST]);
    assert_eq!(
        js,
        json!({
            "timezone": "Europe/Paris", "timezoneRoles": [{ "roleId": role, "zone": "America/Chicago" }], "embedColor": 0x5865F2,
            "customPrefix": "", "urbanNsfwOnly": false, "xpEnabled": false, "invitesEnabled": true, "pollsEnabled": true
        })
    );
    // Rust's colour reset stores null.
    route_interaction(&t, admin_slash("settings", "color", RUST).option("value", option_type::STRING, json!("none")).build()).await;
    assert_eq!(run_node("rust/fixtures/gen/feat-general-js.mjs", &["settings-get", RUST])["embedColor"], Value::Null);

    // JS writes, Rust reads and shows them.
    let data = json!({ "timezone": "Asia/Tokyo", "customPrefix": "?", "embedColor": 0x00ff00, "timezoneRoles": [{ "roleId": role, "zone": "Europe/Berlin" }], "urbanNsfwOnly": false });
    let js = run_node("rust/fixtures/gen/feat-general-js.mjs", &["settings-update", JS, &data.to_string()]);
    assert_eq!(js["customPrefix"], json!("?"), "{js}");
    let s = t.settings().get_settings(JS).await.expect("settings");
    assert_eq!((s.timezone.as_str(), s.custom_prefix.as_str(), s.embed_color, s.urban_nsfw_only), ("Asia/Tokyo", "?", Some(0x00ff00), false));
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("settings").guild(JS).permissions(Permissions::MANAGE_GUILD).group("timezone-role", "list").build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("ℹ️  Timezone Roles (1/25)"));
    assert!(calls[1].embed_description().unwrap_or_default().contains("`Europe/Berlin`"));
    assert!(calls[1].embed_description().unwrap_or_default().contains("use `Asia/Tokyo` (the server timezone)"));
    assert_eq!(calls[1].body["embeds"][0]["color"], json!(0x00ff00));
    t.reset_guilds(&[RUST, JS]).await;
}

#[tokio::test]
async fn help_reads_the_custom_commands_the_js_reads() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    const G: &str = "930000000000000036";
    let t = DbTestApp::new(&url).await.with_guilds([G]);
    t.reset_guilds(&[G]).await;
    t.settings().update_settings(G, SettingsPatch::new().set("customCommandsEnabled", true).set("customPrefix", ">>")).await.expect("settings");
    let tags = json!([
        { "name": "rules", "description": "Server rules", "uses": 5 },
        { "name": "faq", "uses": 5 },
        { "name": "staff", "uses": 9, "allowRoleIds": ["r-staff"] },
        { "name": "hidden", "uses": 1, "enabled": false },
        { "name": "nope", "uses": 0, "denyRoleIds": ["r-muted"] },
    ]);
    let js_all = run_node("rust/fixtures/gen/feat-general-js.mjs", &["tags", G, &tags.to_string(), "null"]);
    let settings = t.settings().get_settings(G).await.expect("settings");
    let rust_all = help::custom_command_group(&t.pool, G, Some(&settings), None).await;
    assert_eq!(serde_json::to_value(&rust_all).unwrap_or_default(), js_all);
    assert_eq!(js_all["lines"][0], json!("`>>staff` — custom command"));
    for viewer in [json!([]), json!(["r-staff", "r-muted"])] {
        let js = run_node("rust/fixtures/gen/feat-general-js.mjs", &["tags", G, "[]", &viewer.to_string()]);
        let ids: Vec<String> = viewer.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
        let rust = help::custom_command_group(&t.pool, G, Some(&settings), Some(&ids)).await;
        assert_eq!(serde_json::to_value(&rust).unwrap_or_default(), js, "{viewer}");
    }
    t.reset_guilds(&[G]).await;
}
