//! Port of `test/welcome.test.js`: the template truth table, healing, payloads, card style, autorole helpers, the
//! senders and hook bodies against a recording Discord fake, the setup modals, diagnostics, the job, and the
//! database and cross-language cases (`TEST_DATABASE_URL` / `TEST_REDIS_URL`).
//!
//! Most pure cases are compared with `rust/fixtures/golden/feat-welcome/golden.json`, which
//! `rust/fixtures/gen/feat-welcome-golden.mjs` produces from the JS service itself.

mod common;

use bot_core::testing::{FakeGuild, TEST_BOT_ID, TestApp};
use bot_db::GuildSettings;
use common::*;
use feat_welcome::welcome_service::{self as svc, Kind, PayloadInput};
use serde_json::{Value, json};
use serenity::all::{Member, Permissions};

fn kind(s: &str) -> Kind {
    Kind::from_name(s).unwrap_or(Kind::Welcome)
}

fn resolved_json(r: &svc::Resolved) -> Value {
    json!({ "content": r.content, "embed": r.embed })
}

#[test]
fn resolve_template_matches_the_js_truth_table() {
    let g = golden();
    let cases = g["welcome"]["resolve"].as_array().expect("cases");
    assert_eq!(cases.len(), 150);
    for c in cases {
        let s = settings(&c["over"]);
        let r = svc::resolve_template(kind(c["kind"].as_str().unwrap_or("")), Some(&s), c["forceText"].as_bool().unwrap_or(false));
        same(&resolved_json(&r), &c["out"], &format!("{c}"));
    }
}

#[test]
fn heal_serialize_and_card_style_match_the_js() {
    let g = golden();
    for c in g["welcome"]["heal"].as_array().into_iter().flatten() {
        let out = svc::heal_embed(Some(&c["input"]));
        same(&out.unwrap_or(Value::Null), &c["out"], &format!("heal {}", c["input"]));
        assert_eq!(svc::has_embed_content(&c["input"]), c["has"].as_bool().unwrap_or(false), "has {}", c["input"]);
    }
    for c in g["welcome"]["serialize"].as_array().into_iter().flatten() {
        same(&svc::serialize_embed(&c["input"]), &c["out"], "serialize");
    }
    for c in g["welcome"]["cardStyle"].as_array().into_iter().flatten() {
        let parsed = svc::parse_card_style(Some(&c["input"]));
        same(&parsed.to_json(), &c["parsed"], &format!("parse {}", c["input"]));
        same_ordered(&svc::serialize_card_style(&parsed), &c["serialized"], "serialize style");
        assert_eq!(svc::describe_card_style(&parsed), c["described"].as_str().unwrap_or(""), "{}", c["input"]);
    }
    for c in g["welcome"]["cardEnabled"].as_array().into_iter().flatten() {
        assert_eq!(svc::welcome_card_enabled(Some(&settings(&c["over"]))), c["out"].as_bool().unwrap_or(false), "{}", c["over"]);
    }
    for c in g["welcome"]["materialise"].as_array().into_iter().flatten() {
        let cur = settings(&c["over"]);
        let mut data = bot_core::SettingsPatch(c["data"].as_object().cloned().unwrap_or_default());
        svc::materialise_welcome(&cur, &mut data);
        same(&Value::Object(data.0), &c["out"], &format!("materialise {c}"));
    }
}

#[test]
fn autorole_helpers_match_the_js() {
    let g = golden();
    for c in g["welcome"]["delay"].as_array().into_iter().flatten() {
        let input = c["input"].as_str().unwrap_or("");
        let expected = &c["out"];
        match svc::parse_delay_input(input) {
            Ok(s) => assert_eq!(Some(s), expected["seconds"].as_i64(), "{input:?}"),
            Err(e) => assert_eq!(Some(e.as_str()), expected["error"].as_str(), "{input:?}"),
        }
    }
    for c in g["welcome"]["delayText"].as_array().into_iter().flatten() {
        assert_eq!(svc::delay_text(c["s"].as_i64().unwrap_or(0)), c["out"].as_str().unwrap_or(""));
    }
    for c in g["welcome"]["screening"].as_array().into_iter().flatten() {
        let out = svc::is_screening_completion(c["oldPending"].as_bool(), c["newPending"].as_bool(), c["oldKnown"].as_bool().unwrap_or(false), c["joined"].as_i64(), c["now"].as_i64().unwrap_or(0));
        let got = match out {
            Some(svc::Screening::Cached) => json!("cached"),
            Some(svc::Screening::Maybe) => json!("maybe"),
            None => Value::Null,
        };
        assert_eq!(got, c["out"], "{c}");
    }
    for c in g["welcome"]["dueAt"].as_array().into_iter().flatten() {
        let mut s = settings(&json!({}));
        s.autorole_delay = c["delay"].as_i64().unwrap_or(0) as i32;
        assert_eq!(svc::autorole_due_at(c["joined"].as_i64(), Some(&s), c["now"].as_i64().unwrap_or(0)), c["out"].as_i64().unwrap_or(-1), "{c}");
    }
    assert_eq!(svc::autorole_job_key(GID, UID), g["welcome"]["constants"]["autoroleJobKey"].as_str().unwrap_or(""));
    assert_eq!(svc::flood_key(GID, "card"), g["welcome"]["floodKeys"][0].as_str().unwrap_or(""));
    let c = &g["welcome"]["constants"];
    assert_eq!(svc::DEFAULT_WELCOME_MESSAGE, c["DEFAULT_WELCOME_MESSAGE"]);
    assert_eq!(svc::DEFAULT_WELCOME_DM_MESSAGE, c["DEFAULT_WELCOME_DM_MESSAGE"]);
    assert_eq!(svc::DEFAULT_GOODBYE_MESSAGE, c["DEFAULT_GOODBYE_MESSAGE"]);
    same_ordered(&svc::default_welcome_embed(), &c["DEFAULT_WELCOME_EMBED"], "default welcome embed");
    same_ordered(&svc::default_goodbye_embed(), &c["DEFAULT_GOODBYE_EMBED"], "default goodbye embed");
    assert_eq!(json!(svc::MAX_AUTOROLE_DELAY_SECONDS), c["MAX_AUTOROLE_DELAY_SECONDS"]);
    assert_eq!(json!(svc::SCREENING_WATCH_MS), c["SCREENING_WATCH_MS"]);
    assert_eq!(json!(svc::SCREENING_POLL_MS), c["SCREENING_POLL_MS"]);
    for (k, n) in svc::FLOOD_CAPS {
        assert_eq!(json!(n), c["FLOOD_CAPS"][k]);
    }
    for k in [Kind::Welcome, Kind::Dm, Kind::Goodbye] {
        assert_eq!(json!(k.message_field()), c["MESSAGE_FIELDS"][k.as_str()]);
    }
}

#[test]
fn descriptions_and_text_helpers_match_the_js() {
    let g = golden();
    for c in g["welcome"]["describeMode"].as_array().into_iter().flatten() {
        let s = settings(&c["over"]);
        assert_eq!(svc::describe_mode(Kind::Welcome, &s), c["welcome"].as_str().unwrap_or(""), "{}", c["over"]);
        assert_eq!(svc::describe_mode(Kind::Dm, &s), c["dm"].as_str().unwrap_or(""));
        assert_eq!(svc::describe_mode(Kind::Goodbye, &s), c["goodbye"].as_str().unwrap_or(""));
        assert_eq!(svc::describe_embed_settings(Kind::Welcome, &s), c["embedWelcome"].as_str().unwrap_or(""), "{}", c["over"]);
        assert_eq!(svc::describe_embed_settings(Kind::Goodbye, &s), c["embedGoodbye"].as_str().unwrap_or(""));
        assert_eq!(svc::card_summary(&s), c["card"].as_str().unwrap_or(""));
        same(&embed_json(&svc::message_info_embed("Welcome Text", Kind::Welcome, &s)), &c["info"]["welcome"], "info welcome");
        same(&embed_json(&svc::message_info_embed("Goodbye Text", Kind::Goodbye, &s)), &c["info"]["goodbye"], "info goodbye");
    }
    for c in g["welcome"]["channelNeeds"].as_array().into_iter().flatten() {
        let s = settings(&c["over"]);
        assert_eq!(json!(svc::welcome_channel_needs(Some(&s))), c["out"], "{}", c["over"]);
    }
    let (guild, member) = (test_guild("Test $& Server"), test_member());
    let ctx = svc::render_ctx(Some(&guild), Some(&member), Some(&member.user), None);
    for c in g["welcome"]["unknown"].as_array().into_iter().flatten() {
        assert_eq!(svc::unknown_token_warning(c["text"].as_str().unwrap_or(""), &ctx), c["out"].as_str().unwrap_or(""));
    }
    for c in g["welcome"]["messageUpdate"].as_array().into_iter().flatten() {
        let s = settings(&c["over"]);
        let ch = &c["change"];
        let e = svc::message_update_embed(
            c["title"].as_str().unwrap_or(""),
            kind(c["kind"].as_str().unwrap_or("")),
            &s,
            ch.get("text").and_then(Value::as_str),
            ch["reset"].as_bool().unwrap_or(false),
            ch.get("enabled").and_then(Value::as_bool),
            &ctx,
        );
        same(&embed_json(&e), &c["out"], &format!("message update {c}"));
    }
}

#[test]
fn payloads_match_the_js_for_every_setting_and_mode() {
    let g = golden();
    let guild = test_guild("Test $& Server");
    let long_guild = test_guild(&"N".repeat(120));
    let member = test_member();
    let cases = g["welcome"]["payloads"].as_array().expect("payloads");
    assert!(cases.len() > 300);
    for c in cases {
        let s = settings(&c["over"]);
        let opts = &c["opts"];
        let gd = if opts["long"].as_bool() == Some(true) { &long_guild } else { &guild };
        let no_member = opts["noMember"].as_bool() == Some(true);
        let built = svc::build_welcome_payload(
            kind(c["kind"].as_str().unwrap_or("")),
            PayloadInput {
                settings: Some(&s),
                guild: Some(gd),
                member: if no_member { None } else { Some(&member) },
                user: Some(&member.user),
                channel: None,
                card: (opts["card"].as_bool() == Some(true)).then(|| vec![1u8]),
                force_text: opts["forceText"].as_bool() == Some(true),
                preview: opts["preview"].as_bool() == Some(true),
            },
        );
        let got = built.map(|m| normalize_payload(m.json)).unwrap_or(Value::Null);
        same(&got, &c["out"], &format!("payload {}", c["name"]));
    }
}

#[test]
fn setup_modals_match_the_js_and_are_never_rejected() {
    let g = golden();
    for c in g["welcome"]["modals"].as_array().into_iter().flatten() {
        let s = settings(&c["over"]);
        for k in [Kind::Welcome, Kind::Dm, Kind::Goodbye] {
            same(&svc::setup_modal_json(k, Some(&s)), &c[k.as_str()], &format!("modal {} {}", k.as_str(), c["over"]));
        }
    }
    // isStaleSetupForm: the retired ids, a partial form, or nothing at all.
    let has = |ids: &'static [&'static str]| move |id: &str| ids.contains(&id);
    assert!(svc::is_stale_setup_form(has(&["welcome-title", "welcome-description", "welcome-color", "welcome-thumbnail", "welcome-footer"])));
    assert!(!svc::is_stale_setup_form(has(&["tpl-content", "tpl-title", "tpl-description", "tpl-color", "tpl-footer"])));
    assert!(svc::is_stale_setup_form(has(&["tpl-content", "tpl-title"])));
    assert!(svc::is_stale_setup_form(has(&[])));
}

#[test]
fn welcome_config_error_carries_an_optional_title() {
    let e = svc::WelcomeConfigError::titled("x", "Title");
    assert_eq!((e.message.as_str(), e.title.as_deref()), ("x", Some("Title")));
    assert_eq!(svc::WelcomeConfigError::new("y").title, None);
    let wrapped = anyhow::Error::new(e.clone());
    assert_eq!(wrapped.downcast_ref::<svc::WelcomeConfigError>(), Some(&e));
    // The patch goes to bot-db unchanged: a null on a Json column is SQL NULL there (Prisma.DbNull).
    let p = bot_core::SettingsPatch::new().set("welcomeEmbed", Value::Null).set("autoroleDelay", 0);
    assert_eq!(svc::to_db_data(p.clone()), p);
}

#[test]
fn enabled_parts_names_only_what_is_on() {
    use feat_welcome::commands::welcome::enabled_parts;
    assert_eq!(enabled_parts(false, false), "Welcome messages are on. Still off: the welcome DM (`/welcome dm enable`) and goodbye messages (`/welcome goodbye enable`).");
    assert_eq!(enabled_parts(false, true), "Welcome messages and goodbye messages are on. Still off: the welcome DM (`/welcome dm enable`).");
    assert_eq!(enabled_parts(true, true), "Welcome messages, welcome DMs and goodbye messages are on.");
}

// ── Autorole resolution and gates against a cached guild ──────────────────────

fn roles_guild() -> FakeGuild {
    FakeGuild::new(GID, OWNER)
        .role(BOT_ROLE, 10, Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::ATTACH_FILES)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role_with("400000000000000001", 2, Permissions::empty(), |r| r["name"] = json!("Member"))
        .role_with("400000000000000003", 2, Permissions::empty(), |r| r["name"] = json!("Fan Club"))
        .role("500000000000000003", 11, Permissions::empty())
        .role("500000000000000004", 2, Permissions::ADMINISTRATOR)
        .role_with("500000000000000005", 2, Permissions::empty(), |r| r["managed"] = json!(true))
        .role("300000000000000001", 1, Permissions::empty())
}

#[tokio::test]
async fn resolve_autoroles_and_inputs() {
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[roles_guild()]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, gid()).expect("cached");
    let (a, b, c) = ("400000000000000001", "400000000000000002", "400000000000000003");
    let mut s = settings(&json!({}));
    s.autorole_ids = vec![a.into(), b.into(), c.into(), a.into()];
    let (roles, missing) = svc::resolve_autoroles(Some(&guild), Some(&s));
    assert_eq!(roles.iter().map(|r| r.id.to_string()).collect::<Vec<_>>(), vec![a, c]);
    assert_eq!(missing, vec![b.to_string()]);
    let r = |i: &str| svc::resolve_autorole_input(Some(&guild), Some(&s), i);
    assert_eq!(r(b).as_deref(), Some(b), "a deleted role can be removed by id");
    assert_eq!(r(&format!("<@&{a}>")).as_deref(), Some(a));
    assert_eq!(r("@member").as_deref(), Some(a));
    assert_eq!(r("FAN CLUB").as_deref(), Some(c));
    assert_eq!(r("499999999999999999"), None);
    assert_eq!(r("nobody"), None);
    assert_eq!(r(""), None);
}

#[tokio::test]
async fn captcha_gate_only_while_enabled_the_role_exists_and_the_member_lacks_it() {
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[roles_guild()]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, gid()).expect("cached");
    let mut s = settings(&json!({ "captchaEnabled": true, "captchaRoleId": "300000000000000009" }));
    let human = test_member();
    assert!(!svc::is_captcha_gated(Some(&guild), &human, Some(&s)), "a deleted captcha role never locks anyone out");
    s.captcha_role_id = Some("300000000000000001".into());
    assert!(svc::is_captcha_gated(Some(&guild), &human, Some(&s)));
    let mut verified = test_member();
    verified.roles.push(serenity::all::RoleId::new(300000000000000001));
    assert!(!svc::is_captcha_gated(Some(&guild), &verified, Some(&s)));
    let mut bot = test_member();
    bot.user.bot = true;
    assert!(!svc::is_captcha_gated(Some(&guild), &bot, Some(&s)), "bots are never gated");
    s.captcha_enabled = false;
    assert!(!svc::is_captcha_gated(Some(&guild), &human, Some(&s)));
}

// ── Senders and hooks against the recording Discord fake ──────────────────────

/// A guild with a welcome channel the bot can fully use, and channels that each lack one permission.
fn send_guild() -> FakeGuild {
    let deny = |p: Permissions| vec![(BOT_ROLE, false, Permissions::empty(), p)];
    FakeGuild::new(GID, OWNER)
        .role(BOT_ROLE, 10, Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::ATTACH_FILES)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role("500000000000000021", 2, Permissions::empty())
        .role("500000000000000022", 2, Permissions::empty())
        .role("500000000000000003", 11, Permissions::empty())
        .role("500000000000000004", 2, Permissions::BAN_MEMBERS)
        .role("300000000000000009", 1, Permissions::empty())
        .channel(CH, 0, "welcome", &[])
        .channel(CH_MUTE, 0, "mute", &deny(Permissions::SEND_MESSAGES))
        .channel(CH_NO_EMBEDS, 0, "no-embeds", &deny(Permissions::EMBED_LINKS))
        .channel(CH_NO_FILES, 0, "no-files", &deny(Permissions::ATTACH_FILES))
}

fn send_app() -> (TestApp, std::sync::Arc<FakeIo>) {
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[send_guild()]);
    let io = FakeIo::install(&t);
    (t, io)
}

fn on(over: Value) -> GuildSettings {
    settings(&over)
}

const WELCOME_TEXT: &str = "Hey <@111111111111111111>, welcome to **Test Guild**!";

#[tokio::test]
async fn send_welcome_channel_posts_the_exact_message_and_pings_only_the_new_member() {
    let (t, io) = send_app();
    let member = test_member();
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH })), &svc::CardMemo::fixed(None)).await;
    let sent = io.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, CH);
    assert_eq!(sent[0].1["content"], WELCOME_TEXT);
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": [], "users": [UID], "roles": [] }));
}

#[tokio::test]
async fn send_welcome_channel_respects_the_toggles_a_missing_channel_and_missing_permissions() {
    let (t, io) = send_app();
    let member = test_member();
    let none = svc::CardMemo::fixed(None);
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH, "welcomeEnabled": false })), &none).await;
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH, "welcomeMessageEnabled": false })), &none).await;
    svc::send_welcome_channel(&t, &member, &on(json!({})), &none).await;
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": "299999999999999999" })), &none).await;
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH_MUTE })), &none).await;
    assert!(io.sent().is_empty());
}

#[tokio::test]
async fn no_embed_links_means_text_and_no_attach_files_means_no_card() {
    let (t, io) = send_app();
    let member = test_member();
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH_NO_EMBEDS })), &svc::CardMemo::fixed(None)).await;
    let sent = io.sent();
    assert_eq!(sent[0].1["embeds"], json!([]));
    assert_eq!(sent[0].1["content"], WELCOME_TEXT);

    let card = svc::CardMemo::fixed(Some(vec![1]));
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH_NO_FILES })), &card).await;
    assert!(!card.was_rendered(), "the card is never rendered for a channel without Attach Files");
    let sent = io.sent();
    assert_eq!(sent[1].1["files"], json!([]));
    assert_eq!(sent[1].1["embeds"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn a_50013_with_the_card_is_retried_without_it_and_a_50035_with_the_default_text() {
    let (t, io) = send_app();
    let member = test_member();
    io.fail_sends(&[Some(50013)]);
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH })), &svc::CardMemo::fixed(Some(vec![1]))).await;
    let sent = io.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].1["files"], json!(["welcome.png"]));
    assert_eq!(sent[1].1["files"], json!([]));

    let (t, io) = send_app();
    io.fail_sends(&[Some(50035)]);
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH, "welcomeEmbed": { "title": "T" } })), &svc::CardMemo::fixed(None)).await;
    let sent = io.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].1["content"], WELCOME_TEXT);
    assert_eq!(sent[1].1["embeds"], Value::Null, "the plain retry carries no embeds");
    assert_eq!(sent[1].1["allowed_mentions"], json!({ "parse": [], "users": [UID], "roles": [] }));

    // Any other failure is swallowed (logged): the sender never fails.
    let (t, io) = send_app();
    io.fail_sends(&[Some(500)]);
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH })), &svc::CardMemo::fixed(None)).await;
    assert_eq!(io.sent().len(), 1);
}

#[tokio::test]
async fn welcome_dm_closed_dms_are_silent_bots_and_the_toggle_are_respected_the_card_is_optional() {
    let (t, io) = send_app();
    let none = svc::CardMemo::fixed(None);
    let dm_on = on(json!({ "welcomeDmEnabled": true }));
    let member = test_member();
    svc::send_welcome_dm(&t, &member, &dm_on, &none).await;
    let dms = io.dms();
    assert_eq!(dms.len(), 1);
    assert_eq!(dms[0].1["components"][0]["components"][0]["custom_id"], "noop:welcome-dm");
    let mut bot = test_member();
    bot.user.bot = true;
    svc::send_welcome_dm(&t, &bot, &dm_on, &none).await;
    svc::send_welcome_dm(&t, &member, &on(json!({ "welcomeDmEnabled": true, "welcomeEnabled": false })), &none).await;
    svc::send_welcome_dm(&t, &member, &on(json!({})), &none).await;
    assert_eq!(io.dms().len(), 1);

    io.fail_dms(&[Some(50007)]);
    svc::send_welcome_dm(&t, &member, &dm_on, &none).await; // resolves without failing
    svc::send_welcome_dm(&t, &member, &on(json!({ "welcomeDmEnabled": true, "welcomeDmCard": true })), &svc::CardMemo::fixed(Some(vec![1]))).await;
    let dms = io.dms();
    assert_eq!(dms.last().map(|d| d.1["files"].clone()), Some(json!(["welcome.png"])));

    io.fail_dms(&[Some(50035)]);
    let before = io.dms().len();
    svc::send_welcome_dm(&t, &member, &on(json!({ "welcomeDmEnabled": true, "welcomeDmEmbed": { "title": "T" } })), &none).await;
    let dms = io.dms();
    assert_eq!(dms.len(), before + 2, "the failed attempt and the plain retry");
    let retry = &dms.last().expect("retry").1;
    assert_eq!(retry["embeds"], json!([]));
    assert_eq!(retry["components"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn goodbye_posts_for_a_cached_or_partial_member_and_never_pings() {
    let (t, io) = send_app();
    let member = test_member();
    let s = on(json!({ "goodbyeEnabled": true, "goodbyeChannelId": CH }));
    svc::send_goodbye(&t, gid(), Some(&member), &member.user, &s).await;
    svc::send_goodbye(&t, gid(), None, &member.user, &s).await;
    let sent = io.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].1["content"], "**newbie** just left the server");
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    assert_eq!(sent[1].1["content"], "**newbie** just left the server");
    svc::send_goodbye(&t, gid(), Some(&member), &member.user, &on(json!({ "goodbyeChannelId": CH }))).await;
    svc::send_goodbye(&t, gid(), Some(&member), &member.user, &on(json!({ "goodbyeEnabled": true, "goodbyeChannelId": CH, "welcomeEnabled": false }))).await;
    svc::send_goodbye(&t, gid(), Some(&member), &member.user, &on(json!({ "goodbyeEnabled": true }))).await;
    assert_eq!(io.sent().len(), 2);
}

fn roles_of(ids: &[&str]) -> Vec<serenity::all::Role> {
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[send_guild()]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, gid()).expect("cached");
    ids.iter().filter_map(|id| guild.roles.get(&serenity::all::RoleId::new(id.parse().unwrap_or(1))).cloned()).collect()
}

#[tokio::test]
async fn grant_autoroles_one_at_a_time_idempotent_and_skips_what_cannot_be_given() {
    let (t, io) = send_app();
    let mut member = test_member();
    member.roles.push(serenity::all::RoleId::new(500000000000000022)); // already held
    let roles = roles_of(&["500000000000000021", "500000000000000022", "500000000000000003", "500000000000000004"]);
    let out = svc::grant_autoroles(&t, &member, &roles, false).await.expect("never fails without rethrow");
    assert_eq!(out.added, vec!["500000000000000021"]);
    assert_eq!(io.roles(), vec![(UID.to_string(), "500000000000000021".to_string(), Some("Welcome autorole".to_string()))]);
    assert_eq!(out.failed.iter().map(|f| f.role_id.as_str()).collect::<Vec<_>>(), vec!["500000000000000003", "500000000000000004"]);
    assert!(out.failed[1].reason.contains("dangerous"), "{}", out.failed[1].reason);
}

#[tokio::test]
async fn grant_autoroles_errors_member_left_stops_role_problems_continue_others_rethrow_on_request() {
    let roles = roles_of(&["500000000000000021", "500000000000000022"]);
    let member = test_member();
    let (t, io) = send_app();
    io.fail_roles(&[Some(10007)]);
    let a = svc::grant_autoroles(&t, &member, &roles, false).await.expect("ok");
    assert_eq!(a, svc::Granted::default());
    assert_eq!(io.roles().len(), 1, "stopped after the member left");

    let (t, io) = send_app();
    io.fail_roles(&[Some(50013)]);
    let b = svc::grant_autoroles(&t, &member, &roles, false).await.expect("ok");
    assert_eq!(io.roles().len(), 2);
    assert_eq!(b.added, vec!["500000000000000022"]);
    assert_eq!(b.failed[0].role_id, "500000000000000021");

    let (t, io) = send_app();
    io.fail_roles(&[Some(500), Some(500)]);
    let e = svc::grant_autoroles(&t, &member, &roles[..1], true).await.expect_err("rethrown");
    assert_eq!(e.code, Some(500));
    let soft = svc::grant_autoroles(&t, &member, &roles[..1], false).await.expect("soft");
    assert_eq!(soft.failed.len(), 1);
}

async fn join(t: &TestApp, member: Member, s: GuildSettings, captcha_pending: bool) {
    t.settings_store.put(s);
    t.settings().invalidate_settings(GID).await;
    let data = bot_core::hooks::events::GuildMemberAdd { guild_id: gid(), user: member.user.clone(), member };
    let ctx = std::sync::Arc::new(bot_core::HookCtx::new(t.app.clone(), data));
    if captcha_pending {
        ctx.state.insert(svc::CAPTCHA_PENDING_KEY, true);
    }
    svc::on_member_join(ctx).await.expect("never fails");
}

#[tokio::test]
async fn on_member_join_grants_autoroles_whatever_the_module_flag_and_the_message_do() {
    let (t, io) = send_app();
    io.fail_sends(&[Some(500)]);
    let style = svc::serialize_card_style(&svc::CardStyle { enabled: Some(false), ..Default::default() });
    join(&t, test_member(), on(json!({ "autoroleIds": ["500000000000000021"], "welcomeChannelId": CH, "welcomeCardStyle": style })), false).await;
    assert_eq!(io.roles().len(), 1);
    assert_eq!(io.sent().len(), 1, "the failing send did not undo or block the role");
    join(&t, test_member(), on(json!({ "welcomeEnabled": false, "autoroleIds": ["500000000000000021"], "welcomeChannelId": CH })), false).await;
    assert_eq!(io.roles().len(), 2);
    assert_eq!(io.sent().len(), 1, "module off: no message");
}

#[tokio::test]
async fn on_member_join_holds_captcha_pending_and_gated_humans_but_never_bots() {
    let (t, io) = send_app();
    let gate = json!({ "welcomeEnabled": false, "autoroleIds": ["500000000000000021"], "captchaEnabled": true, "captchaRoleId": "300000000000000009" });
    join(&t, test_member(), on(json!({ "welcomeEnabled": false, "autoroleIds": ["500000000000000021"] })), true).await;
    assert!(io.roles().is_empty(), "captcha calls assignAutorole later");
    join(&t, test_member(), on(gate.clone()), false).await;
    assert!(io.roles().is_empty());
    let mut bot = test_member();
    bot.user.bot = true;
    join(&t, bot, on(gate), false).await;
    assert_eq!(io.roles().len(), 1, "a bot is never held back by the captcha gate");
}

#[tokio::test]
async fn on_member_join_sends_the_message_and_the_dm_sharing_one_card_render() {
    let (t, io) = send_app();
    let style = svc::serialize_card_style(&svc::CardStyle { enabled: Some(false), ..Default::default() });
    join(&t, test_member(), on(json!({ "welcomeChannelId": CH, "welcomeDmEnabled": true, "welcomeDmCard": true, "welcomeCardStyle": style })), false).await;
    assert_eq!(io.sent().len(), 1);
    let dms = io.dms();
    assert_eq!(dms.len(), 1);
    assert_eq!(dms[0].1["files"], json!(["welcome.png"]), "the DM card (the channel card is switched off by the style)");
    assert_eq!(io.cards(), 1, "one render");
}

#[tokio::test]
async fn on_member_leave_posts_the_goodbye_only_when_both_flags_are_on() {
    let (t, io) = send_app();
    for over in [
        json!({ "goodbyeEnabled": true, "goodbyeChannelId": CH }),
        json!({ "goodbyeEnabled": false, "goodbyeChannelId": CH }),
        json!({ "welcomeEnabled": false, "goodbyeEnabled": true, "goodbyeChannelId": CH }),
    ] {
        t.settings_store.put(on(over));
        t.settings().invalidate_settings(GID).await;
        let member = test_member();
        let data = bot_core::hooks::events::GuildMemberRemove { guild_id: gid(), user: member.user.clone(), member: Some(member) };
        svc::on_member_leave(std::sync::Arc::new(bot_core::HookCtx::new(t.app.clone(), data))).await.expect("ok");
    }
    assert_eq!(io.sent().len(), 1);
}

#[tokio::test]
async fn diagnose_reports_what_stops_the_feature_in_short_lines() {
    let (t, _io) = send_app();
    let d = |over: Value, k: svc::Diagnose| svc::diagnose_welcome(&t, gid(), &on(over), k);
    let off = d(json!({ "welcomeEnabled": false }), svc::Diagnose::Welcome);
    assert!(off.iter().any(|l| l.contains("module is off")));
    assert!(off.iter().any(|l| l.contains("No welcome channel")));
    assert!(!off.iter().any(|l| l.starts_with('💡')), "the default text pings the member, so there is no tip");
    let tip = d(json!({ "welcomeMessage": "Welcome!" }), svc::Diagnose::Welcome);
    assert!(tip.iter().any(|l| l.starts_with('💡')));
    assert!(off.iter().chain(&tip).all(|l| l.starts_with("⚠️") || l.starts_with('💡')));
    let perms = d(json!({ "welcomeChannelId": CH_NO_EMBEDS }), svc::Diagnose::Welcome);
    assert!(perms.iter().any(|l| l.contains("Embed Links")), "{perms:?}");
    assert!(!perms.iter().any(|l| l.contains("module is off")));
    assert!(d(json!({ "welcomeChannelId": "299999999999999990" }), svc::Diagnose::Welcome).iter().any(|l| l.contains("was not found")));
    assert!(d(json!({}), svc::Diagnose::Dm).iter().any(|l| l.contains("welcome DM is off")));
    assert!(d(json!({ "welcomeDmEnabled": true, "welcomeDmMessage": "" }), svc::Diagnose::Dm).iter().any(|l| l.contains("Nothing would be sent")));
    assert!(d(json!({}), svc::Diagnose::Goodbye).iter().any(|l| l.contains("goodbye message is off")));
    let auto = d(json!({ "welcomeEnabled": false, "autoroleIds": ["500000000000000003", "699999999999999999"] }), svc::Diagnose::Autoroles);
    assert!(auto.iter().any(|l| l.contains("no longer exist")));
    assert!(auto.iter().any(|l| l.contains("<@&500000000000000003>")));
    assert!(auto.iter().any(|l| l.contains("run even while the module is off")));
    // The status embed: the module, channel message, DM, goodbye, card style, autoroles and the problems.
    let fields = svc::status_fields(&t, gid(), &on(json!({ "welcomeChannelId": CH, "autoroleIds": ["500000000000000021"], "autoroleDelay": 600 })));
    let names: Vec<&str> = fields.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(names, vec!["Module", "Channel message", "Welcome DM", "Goodbye", "Card style", "Autoroles", "Problems"]);
    assert_eq!(fields[0].1, "✅ On");
    assert_eq!(fields[1].1, format!("On · <#{CH}>\nText + embed (default)\nCard On\n\"Hey {{user}}, welcome to **{{server}}**!\""));
    assert_eq!(fields[5].1, "<@&500000000000000021>\nDelay: 10 minutes after joining");
}

#[tokio::test]
async fn the_autorole_job_payload_and_its_handler_offline() {
    let (t, io) = send_app();
    t.settings_store.put(on(json!({ "autoroleIds": ["500000000000000021", "500000000000000022"] })));
    let job = |payload: Value| bot_core::scheduler::Job {
        id: "J".into(),
        guild_id: GID.into(),
        job_type: "welcome.autorole".into(),
        payload,
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 3,
        dedupe_key: Some(svc::autorole_job_key(GID, UID)),
        version: 1,
    };
    let ctx = || bot_core::scheduler::JobCtx { app: t.app.clone(), guild_id: GID.into(), late_by_ms: 0, attempt: 1 };
    let bad = svc::run_autorole_job(job(json!({ "userId": "abc" })), ctx()).await.expect_err("permanent");
    assert!(bot_core::PermanentJobError::is(&bad));
    assert_eq!(svc::parse_autorole_payload(&json!({ "userId": UID, "watch": true })), Some(svc::AutorolePayload { user_id: UID.into(), watch: true }));
    io.set_member(test_member());
    assert_eq!(svc::run_autorole_job(job(json!({ "userId": UID })), ctx()).await.expect("granted"), bot_core::scheduler::JobOutcome::Done);
    assert_eq!(io.roles().len(), 2, "one role at a time, from the current settings");
    io.fail_fetches(&[Some(10007)]);
    assert_eq!(svc::run_autorole_job(job(json!({ "userId": UID })), ctx()).await.expect("left"), bot_core::scheduler::JobOutcome::Done);
    io.fail_fetches(&[Some(500)]);
    assert!(svc::run_autorole_job(job(json!({ "userId": UID })), ctx()).await.is_err(), "a transient failure is retried");
    // A watch while the member is still pending reschedules itself.
    let mut pending = test_member();
    pending.pending = true;
    pending.joined_at = Some(serenity::all::Timestamp::now());
    io.set_member(pending);
    match svc::run_autorole_job(job(json!({ "userId": UID, "watch": true })), ctx()).await.expect("watch") {
        bot_core::scheduler::JobOutcome::Reschedule { payload: None, .. } => {}
        other => panic!("expected a reschedule, got {other:?}"),
    }
}

#[tokio::test]
async fn hook_filters_bot_leaving_screening_completions_unrelated_role_deletions() {
    use feat_welcome::plugins::welcome::{leave_filter, role_delete_filter, update_filter};
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[send_guild()]);
    assert!(!leave_filter(&t, serenity::all::UserId::new(TEST_BOT_ID.parse().unwrap_or(1))));
    assert!(leave_filter(&t, serenity::all::UserId::new(UID.parse().unwrap_or(1))));
    let mut old = test_member();
    old.pending = true;
    let mut new = test_member();
    new.pending = false;
    new.joined_at = Some(serenity::all::Timestamp::now());
    let upd = |old: Option<Member>, known: bool| bot_core::hooks::events::GuildMemberUpdate { guild_id: gid(), old_member: old, member: new.clone(), old_known: known };
    assert!(update_filter(&t, &upd(Some(old.clone()), true)));
    assert!(!update_filter(&t, &upd(Some(new.clone()), true)));
    assert!(update_filter(&t, &upd(None, false)));
    assert!(role_delete_filter(&t, GID, "1"), "no memo: the handler re-checks with real settings");
}

// ═════════════════════════════════════════════════════════════════════════════
// DB cases (TEST_DATABASE_URL and TEST_REDIS_URL)
// ═════════════════════════════════════════════════════════════════════════════

use bot_core::testing::{DbTestApp, FakeInteraction, database_url, offline_discord_as, redis_url, run_node};

const ADMIN: &str = "600000000000000009";
const ADMIN_ROLE: &str = "820000000000000002";

/// The fake guild of the DB cases: roles the bot can give, an admin, a welcome channel.
fn db_guild(id: &str) -> FakeGuild {
    FakeGuild::new(id, OWNER)
        .role(BOT_ROLE, 10, Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::ATTACH_FILES)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role_with(ADMIN_ROLE, 9, Permissions::MANAGE_GUILD, |r| r["name"] = json!("Admins"))
        .member(ADMIN, &[ADMIN_ROLE])
        .role_with("955008000000000011", 2, Permissions::empty(), |r| r["name"] = json!("One"))
        .role_with("955008000000000012", 2, Permissions::empty(), |r| r["name"] = json!("Two"))
        .role("955008000000000013", 2, Permissions::BAN_MEMBERS)
        .role_with("955008000000000021", 2, Permissions::empty(), |r| r["name"] = json!("Verified"))
        .channel(CH, 0, "welcome", &[])
}

struct Db {
    t: DbTestApp,
    io: std::sync::Arc<FakeIo>,
    g: &'static str,
}

impl Db {
    async fn new(g: &'static str) -> Option<Db> {
        let db = database_url()?;
        let t = match redis_url() {
            Some(r) => DbTestApp::with_redis(&db, &r).await,
            None => DbTestApp::new(&db).await,
        }
        .with_guilds([g]);
        t.set_discord(offline_discord_as(&[db_guild(g).json()]));
        t.reset_guilds(&[g]).await;
        t.settings().ensure_guild_settings(g).await.expect("settings row");
        let io = FakeIo::install(&t);
        Some(Db { t, io, g })
    }

    fn gid(&self) -> serenity::all::GuildId {
        serenity::all::GuildId::new(self.g.parse().unwrap_or(1))
    }

    async fn set(&self, patch: Value) {
        let p = bot_core::SettingsPatch(patch.as_object().cloned().unwrap_or_default());
        self.t.settings().update_settings(self.g, p).await.expect("update");
    }

    async fn fresh(&self) -> bot_core::Settings {
        self.t.settings().get_settings_with(self.g, true).await.expect("settings")
    }

    async fn job(&self, user: &str) -> Option<bot_db::ScheduledJob> {
        self.t.scheduler().get_job(&svc::autorole_job_key(self.g, user)).await
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn member_in(g: &str, id: &str, joined_ms: i64, pending: bool) -> Member {
    let joined = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(joined_ms).unwrap_or_default().to_rfc3339();
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": false },
        "roles": [], "joined_at": joined, "deaf": false, "mute": false, "flags": 0, "pending": pending, "guild_id": g,
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

fn config_error(e: &anyhow::Error) -> String {
    e.downcast_ref::<svc::WelcomeConfigError>().map(|c| c.message.clone()).unwrap_or_else(|| format!("not a config error: {e}"))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[tokio::test]
async fn db_embed_writes_use_sql_null_keep_a_legacy_card_off_and_freeze_the_text() {
    let Some(d) = Db::new("955008000000000501").await else { return };
    d.set(json!({ "welcomeEmbed": { "title": "Old", "description": "Legacy", "color": 0x5865F2, "thumbnail": "user_avatar", "card": false } })).await;
    let a = svc::update_embed_options(&d.t, d.g, Kind::Welcome, svc::EmbedOptions { thumbnail: Some("server".into()), ..Default::default() }).await.expect("a");
    let e = a.welcome_embed.clone().unwrap_or(Value::Null);
    assert_eq!(e["thumbnail"], "{server.icon}");
    assert_eq!(e["title"], "Old");
    assert!(e.get("card").is_none());
    assert!(!svc::welcome_card_enabled(Some(&a)));
    assert_eq!(a.welcome_card_style.as_ref().map(|s| s["enabled"].clone()), Some(json!(false)));
    assert_eq!(a.welcome_message.as_deref(), Some(""), "the legacy no-text line is now explicit");

    let b = svc::update_embed_options(&d.t, d.g, Kind::Welcome, svc::EmbedOptions { reset: true, ..Default::default() }).await.expect("b");
    assert!(b.welcome_embed.is_none());
    assert!(!svc::welcome_card_enabled(Some(&b)));
    let raw: Option<Value> = bot_db::sqlx::query_scalar(r#"SELECT "welcomeEmbed" FROM "GuildSettings" WHERE "guildId" = $1"#).bind(d.g).fetch_one(d.t.db()).await.expect("raw");
    assert_eq!(raw, None, "a real SQL NULL (Prisma.DbNull), not the JSON literal null");

    let c = svc::update_embed_options(&d.t, d.g, Kind::Goodbye, svc::EmbedOptions { enabled: Some(true), thumbnail: Some("none".into()), ..Default::default() }).await.expect("c");
    assert!(c.goodbye_use_embed);
    let ge = c.goodbye_embed.clone().unwrap_or(Value::Null);
    assert!(ge.get("thumbnail").is_none());
    assert_eq!(ge["description"], svc::DEFAULT_GOODBYE_MESSAGE);
    let dd = svc::update_embed_options(&d.t, d.g, Kind::Goodbye, svc::EmbedOptions { reset: true, ..Default::default() }).await.expect("d");
    assert!(dd.goodbye_embed.is_none());

    let err = |o: svc::EmbedOptions| async { config_error(&svc::update_embed_options(&d.t, d.g, Kind::Welcome, o).await.expect_err("refused")) };
    assert!(err(svc::EmbedOptions::default()).await.contains("at least one option"));
    assert!(err(svc::EmbedOptions { reset: true, thumbnail: Some("none".into()), ..Default::default() }).await.contains("reset on its own"));
    assert!(err(svc::EmbedOptions { image_url: Some("javascript:alert(1)".into()), ..Default::default() }).await.contains("http(s) URL"));
    let img = svc::update_embed_options(&d.t, d.g, Kind::Welcome, svc::EmbedOptions { image_url: Some("https://example.test/a.png".into()), ..Default::default() }).await.expect("img");
    assert_eq!(img.welcome_embed.as_ref().map(|e| e["image"].clone()), Some(json!("https://example.test/a.png")));
    let no = svc::update_embed_options(&d.t, d.g, Kind::Welcome, svc::EmbedOptions { image_url: Some("NONE".into()), ..Default::default() }).await.expect("none");
    assert!(no.welcome_embed.as_ref().is_some_and(|e| e.get("image").is_none()));
    d.done().await;
}

#[tokio::test]
async fn db_message_and_card_style_updates() {
    let Some(d) = Db::new("955008000000000502").await else { return };
    let a = svc::update_message(&d.t, d.g, Kind::Welcome, Some("Hi {user}!"), false, Some(false)).await.expect("a").expect("changed");
    assert_eq!((a.welcome_message.as_deref(), a.welcome_message_enabled), (Some("Hi {user}!"), false));
    let b = svc::update_message(&d.t, d.g, Kind::Welcome, None, true, None).await.expect("b").expect("changed");
    assert_eq!(b.welcome_message, None);
    assert!(svc::update_message(&d.t, d.g, Kind::Dm, None, false, None).await.expect("nothing").is_none());
    let c = svc::update_message(&d.t, d.g, Kind::Dm, Some("psst"), false, None).await.expect("c").expect("changed");
    assert_eq!(c.welcome_dm_message.as_deref(), Some("psst"));
    let long = "x".repeat(2500);
    assert!(config_error(&svc::update_message(&d.t, d.g, Kind::Goodbye, Some(&long), false, None).await.expect_err("too long")).contains("2500"));
    assert!(config_error(&svc::update_message(&d.t, d.g, Kind::Goodbye, Some("x"), true, None).await.expect_err("both")).contains("either text or reset"));

    let s1 = svc::update_card_style(&d.t, d.g, svc::CardStyleOptions { background: Some("#ABCDEF".into()), overlay: Some(0.456), enabled: Some(false), ..Default::default() })
        .await
        .expect("s1");
    assert_eq!(
        s1.welcome_card_style,
        Some(json!({ "v": 1, "enabled": false, "background": "#abcdef", "backgroundUrl": null, "textColor": null, "accentColor": null, "overlayOpacity": 0.46 }))
    );
    let bad = svc::update_card_style(&d.t, d.g, svc::CardStyleOptions { text_color: Some("not-a-colour".into()), ..Default::default() }).await.expect_err("bad");
    assert!(config_error(&bad).contains("Text colour is not a colour"));
    let s2 = svc::update_card_style(&d.t, d.g, svc::CardStyleOptions { accent_color: Some("blurple".into()), ..Default::default() }).await.expect("s2");
    let st = s2.welcome_card_style.clone().unwrap_or(Value::Null);
    assert_eq!((st["accentColor"].clone(), st["enabled"].clone()), (json!("#5865f2"), json!(false)), "untouched options keep their value");
    let img = svc::update_card_style(&d.t, d.g, svc::CardStyleOptions { background_url: Some("https://x.test/bad.png".into()), ..Default::default() }).await.expect_err("image");
    assert_eq!(img.downcast_ref::<svc::WelcomeConfigError>().and_then(|e| e.title.clone()).as_deref(), Some("Image Not Loaded"));
    let s3 = svc::update_card_style(&d.t, d.g, svc::CardStyleOptions { reset: true, ..Default::default() }).await.expect("s3");
    let st = s3.welcome_card_style.clone().unwrap_or(Value::Null);
    assert_eq!((st["background"].clone(), st["enabled"].clone()), (Value::Null, json!(false)));
    d.done().await;
}

#[tokio::test]
async fn db_autorole_add_keeps_the_stored_delay_enforces_the_cap_and_removes_deleted_roles() {
    let Some(d) = Db::new("955008000000000503").await else { return };
    let (one, two) = (serenity::all::RoleId::new(955008000000000011), serenity::all::RoleId::new(955008000000000012));
    let (a, _) = svc::add_autorole(&d.t, d.gid(), None, one, Some("10m")).await.expect("a");
    assert_eq!((a.autorole_ids.clone(), a.autorole_delay), (vec!["955008000000000011".to_string()], 600));
    let (b, _) = svc::add_autorole(&d.t, d.gid(), None, two, None).await.expect("b");
    assert_eq!(b.autorole_ids.len(), 2);
    assert_eq!(b.autorole_delay, 600, "adding without a delay must not reset it");
    let dup = svc::add_autorole(&d.t, d.gid(), None, two, None).await.expect_err("dup");
    assert!(config_error(&dup).contains("already an autorole"));
    let (upd, already) = svc::add_autorole(&d.t, d.gid(), None, two, Some("0")).await.expect("upd");
    assert!(already);
    assert_eq!(upd.autorole_delay, 0);
    assert!(config_error(&svc::add_autorole(&d.t, d.gid(), None, two, Some("9d")).await.expect_err("9d")).contains("7 days"));
    let danger = svc::add_autorole(&d.t, d.gid(), None, serenity::all::RoleId::new(955008000000000013), None).await.expect_err("dangerous");
    assert!(config_error(&danger).contains("dangerous"), "{}", config_error(&danger));
    assert!(config_error(&danger).ends_with("so roles with these permissions are refused."));

    let (ds, secs) = svc::set_autorole_delay(&d.t, d.g, "2h").await.expect("delay");
    assert_eq!((secs, ds.autorole_delay), (7200, 7200));
    assert!(config_error(&svc::set_autorole_delay(&d.t, d.g, "30d").await.expect_err("30d")).contains("7 days"));

    let many: Vec<String> = (0..bot_config::limits::AUTOROLES).map(|i| format!("9550080000001{i:05}")).collect();
    d.set(json!({ "autoroleIds": many })).await;
    assert!(config_error(&svc::add_autorole(&d.t, d.gid(), None, one, None).await.expect_err("cap")).contains("up to 10 autoroles"));

    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000099"] })).await;
    let cur = d.fresh().await;
    let (gone, id) = svc::remove_autorole(&d.t, d.gid(), &cur, "955008000000000099").await.expect("remove");
    assert_eq!(id.as_deref(), Some("955008000000000099"));
    assert_eq!(gone.map(|s| s.autorole_ids.clone()), Some(vec!["955008000000000011".to_string()]));
    assert_eq!(svc::remove_autorole(&d.t, d.gid(), &*d.fresh().await, "nobody").await.expect("none").1, None);
    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000098"] })).await;
    let guild = bot_core::interaction_helpers::cached_guild(&d.t, d.gid()).expect("guild");
    let (_, missing) = svc::resolve_autoroles(Some(&guild), Some(&*d.fresh().await));
    let existing: Vec<String> = guild.roles.keys().map(|r| r.to_string()).collect();
    assert_eq!(svc::prune_missing_autoroles(&d.t, d.g, &missing, existing).await.expect("prune"), 1);
    assert_eq!(d.fresh().await.autorole_ids, vec!["955008000000000011".to_string()]);
    d.done().await;
}

#[tokio::test]
async fn db_a_delayed_autorole_is_a_durable_job_anchored_on_the_join_time_and_cancelled_on_leave() {
    let Some(d) = Db::new("955008000000000504").await else { return };
    let user = "955008000000000002";
    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000012"], "autoroleDelay": 600 })).await;
    let joined = now_ms() - 60_000;
    let member = member_in(d.g, user, joined, false);
    svc::assign_autorole(&d.t, &member, false).await;
    assert!(d.io.roles().is_empty(), "nothing is granted before the delay");
    let job = d.job(user).await.expect("job");
    assert_eq!(job.r#type, "welcome.autorole");
    assert_eq!(job.guild_id, d.g);
    assert_eq!(job.payload, json!({ "userId": user }));
    assert_eq!(job.run_at.timestamp_millis(), joined + 600_000);

    // Leaving cancels it.
    let data = bot_core::hooks::events::GuildMemberRemove { guild_id: d.gid(), user: member.user.clone(), member: None };
    svc::on_member_leave(std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), data))).await.expect("leave");
    assert!(d.job(user).await.is_none());

    // The job, when it fires, grants from the current settings, one role at a time; again it changes nothing.
    d.io.set_member(member_in(d.g, user, joined, false));
    let row = bot_core::scheduler::Job {
        id: "J".into(),
        guild_id: d.g.into(),
        job_type: "welcome.autorole".into(),
        payload: json!({ "userId": user }),
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 3,
        dedupe_key: None,
        version: 1,
    };
    let ctx = || bot_core::scheduler::JobCtx { app: d.t.app.clone(), guild_id: d.g.into(), late_by_ms: 0, attempt: 1 };
    svc::run_autorole_job(row.clone(), ctx()).await.expect("granted");
    assert_eq!(d.io.roles().iter().map(|r| r.1.clone()).collect::<Vec<_>>(), vec!["955008000000000011", "955008000000000012"]);
    d.io.remove_member(user);
    assert_eq!(svc::run_autorole_job(row.clone(), ctx()).await.expect("left"), bot_core::scheduler::JobOutcome::Done);
    let mut bad = row.clone();
    bad.payload = json!({ "userId": "abc" });
    assert!(bot_core::PermanentJobError::is(&svc::run_autorole_job(bad, ctx()).await.expect_err("bad")));

    // Zero delay: granted at once, nothing scheduled.
    d.set(json!({ "autoroleDelay": 0 })).await;
    svc::assign_autorole(&d.t, &member_in(d.g, user, now_ms(), false), false).await;
    assert!(d.job(user).await.is_none());
    assert_eq!(d.io.roles().len(), 4);
    d.done().await;
}

#[tokio::test]
async fn db_a_bot_keeps_its_delayed_autoroles_and_a_gated_human_waits_for_verification() {
    let Some(d) = Db::new("955008000000000505").await else { return };
    let user = "955008000000000002";
    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000012"], "autoroleDelay": 600, "captchaEnabled": true, "captchaRoleId": "955008000000000021" })).await;
    let joined = now_ms() - 60_000;
    let mut bot = member_in(d.g, user, joined, false);
    bot.user.bot = true;
    svc::assign_autorole(&d.t, &bot, true).await;
    let job = d.job(user).await.expect("job");
    assert_eq!(job.payload, json!({ "userId": user }));
    assert_eq!(job.run_at.timestamp_millis(), joined + 600_000);
    // Welcome's own join hook (after captcha's) neither drops nor replaces it.
    let data = bot_core::hooks::events::GuildMemberAdd { guild_id: d.gid(), user: bot.user.clone(), member: bot.clone() };
    svc::on_member_join(std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), data))).await.expect("join");
    let kept = d.job(user).await.expect("kept");
    assert_eq!((kept.payload.clone(), kept.run_at.timestamp_millis()), (json!({ "userId": user }), joined + 600_000));
    d.t.scheduler().cancel_jobs(bot_core::scheduler::CancelJobs { guild_id: Some(d.g.into()), ..Default::default() }).await;

    // A human who has not verified: no job at join...
    let human = member_in(d.g, user, joined, false);
    let data = bot_core::hooks::events::GuildMemberAdd { guild_id: d.gid(), user: human.user.clone(), member: human.clone() };
    let ctx = std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), data));
    ctx.state.insert(svc::CAPTCHA_PENDING_KEY, true);
    svc::on_member_join(ctx).await.expect("join");
    assert!(d.job(user).await.is_none());
    // ...a screening watch that fires while they are unverified drops out without granting...
    d.io.set_member(human.clone());
    let watch = bot_core::scheduler::Job {
        id: "W".into(),
        guild_id: d.g.into(),
        job_type: "welcome.autorole".into(),
        payload: json!({ "userId": user, "watch": true }),
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 3,
        dedupe_key: None,
        version: 1,
    };
    let jctx = bot_core::scheduler::JobCtx { app: d.t.app.clone(), guild_id: d.g.into(), late_by_ms: 0, attempt: 1 };
    assert_eq!(svc::run_autorole_job(watch, jctx).await.expect("watch"), bot_core::scheduler::JobOutcome::Done);
    assert!(d.io.roles().is_empty());
    // ...and once verified, captcha's forced assignAutorole schedules the same delayed grant.
    let mut verified = human;
    verified.roles.push(serenity::all::RoleId::new(955008000000000021));
    svc::assign_autorole(&d.t, &verified, true).await;
    assert_eq!(d.job(user).await.map(|j| j.payload), Some(json!({ "userId": user })));
    d.done().await;
}

#[tokio::test]
async fn db_a_member_in_membership_screening_gets_a_watch_job_that_force_never_skips() {
    let Some(d) = Db::new("955008000000000506").await else { return };
    let user = "955008000000000002";
    d.set(json!({ "autoroleIds": ["955008000000000011"], "autoroleDelay": 0 })).await;
    let joined = now_ms() - 1000;
    svc::assign_autorole(&d.t, &member_in(d.g, user, joined, true), true).await;
    assert!(d.io.roles().is_empty());
    let watch = d.job(user).await.expect("watch");
    assert_eq!(watch.payload, json!({ "userId": user, "watch": true }));
    assert!(watch.run_at.timestamp_millis() > now_ms() + 5 * 3_600_000);

    let job = |payload: Value| bot_core::scheduler::Job {
        id: "W".into(),
        guild_id: d.g.into(),
        job_type: "welcome.autorole".into(),
        payload,
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 3,
        dedupe_key: None,
        version: 1,
    };
    let ctx = || bot_core::scheduler::JobCtx { app: d.t.app.clone(), guild_id: d.g.into(), late_by_ms: 0, attempt: 1 };
    d.io.set_member(member_in(d.g, user, joined, true));
    assert!(matches!(svc::run_autorole_job(job(json!({ "userId": user, "watch": true })), ctx()).await.expect("again"), bot_core::scheduler::JobOutcome::Reschedule { .. }));
    assert!(d.io.roles().is_empty());
    d.io.set_member(member_in(d.g, user, joined, false));
    assert_eq!(svc::run_autorole_job(job(json!({ "userId": user, "watch": true })), ctx()).await.expect("granted"), bot_core::scheduler::JobOutcome::Done);
    assert_eq!(d.io.roles().len(), 1);
    d.set(json!({ "autoroleDelay": 3600 })).await;
    d.io.set_member(member_in(d.g, user, now_ms(), false));
    match svc::run_autorole_job(job(json!({ "userId": user, "watch": true })), ctx()).await.expect("delayed") {
        bot_core::scheduler::JobOutcome::Reschedule { next_run_at, payload } => {
            assert_eq!(payload, Some(json!({ "userId": user })));
            assert!(next_run_at.timestamp_millis() > now_ms() + 3_000_000);
        }
        other => panic!("expected a delayed grant, got {other:?}"),
    }

    // Screening completes through guildMemberUpdate with a cached old member: granted, the watch dropped.
    d.set(json!({ "autoroleDelay": 0 })).await;
    let update = |old: Option<Member>, known: bool| bot_core::hooks::events::GuildMemberUpdate {
        guild_id: d.gid(),
        old_member: old,
        member: member_in(d.g, user, joined, false),
        old_known: known,
    };
    svc::on_member_update(std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), update(Some(member_in(d.g, user, joined, true)), true)))).await.expect("update");
    assert!(d.job(user).await.is_none());
    assert_eq!(d.io.roles().len(), 2);
    // An uncached old member is acted on only while a watch job exists.
    svc::on_member_update(std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), update(None, false)))).await.expect("update");
    assert_eq!(d.io.roles().len(), 2, "no watch job: a moderator-removed role is not re-added");
    d.done().await;
}

#[tokio::test]
async fn db_role_delete_prunes_the_autorole_list() {
    let Some(d) = Db::new("955008000000000507").await else { return };
    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000012"] })).await;
    let del = |role: u64| bot_core::hooks::events::RoleDelete { guild_id: d.gid(), role_id: serenity::all::RoleId::new(role), role: None };
    svc::on_role_delete(std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), del(955008000000000011)))).await.expect("prune");
    assert_eq!(d.fresh().await.autorole_ids, vec!["955008000000000012".to_string()]);
    svc::on_role_delete(std::sync::Arc::new(bot_core::HookCtx::new(d.t.app.clone(), del(955008000000000077)))).await.expect("noop");
    assert_eq!(d.fresh().await.autorole_ids, vec!["955008000000000012".to_string()]);
    d.done().await;
}

fn modal(g: &str, kind: &str, fields: &[(&str, &str)], admin: bool) -> serenity::all::Interaction {
    let mut f = FakeInteraction::modal(kind, fields).guild(g);
    f = if admin { f.user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD) } else { f.user("600000000000000010") };
    f.build()
}

#[tokio::test]
async fn db_the_setup_form_saves_content_and_embed_keeps_the_card_flag_refuses_stale_forms_and_non_admins() {
    let Some(d) = Db::new("955008000000000508").await else { return };
    d.t.install(registry());
    d.set(json!({ "welcomeEmbed": { "title": "Legacy", "color": 0, "thumbnail": "none", "card": false } })).await;
    let fields = [("tpl-content", "Welcome {user}!"), ("tpl-title", "Hi {usr}"), ("tpl-description", "You are {server.member_count.ordinal}"), ("tpl-color", "#123456"), ("tpl-footer", "bye")];
    let submit = |kind: &str, f: &[(&str, &str)], admin: bool| {
        let i = modal(d.g, kind, f, admin);
        let t = &d.t;
        async move {
            t.responses.clear();
            bot_core::router::route_interaction(t, i).await;
            let calls = t.responses.calls();
            assert_eq!(calls.first().map(|c| c.method), Some("deferReply"));
            assert!(!calls[0].ephemeral(), "public, the admin convention");
            let last = calls.last().expect("an answer");
            (last.embed_title().unwrap_or_default().to_string(), last.embed_description().unwrap_or_default().to_string())
        }
    };
    let (title, desc) = submit("welcome-setup", &fields, true).await;
    assert_eq!(title, "✅  Welcome Message Saved");
    assert!(desc.contains("Unknown variables: `{usr}`"), "{desc}");
    let after = d.fresh().await;
    assert_eq!(after.welcome_message.as_deref(), Some("Welcome {user}!"));
    let e = after.welcome_embed.clone().unwrap_or(Value::Null);
    assert_eq!((e["title"].clone(), e["color"].clone(), e["footer"]["text"].clone()), (json!("Hi {usr}"), json!(0x123456), json!("bye")));
    assert!(!svc::welcome_card_enabled(Some(&after)), "a card the admin switched off stays off");

    let mut bad = fields;
    bad[3] = ("tpl-color", "FFFFFFF");
    assert!(submit("welcome-setup", &bad, true).await.0.contains("Not Saved"));
    assert_eq!(d.fresh().await.welcome_embed.as_ref().map(|e| e["color"].clone()), Some(json!(0x123456)), "nothing was written");
    let long = "x".repeat(300);
    let mut too_long = fields;
    too_long[1] = ("tpl-title", &long);
    assert!(submit("welcome-setup", &too_long, true).await.1.contains("Title is 300 characters"));
    let stale = [("welcome-title", "x"), ("welcome-description", "y"), ("welcome-color", ""), ("welcome-thumbnail", ""), ("welcome-footer", "")];
    assert!(submit("welcome-setup", &stale, true).await.0.contains("Form Expired"));
    assert_eq!(d.fresh().await.welcome_message.as_deref(), Some("Welcome {user}!"), "a stale form never wipes the config");
    assert!(submit("welcome-setup", &fields, false).await.0.contains("Insufficient Permissions"));

    let emptied = [("tpl-content", ""), ("tpl-title", ""), ("tpl-description", ""), ("tpl-color", ""), ("tpl-footer", "")];
    let (_, desc) = submit("welcome-setup", &emptied, true).await;
    let wiped = d.fresh().await;
    assert_eq!(wiped.welcome_message.as_deref(), Some(""));
    assert!(wiped.welcome_embed.is_none());
    assert!(desc.contains("default embed is used"), "{desc}");

    let mut dm = fields;
    dm[0] = ("tpl-content", "psst");
    submit("welcome-dm-setup", &dm, true).await;
    let mut bye = fields;
    bye[0] = ("tpl-content", "**{user.idname}** left");
    submit("goodbye-setup", &bye, true).await;
    let last = d.fresh().await;
    assert_eq!(last.welcome_dm_message.as_deref(), Some("psst"));
    assert_eq!(last.welcome_dm_embed.as_ref().map(|e| e["title"].clone()), Some(json!("Hi {usr}")));
    assert_eq!(last.goodbye_message.as_deref(), Some("**{user.idname}** left"));
    assert_eq!(last.goodbye_embed.as_ref().map(|e| e["title"].clone()), Some(json!("Hi {usr}")));
    d.done().await;
}

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_welcome::register(&mut reg);
    reg
}

fn slash(g: &str, group: Option<&str>, sub: &str) -> FakeInteraction {
    let f = FakeInteraction::slash("welcome").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD);
    match group {
        Some(gr) => f.group(gr, sub),
        None => f.subcommand(sub),
    }
}

#[tokio::test]
async fn db_welcome_commands_through_the_router() {
    use bot_core::command::option_type as ot;
    let Some(d) = Db::new("955008000000000509").await else { return };
    d.t.install(registry());
    let run = |i: serenity::all::Interaction| {
        let t = &d.t;
        async move {
            t.responses.clear();
            bot_core::router::route_interaction(t, i).await;
            let calls = t.responses.calls();
            let last = calls.last().expect("answered");
            (calls.iter().map(|c| c.method).collect::<Vec<_>>(), last.embed_title().unwrap_or_default().to_string(), last.embed_description().unwrap_or_default().to_string())
        }
    };
    let (names, title, desc) = run(slash(d.g, None, "enable").build()).await;
    assert_eq!(names, vec!["deferReply", "editReply"]);
    assert_eq!(title, "✅  Welcome & Goodbye Enabled");
    assert!(desc.starts_with("Welcome messages are on. Still off: the welcome DM"), "{desc}");
    assert!(d.fresh().await.welcome_enabled);

    let (_, title, desc) = run(slash(d.g, None, "channel").option("channel", ot::CHANNEL, json!(CH)).build()).await;
    assert_eq!(title, "✅  Welcome Channel Set", "{desc}");
    assert_eq!(d.fresh().await.welcome_channel_id.as_deref(), Some(CH));

    let (_, title, desc) = run(slash(d.g, None, "message").option("text", ot::STRING, json!("Hi there")).build()).await;
    assert_eq!(title, "✅  Welcome Text Updated");
    assert!(desc.contains("💡 Add `{user}`"), "{desc}");
    let (_, title, _) = run(slash(d.g, None, "message").build()).await;
    assert_eq!(title, "ℹ️  Welcome Text");

    let (_, title, desc) = run(slash(d.g, Some("autorole"), "add").option("role", ot::ROLE, json!("955008000000000011")).option("delay", ot::STRING, json!("10m")).build()).await;
    assert_eq!(title, "✅  Autorole Added", "{desc}");
    assert_eq!(desc, "<@&955008000000000011> is given to new members after **10 minutes** (the delay applies to every autorole and counts from when they joined).");
    let (_, title, desc) = run(slash(d.g, Some("autorole"), "add").option("role", ot::ROLE, json!("955008000000000013")).build()).await;
    assert_eq!(title, "❌  Cannot Use This Role", "{desc}");
    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000099"] })).await;
    let (_, title, desc) = run(slash(d.g, Some("autorole"), "list").build()).await;
    assert_eq!(title, "ℹ️  Autoroles");
    assert!(desc.contains("Removed 1 deleted role from the list."), "{desc}");
    assert_eq!(d.fresh().await.autorole_ids, vec!["955008000000000011".to_string()]);
    let (_, title, _) = run(slash(d.g, Some("autorole"), "remove").option("role", ot::STRING, json!("nobody")).build()).await;
    assert_eq!(title, "❌  Not an Autorole");
    let (_, title, desc) = run(slash(d.g, Some("autorole"), "delay").option("duration", ot::STRING, json!("9d")).build()).await;
    assert_eq!((title.as_str(), desc.as_str()), ("❌  Not Saved", "The longest autorole delay is 7 days."));

    let (_, title, desc) = run(slash(d.g, Some("goodbye"), "enable").build()).await;
    assert_eq!(title, "✅  Goodbye Enabled");
    assert!(desc.contains("No goodbye channel is set"), "{desc}");
    let (_, title, _) = run(slash(d.g, None, "status").build()).await;
    assert_eq!(title, "ℹ️  Welcome & Goodbye");

    // The setup form is the first answer; the preview goes through the same payload builder.
    let (names, _, _) = run(slash(d.g, None, "setup").build()).await;
    assert_eq!(names, vec!["showModal"]);
    let (names, _, _) = run(slash(d.g, Some("goodbye"), "test").build()).await;
    assert_eq!(names.first().map(|n| n.to_string()).as_deref(), Some("deferReply"));

    // Autocomplete: the configured autoroles, deleted ones included.
    d.set(json!({ "autoroleIds": ["955008000000000011", "955008000000000088"] })).await;
    d.t.responses.clear();
    let ac = FakeInteraction::autocomplete("welcome").guild(d.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).group("autorole", "remove").focused("role", "").build();
    bot_core::router::route_interaction(&d.t, ac).await;
    let choices = d.t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(json!(choices), json!([{ "name": "@One", "value": "955008000000000011" }, { "name": "Deleted role (955008000000000088)", "value": "955008000000000088" }]));
    d.done().await;
}

// ── Cross-language: the JS service writes, Rust reads; Rust writes, the JS reads ──

#[tokio::test]
async fn js_writes_welcome_settings_and_a_job_and_rust_reads_and_runs_them() {
    let Some(d) = Db::new("955008000000000510").await else { return };
    let user = "955008000000000002";
    let pending_user = "955008000000000003";
    let js = run_node("rust/fixtures/gen/feat-welcome-js.mjs", &["welcome-write", d.g, user, "955008000000000011", "955008000000000012", pending_user]);
    assert!(js.get("error").is_none(), "{js}");
    // The JS screening watch: same key, `{ userId, watch: true }`, polled 6 h after it was scheduled; Rust keeps
    // polling it while the member is still pending.
    let watch = d.job(pending_user).await.expect("the JS watch job");
    assert_eq!(watch.payload, json!({ "userId": pending_user, "watch": true }));
    let at = watch.run_at.timestamp_millis() - js["before"].as_i64().unwrap_or(0);
    assert!((svc::SCREENING_POLL_MS - 1_000..=svc::SCREENING_POLL_MS + 60_000).contains(&at), "{at}");
    d.io.set_member(member_in(d.g, pending_user, js["joined"].as_i64().unwrap_or(0), true));
    let ctx = bot_core::scheduler::JobCtx { app: d.t.app.clone(), guild_id: d.g.into(), late_by_ms: 0, attempt: 1 };
    let polled = svc::run_autorole_job(bot_core::scheduler::Job::from_row(&watch), ctx).await;
    assert!(matches!(polled.expect("polled"), bot_core::scheduler::JobOutcome::Reschedule { payload: None, .. }));
    let s = d.fresh().await;
    assert_eq!(serde_json::to_value(&*s).ok().map(|v| v["welcomeEmbed"].clone()), Some(js["settings"]["welcomeEmbed"].clone()));
    assert!(!svc::welcome_card_enabled(Some(&s)), "the legacy card:false moved into the style");
    let style = svc::parse_card_style(s.welcome_card_style.as_ref());
    assert_eq!((style.background.as_deref(), style.accent_color.as_deref(), style.overlay_opacity), (Some("#abcdef"), Some("#5865f2"), Some(0.46)));
    assert_eq!(s.welcome_message.as_deref(), Some(""), "the JS froze the legacy text");
    assert_eq!(svc::resolve_template(Kind::Dm, Some(&s), false).content.as_deref(), Some("Psst {user.name}"));
    assert_eq!(s.autorole_ids, vec!["955008000000000011".to_string(), "955008000000000012".to_string()]);
    assert_eq!(s.autorole_delay, 600);
    // The JS job: same key, type and payload; Rust runs it.
    let job = d.job(user).await.expect("the JS job");
    assert_eq!(job.payload, json!({ "userId": user }));
    assert_eq!(job.run_at.timestamp_millis(), js["joined"].as_i64().unwrap_or(0) + 600_000);
    d.io.set_member(member_in(d.g, user, js["joined"].as_i64().unwrap_or(0), false));
    let outcome = svc::run_autorole_job(bot_core::scheduler::Job::from_row(&job), bot_core::scheduler::JobCtx { app: d.t.app.clone(), guild_id: d.g.into(), late_by_ms: 0, attempt: 1 }).await;
    assert_eq!(outcome.expect("ran"), bot_core::scheduler::JobOutcome::Done);
    assert_eq!(d.io.roles().len(), 2);
    d.done().await;
}

#[tokio::test]
async fn rust_writes_welcome_settings_and_a_job_and_the_js_reads_them() {
    let Some(d) = Db::new("955008000000000511").await else { return };
    let user = "955008000000000002";
    d.set(json!({ "welcomeEmbed": { "title": "Old", "thumbnail": "user_avatar", "card": false } })).await;
    svc::update_embed_options(&d.t, d.g, Kind::Welcome, svc::EmbedOptions { thumbnail: Some("server".into()), image_url: Some("{user.avatar}".into()), ..Default::default() })
        .await
        .expect("embed");
    svc::update_embed_options(&d.t, d.g, Kind::Goodbye, svc::EmbedOptions { enabled: Some(true), thumbnail: Some("avatar".into()), ..Default::default() }).await.expect("bye");
    svc::update_card_style(&d.t, d.g, svc::CardStyleOptions { text_color: Some("#FFEEDD".into()), overlay: Some(1.0), ..Default::default() }).await.expect("style");
    svc::add_autorole(&d.t, d.gid(), None, serenity::all::RoleId::new(955008000000000011), Some("1h")).await.expect("autorole");
    let joined = now_ms() - 120_000;
    svc::assign_autorole(&d.t, &member_in(d.g, user, joined, false), false).await;
    let pending_user = "955008000000000003";
    let before = now_ms();
    svc::assign_autorole(&d.t, &member_in(d.g, pending_user, joined, true), true).await;

    let js = run_node("rust/fixtures/gen/feat-welcome-js.mjs", &["welcome-read", d.g, user, pending_user]);
    assert!(js.get("error").is_none(), "{js}");
    let ours = serde_json::to_value(&*d.fresh().await).unwrap_or(Value::Null);
    for k in ["welcomeEmbed", "goodbyeEmbed", "goodbyeUseEmbed", "welcomeCardStyle", "welcomeMessage", "autoroleIds", "autoroleDelay"] {
        same(&ours[k], &js["settings"][k], k);
    }
    assert_eq!(js["cardEnabled"], json!(false));
    same(&js["style"], &svc::parse_card_style(d.fresh().await.welcome_card_style.as_ref()).to_json(), "style");
    let s = d.fresh().await;
    for (k, kind) in [("welcome", Kind::Welcome), ("goodbye", Kind::Goodbye), ("dm", Kind::Dm)] {
        let r = svc::resolve_template(kind, Some(&s), false);
        same(&json!({ "content": r.content, "embed": r.embed }), &js[k], k);
    }
    assert_eq!(js["job"]["type"], "welcome.autorole");
    assert_eq!(js["job"]["dedupeKey"], svc::autorole_job_key(d.g, user));
    assert_eq!(js["job"]["payload"], json!({ "userId": user }));
    let run_at = js["job"]["runAt"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp_millis());
    assert_eq!(run_at, Some(joined + 3_600_000));
    assert_eq!(js["watch"]["type"], "welcome.autorole");
    assert_eq!(js["watch"]["dedupeKey"], svc::autorole_job_key(d.g, pending_user));
    assert_eq!(js["watch"]["payload"], json!({ "userId": pending_user, "watch": true }));
    let watch_at = js["watch"]["runAt"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp_millis() - before).unwrap_or(0);
    assert!((svc::SCREENING_POLL_MS - 1_000..=svc::SCREENING_POLL_MS + 60_000).contains(&watch_at), "{watch_at}");
    d.done().await;
}

#[tokio::test]
async fn the_serenity_io_degrades_offline_without_panicking() {
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[send_guild()]);
    let member = test_member();
    // No fake installed: the real HTTP client, which cannot reach Discord in tests.
    let style = json!({ "welcomeChannelId": CH, "welcomeCardStyle": { "v": 1, "enabled": false } });
    svc::send_welcome_channel(&t, &member, &on(style), &svc::CardMemo::fixed(None)).await;
    svc::send_welcome_dm(&t, &member, &on(json!({ "welcomeDmEnabled": true })), &svc::CardMemo::fixed(None)).await;
    let io = feat_welcome::io::io(&t);
    assert!(io.fetch_member(gid(), member.user.id).await.is_err());
    assert!(matches!(io.validate_background("not a url".into()).await, bot_cards::welcome_card::BackgroundCheck::Invalid { .. }));
}

// ── Channels missing from the cache are fetched over REST (`guild.channels.fetch`) ──

const CH_LATE: &str = "500000000000000099";
const CH_LATE_BYE: &str = "500000000000000097";

fn late_channel_json() -> Value {
    json!({ "id": CH_LATE, "type": 0, "guild_id": GID, "name": "late", "position": 9, "permission_overwrites": [], "nsfw": false, "parent_id": null })
}

#[tokio::test]
async fn an_uncached_welcome_or_goodbye_channel_is_fetched_over_rest() {
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds_rest(&[send_guild()], &rest);
    let io = FakeIo::install(&t);
    rest.on("GET", &format!("/channels/{CH_LATE}"), 200, late_channel_json());
    let member = test_member();
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": CH_LATE })), &svc::CardMemo::fixed(None)).await;
    let sent = io.sent();
    assert_eq!(sent.len(), 1, "the JS resolveChannel falls back to guild.channels.fetch");
    assert_eq!(sent[0].0, CH_LATE);
    assert_eq!(sent[0].1["content"], WELCOME_TEXT);
    // The goodbye channel is another uncached one (CH_LATE is cached by now).
    let mut bye = late_channel_json();
    bye["id"] = json!(CH_LATE_BYE);
    rest.on("GET", &format!("/channels/{CH_LATE_BYE}"), 200, bye);
    svc::send_goodbye(&t, gid(), Some(&member), &member.user, &on(json!({ "goodbyeEnabled": true, "goodbyeChannelId": CH_LATE_BYE }))).await;
    assert_eq!(io.sent().len(), 2);
    assert_eq!(io.sent()[1].0, CH_LATE_BYE);
    // A channel Discord does not know is still "missing".
    svc::send_welcome_channel(&t, &member, &on(json!({ "welcomeChannelId": "500000000000000098" })), &svc::CardMemo::fixed(None)).await;
    assert_eq!(io.sent().len(), 2);
}

#[tokio::test]
async fn check_config_channel_fetches_an_uncached_channel() {
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds_rest(&[send_guild()], &rest);
    rest.on("GET", &format!("/channels/{CH_LATE}"), 200, late_channel_json());
    let late = serenity::all::ChannelId::new(CH_LATE.parse().expect("id"));
    assert_eq!(svc::check_config_channel(&t, gid(), late, &["ViewChannel", "SendMessages"]).await, Ok(()));
    let mute = serenity::all::ChannelId::new(CH_MUTE.parse().expect("id"));
    let refused = svc::check_config_channel(&t, gid(), mute, &["ViewChannel", "SendMessages"]).await.expect_err("mute");
    assert!(refused.contains("Send Messages"), "{refused}");
}

/// `guild.roles.cache.get(picked.id) ?? picked`: a role Discord resolved in the option but the cache lacks is checked
/// as that role (here: refused as dangerous), not answered with "Role not found.".
#[tokio::test]
async fn autorole_add_checks_the_resolved_role_when_the_cache_lacks_it() {
    let (t, _io) = send_app();
    let role: serenity::all::Role = serde_json::from_value(json!({
        "id": "500000000000000077", "guild_id": GID, "name": "Mods", "color": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }, "hoist": false, "position": 1,
        "permissions": Permissions::BAN_MEMBERS.bits().to_string(), "managed": false, "mentionable": false,
    }))
    .expect("role");
    let err = svc::add_autorole_resolved(&t, gid(), None, role.id, Some(&role), None).await.expect_err("refused");
    let text = err.downcast_ref::<svc::WelcomeConfigError>().map(|e| e.message.clone()).unwrap_or_default();
    assert!(text.contains("Autoroles are given to everyone who joins"), "{text}");
    let err = svc::add_autorole(&t, gid(), None, role.id, None).await.expect_err("unknown");
    let text = err.downcast_ref::<svc::WelcomeConfigError>().map(|e| e.message.clone()).unwrap_or_default();
    assert!(text.starts_with("Role not found."), "{text}");
}
