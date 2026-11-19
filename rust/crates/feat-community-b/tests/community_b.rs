//! Port of `test/modmail.test.js`, `test/boosts.test.js` and `test/auto-threads.test.js`, plus the golden, database
//! and cross-language checks.
//!
//! Offline: the pure rules against sweeps the JS produced (`rust/fixtures/gen/feat-community-b-golden.mjs`), the
//! payloads and customIds, the hook filters, the command answers that end before any Discord REST call, and the
//! declarations. With `TEST_DATABASE_URL`: the `ModmailThread` and `BoosterRole` access, the settings lists, the sweep
//! job, the command paths that use them, and both directions against the JS services
//! (`rust/fixtures/gen/feat-community-b-js.mjs`).

use std::sync::OnceLock;

use bot_core::prelude::*;
use bot_core::testing::{FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp};
use bot_db::GuildSettings;
use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser};
use feat_community_b::auto_thread_service as at;
use feat_community_b::boost_service as bs;
use feat_community_b::modmail_service as mm;
use feat_community_b::plugins::modmail as mmp;
use serde_json::{Value, json};
use serenity::all::Permissions as P;
use serenity::builder::CreateEmbed;

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_community_b::register(&mut reg);
    reg
}

fn pure() -> &'static Value {
    static PURE: OnceLock<Value> = OnceLock::new();
    PURE.get_or_init(|| bot_core::testing::load_golden("feat-community-b/pure.json"))
}

fn sweep(name: &str) -> Vec<(Value, Value)> {
    let cases = pure()[name].as_array().unwrap_or_else(|| panic!("no sweep {name}"));
    assert!(!cases.is_empty(), "empty sweep {name}");
    cases.iter().map(|c| (c["in"].clone(), c["out"].clone())).collect()
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// An embed as the golden script records it.
fn embed_json(e: &CreateEmbed) -> Value {
    let j = serde_json::to_value(e).unwrap_or(Value::Null);
    json!({
        "title": j.get("title").cloned().unwrap_or(Value::Null),
        "description": j.get("description").cloned().unwrap_or(Value::Null),
        "color": j.get("color").cloned().unwrap_or(Value::Null),
        "footer": j.get("footer").and_then(|f| f.get("text")).cloned().unwrap_or(Value::Null),
        "author": j.get("author").and_then(|f| f.get("name")).cloned().unwrap_or(Value::Null),
        "fields": j.get("fields").and_then(Value::as_array).map(|fs| fs.iter().map(|f| json!([f["name"], f["value"]])).collect::<Vec<_>>()).unwrap_or_default(),
    })
}

/// The buttons and selects of a component row list, reduced to what both libraries agree on.
fn components_json(rows: &Value) -> Value {
    let reduce = |c: &Value| {
        json!({
            "type": c["type"], "custom_id": c["custom_id"], "label": c.get("label").cloned().unwrap_or(Value::Null), "style": c.get("style").cloned().unwrap_or(Value::Null),
            "emoji": c.get("emoji").and_then(|e| e.get("name")).cloned().unwrap_or(Value::Null),
            "placeholder": c.get("placeholder").cloned().unwrap_or(Value::Null),
            "options": c.get("options").and_then(Value::as_array).map(|o| o.iter().map(|x| json!([x["label"], x["value"]])).collect::<Vec<_>>()).unwrap_or_default(),
        })
    };
    Value::Array(rows.as_array().into_iter().flatten().map(|r| Value::Array(r["components"].as_array().into_iter().flatten().map(reduce).collect())).collect())
}

fn message_json(m: &serenity::builder::CreateMessage) -> Value {
    let j = serde_json::to_value(m).unwrap_or(Value::Null);
    let embeds: Vec<Value> = j["embeds"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            json!({
                "title": e.get("title").cloned().unwrap_or(Value::Null),
                "description": e.get("description").cloned().unwrap_or(Value::Null),
                "color": e.get("color").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "embeds": embeds, "components": components_json(&j["components"]) })
}

fn golden_message(v: &Value) -> Value {
    let embeds: Vec<Value> = v["embeds"].as_array().into_iter().flatten().map(|e| json!({ "title": e["title"], "description": e["description"], "color": e["color"] })).collect();
    json!({ "embeds": embeds, "components": components_json(&v["components"]) })
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["modmail", "boost", "autothread"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "modmail");
    bot_core::testing::assert_plugin_matches_golden(&reg, "boosts");
    bot_core::testing::assert_plugin_matches_golden(&reg, "auto-threads");
}

#[test]
fn hook_order_matches_the_js_comments() {
    let at_ = |e: &str, id: &str| bot_config::hook_index(e, id);
    assert!(at_("messageCreate", "autotime") < at_("messageCreate", "modmail"), "modmail after autotime");
    assert!(at_("messageCreate", "automod") < at_("messageCreate", "auto-threads"));
    assert!(at_("messageCreate", "auto-threads") < at_("messageCreate", "sticky"), "auto-threads before sticky");
    assert!(at_("directMessage", "modmail").is_some());
    assert!(at_("threadDelete", "modmail").is_some());
    for e in ["guildMemberUpdate", "guildMemberRemove", "guildReady", "channelDelete", "roleDelete"] {
        assert!(at_(e, "boosts").is_some(), "{e}");
    }
    let meta = feat_community_b::commands::boost::command().metadata();
    assert_eq!(meta["subcommandPermissions"], json!({ "config": 2, "test": 2, "list": 2, "remove": 2 }));
    let payload = feat_community_b::commands::boost::command().payload();
    assert!(payload.get("default_member_permissions").is_none(), "boosters must see /boost role");
}

#[test]
fn custom_ids_match_the_js() {
    for (input, out) in sweep("mm.customIds") {
        let parts: Vec<String> = input.as_array().into_iter().flatten().filter_map(s).collect();
        let built = bot_config::build_custom_id(&parts[0], &parts[1..]).expect("known prefix");
        assert_eq!(Value::from(built), out, "{input}");
    }
    let row = serde_json::to_value(at::close_row("600000000000000001")).unwrap_or(Value::Null);
    assert_eq!(row["components"][0]["custom_id"], "autothread:close:600000000000000001");
    assert_eq!(row["components"][0]["label"], "Close thread");
    assert_eq!(row["components"][0]["style"], 2);
    let close = serde_json::to_value(mmp::close_button_row("clrow1")).unwrap_or(Value::Null);
    assert_eq!(close["components"][0]["custom_id"], "modmail:close:clrow1");
    assert_eq!(close["components"][0]["label"], "Close conversation");
    assert_eq!(bs::sweep_key("900000000000000001"), "boosts.sweep:900000000000000001");
    assert_eq!(mm::open_lock_key("G", "U"), "modmail-open:G:U");
}

// ── Modmail: pure rules ───────────────────────────────────────────────────────

#[test]
fn modmail_constants_match_the_js() {
    let c = &sweep("mm.constants")[0].1;
    assert_eq!(c["DEFAULT_GREETING"], mm::DEFAULT_GREETING);
    assert_eq!(c["STAFF_HELP"], mm::STAFF_HELP);
    let m = &c["MODMAIL"];
    assert_eq!(m["NOTE_PREFIX"], mm::limits::NOTE_PREFIX);
    assert_eq!(m["TEXT_MAX"], mm::limits::TEXT_MAX);
    assert_eq!(m["REASON_MAX"], mm::limits::REASON_MAX);
    assert_eq!(m["PENDING_PREFIX"], mm::limits::PENDING_PREFIX);
    assert_eq!(m["PENDING_STALE_MS"], mm::limits::PENDING_STALE_MS);
    assert_eq!(m["RATE_MAX"], mm::limits::RATE_MAX);
    assert_eq!(m["RATE_WINDOW_MS"], mm::limits::RATE_WINDOW_MS);
    assert_eq!(m["PROMPT_EVERY_MS"], mm::limits::PROMPT_EVERY_MS);
    assert_eq!(m["PICK_MAX"], mm::limits::PICK_MAX);
    assert_eq!(m["CATCH_UP_MAX"], mm::limits::CATCH_UP_MAX);
    assert_eq!(m["CATCH_UP_SCAN"], mm::limits::CATCH_UP_SCAN);
    assert_eq!(m["BLOCKED_LIST_MAX"], mm::limits::BLOCKED_LIST_MAX);
}

#[test]
fn is_internal_note_matches_the_js() {
    for (input, out) in sweep("mm.isInternalNote") {
        assert_eq!(Value::from(mm::is_internal_note(input.as_str())), out, "{input}");
    }
    assert!(!mm::is_internal_note(None));
}

#[test]
fn thread_name_is_dash_safe_like_the_js() {
    for (input, out) in sweep("mm.threadName") {
        assert_eq!(Value::from(mm::thread_name(input.as_str())), out, "{input}");
    }
    assert!(mm::thread_name(Some(&"a".repeat(300))).chars().count() <= 100);
}

fn relay_of(input: &Value) -> mm::RelayMessage {
    mm::RelayMessage {
        content: s(&input["content"]).unwrap_or_default(),
        attachments: input["attachments"].as_array().into_iter().flatten().map(|a| (s(&a[0]), s(&a[1]).unwrap_or_default())).collect(),
        stickers: input["stickers"].as_array().into_iter().flatten().map(s).collect(),
    }
}

#[test]
fn attachments_stickers_and_long_text_relay_like_the_js() {
    for (input, out) in sweep("mm.relay") {
        let m = relay_of(&input);
        assert_eq!(json!(mm::attachment_lines(&m)), out["lines"], "{input}");
        let parts = mm::relay_parts(&m);
        assert_eq!(Value::from(parts.description), out["parts"]["description"], "{input}");
        assert_eq!(json!(parts.attachments), out["parts"]["attachments"], "{input}");
        assert_eq!(Value::from(mm::has_content(&m)), out["has"], "{input}");
    }
}

#[test]
fn who_lines_and_staff_author_match_the_js() {
    for (input, out) in sweep("mm.whoLines") {
        let lines = mm::who_lines(input[0].as_str().unwrap_or(""), input[1].as_f64(), input[2].as_f64());
        assert_eq!(json!(lines), out, "{input}");
    }
    for (input, out) in sweep("mm.staffAuthor") {
        let a = mm::staff_author(input[0].as_str().map(|n| (n, None)), Some("icon".into()), input[1].as_bool().unwrap_or(false));
        assert_eq!(Value::from(a.name), out, "{input}");
    }
    let anon = mm::staff_author(Some(("sam", Some("a".into()))), Some("g".into()), true);
    assert_eq!(anon.icon_url.as_deref(), Some("g"), "anonymous replies show the server icon");
}

#[test]
fn modmail_embeds_and_prompts_match_the_js() {
    for (input, out) in sweep("mm.closedEmbed") {
        let e = mmp::closed_embed(input[0].as_str().unwrap_or(""), input[1].as_str());
        let j = embed_json(&e);
        assert_eq!((&j["title"], &j["description"], &j["color"]), (&out["title"], &out["description"], &out["color"]), "{input}");
    }
    for (input, out) in sweep("mm.confirmPayload") {
        let m = mmp::confirm_payload(input[0].as_str().unwrap_or(""), input[1].as_str().unwrap_or(""), input[2].as_str().unwrap_or(""));
        assert_eq!(message_json(&m), golden_message(&out));
    }
    let servers = |n: u64| -> Vec<(String, String)> {
        (0..n)
            .map(|i| {
                let name = if i == 1 { String::new() } else { format!("Server {i} {}", if i == 2 { "x".repeat(120) } else { String::new() }) };
                (name, format!("9000000000000000{i:02}"))
            })
            .collect()
    };
    for (input, out) in sweep("mm.openPickPayload") {
        let m = mmp::open_pick_payload(&servers(input.as_u64().unwrap_or(0)), "700000000000000002");
        assert_eq!(message_json(&m), golden_message(&out), "{input} servers");
    }
    for (input, out) in sweep("mm.relayPickPayload") {
        let convs: Vec<(String, String)> = servers(input.as_u64().unwrap_or(0)).into_iter().enumerate().map(|(i, (n, _))| (n, format!("row{i}"))).collect();
        assert_eq!(message_json(&mmp::relay_pick_payload(&convs, "700000000000000003")), golden_message(&out));
    }
    let author = mmp::RelayAuthor { id: "U9".into(), name: "Dee".into(), avatar_url: Some("https://cdn.example/av.png".into()) };
    for (input, out) in sweep("mm.memberMessageEmbed") {
        assert_eq!(embed_json(&mmp::member_message_embed(None, &author, &relay_of(&input))), out, "{input}");
    }
    let mut colored = default_settings();
    colored.embed_color = Some(0x123456);
    let staff = mm::StaffAuthor { name: "Staff".into(), icon_url: None };
    for (input, out) in sweep("mm.staffMessageEmbed") {
        assert_eq!(embed_json(&mmp::staff_message_embed(Some(&colored), "The Guild", None, &staff, &relay_of(&input))), out, "{input}");
    }
}

#[test]
fn modmail_texts() {
    assert_eq!(mmp::sent_text(1, 0, "to the staff."), "Your message was sent to the staff.");
    assert_eq!(mmp::sent_text(3, 2, "to the staff."), "Your 3 messages were sent to the staff. 2 could not be delivered (marked ❌).");
    assert_eq!(mmp::close_notice(Some("U1"), Some("done")), "Conversation closed by <@U1>.\n**Reason:** done");
    assert_eq!(mmp::close_notice(None, None), "Conversation closed.");
    let e = embed_json(&mmp::dm_text("OPENING"));
    assert_eq!(e["title"], "ℹ️  One Moment");
    assert_eq!(embed_json(&mmp::dm_text("BLOCKED"))["description"], "You cannot contact that server through me right now.");
    assert_eq!(embed_json(&mmp::dm_text("CREATE_FAILED"))["title"], "❌  Try Again Later");
    let (text, footer) = feat_community_b::commands::modmail::blocked_list(&[]);
    assert_eq!((text.as_str(), footer.as_str()), ("Nobody is blocked.", "0 of 500"));
    let ids: Vec<String> = (0..53).map(|i| format!("6000000000000000{i:02}")).collect();
    let (text, footer) = feat_community_b::commands::modmail::blocked_list(&ids);
    assert!(text.ends_with("… and 3 more"), "{text}");
    assert_eq!(footer, "53 of 500");
    assert_eq!(
        feat_community_b::commands::modmail::close_text("U1", Some("spam"), false),
        "Closed by <@U1>.\n**Reason:** spam\nThe member could not be told (their DMs are closed)."
    );
    assert_eq!(
        feat_community_b::commands::modmail::setup_text("C1", &[], &["w".into()]),
        "Modmail is on. Members can DM me; each conversation opens as a thread in <#C1>.\nNo role is pinged yet.\nKeep the channel staff-only; members never need access to it.\n⚠️ w"
    );
}

fn default_settings() -> GuildSettings {
    bot_core::settings::MemorySettingsStore::new_row("900000000000000001", serde_json::Map::new()).expect("default row")
}

fn settings_with(f: impl FnOnce(&mut GuildSettings)) -> GuildSettings {
    let mut s = default_settings();
    f(&mut s);
    s
}

#[test]
fn blocked_members_and_accepts_conversations() {
    let s = settings_with(|s| {
        s.modmail_enabled = true;
        s.modmail_channel_id = Some("C".into());
        s.modmail_blocked_user_ids = vec!["B".into()];
    });
    assert!(mm::is_blocked(Some(&s), "B"));
    assert!(!mm::is_blocked(Some(&s), "A"));
    assert!(!mm::is_blocked(None, "A"));
    assert!(mm::accepts_conversations(Some(&s), "A"));
    assert!(!mm::accepts_conversations(Some(&s), "B"), "blocked");
    let mut off = s.clone();
    off.modmail_enabled = false;
    assert!(!mm::accepts_conversations(Some(&off), "A"), "module off");
    let mut none = s.clone();
    none.modmail_channel_id = None;
    assert!(!mm::accepts_conversations(Some(&none), "A"), "no channel");
    assert!(!mm::accepts_conversations(None, "A"));
    assert!(mm::is_pending_thread_id("pending-abc"));
    assert!(!mm::is_pending_thread_id("123"));
}

#[test]
fn is_staff_author_moderator_ping_role_or_thread_manager() {
    let s = settings_with(|s| {
        s.mod_role_ids = vec!["MODROLE".into()];
        s.modmail_ping_role_ids = vec!["PING".into()];
    });
    use bot_config::PermissionLevel as L;
    assert!(mm::is_staff_author(Some(L::Moderator), &[], Some(&s), Some(P::empty())));
    assert!(mm::is_staff_author(Some(L::Member), &["PING".into()], Some(&s), Some(P::empty())));
    assert!(mm::is_staff_author(Some(L::Member), &[], Some(&s), Some(P::MANAGE_THREADS)));
    assert!(mm::is_staff_author(Some(L::Member), &[], Some(&s), Some(P::ADMINISTRATOR)), "Administrator has every permission");
    assert!(!mm::is_staff_author(Some(L::Member), &["OTHER".into()], Some(&s), Some(P::SEND_MESSAGES)));
    assert!(!mm::is_staff_author(None, &[], Some(&s), Some(P::all())), "no member");
}

#[tokio::test]
async fn within_rate_cuts_a_flood() {
    let t = TestApp::new();
    let now = 1_000_000;
    for i in 0..mm::limits::RATE_MAX {
        assert!(mmp::within_rate(&t, "U", now + i64::from(i)), "message {i}");
    }
    assert!(!mmp::within_rate(&t, "U", now + 20), "the 11th in the window");
    assert!(mmp::within_rate(&t, "OTHER", now + 20), "per member");
    assert!(mmp::within_rate(&t, "U", now + mm::limits::RATE_WINDOW_MS), "a new window");
}

// ── Boosts: pure rules ────────────────────────────────────────────────────────

#[test]
fn boost_constants_match_the_js() {
    let c = &sweep("bs.constants")[0].1;
    assert_eq!(c["DEFAULT_BOOST_MESSAGE"], bs::DEFAULT_BOOST_MESSAGE);
    let b = &c["BOOST"];
    assert_eq!(b["JOB_TYPE"], bs::consts::JOB_TYPE);
    assert_eq!(b["GRACE_MAX_DAYS"], bs::consts::GRACE_MAX_DAYS);
    assert_eq!(b["SWEEP_EVERY_MS"], bs::consts::SWEEP_EVERY_MS);
    assert_eq!(b["SWEEP_MORE_MS"], bs::consts::SWEEP_MORE_MS);
    assert_eq!(b["SWEEP_RETRY_MS"], bs::consts::SWEEP_RETRY_MS);
    assert_eq!(b["LOOKUPS_PER_RUN"], bs::consts::LOOKUPS_PER_RUN);
    assert_eq!(b["DELETES_PER_RUN"], bs::consts::DELETES_PER_RUN);
    assert_eq!(b["SWEEP_BUDGET_MS"], bs::consts::SWEEP_BUDGET_MS);
    assert_eq!(b["EDIT_COOLDOWN_MS"], bs::consts::EDIT_COOLDOWN_MS);
    assert_eq!(b["EMOJI_MAX"], bs::consts::EMOJI_MAX);
    assert_eq!(b["MESSAGE_MAX"], bs::consts::MESSAGE_MAX);
    assert_eq!(b["ROLES_PER_GUILD"], bs::consts::ROLES_PER_GUILD);
}

#[test]
fn parse_color_and_emoji_match_the_js() {
    for (input, out) in sweep("bs.parseColor") {
        let got = match bs::parse_color(input.as_str()) {
            Ok(v) => json!({ "ok": true, "value": v }),
            Err(bs::BadColor) => json!({ "ok": false }),
        };
        assert_eq!(got, out, "{input}");
    }
    for (input, out) in sweep("bs.parseEmoji") {
        let got = match bs::parse_emoji(input.as_str()) {
            Some(e) => json!({ "ok": true, "emoji": e }),
            None => json!({ "ok": false }),
        };
        assert_eq!(got, out, "{input}");
    }
}

#[test]
fn validate_role_name_matches_the_js() {
    let roles = [("R0", "Moderator"), ("R1", "Admin Team"), ("R2", "@everyone"), ("R3", "VIP ★")];
    for (input, out) in sweep("bs.validateRoleName") {
        let got = match bs::validate_role_name(input[0].as_str(), roles.iter().copied(), input[1].as_str()) {
            Ok(name) => json!({ "ok": true, "name": name }),
            Err(r) => json!({ "ok": false, "reason": r.as_str() }),
        };
        assert_eq!(got, out, "{input}");
    }
}

#[test]
fn plan_sweep_matches_the_js() {
    let day = bs::consts::DAY_MS;
    let now = 10 * day;
    for (input, out) in sweep("bs.planSweep") {
        let ended = input["ended"].as_i64().map(|d| now - d * day);
        let member = match input["member"].as_str() {
            Some("unknown") => bs::MemberState::Unknown,
            Some("left") => bs::MemberState::Left,
            Some(m) => bs::MemberState::Present { boosting: m == "boosting" },
            None => bs::MemberState::Unknown,
        };
        let settings = settings_with(|s| s.boost_role_grace_days = input["grace"].as_i64().unwrap_or(0) as i32);
        let plan = bs::plan_sweep(ended, member, Some(&settings), now);
        let got = match plan {
            bs::Plan::Delete(why) => json!({ "type": "delete", "why": why }),
            p => json!({ "type": p.kind() }),
        };
        assert_eq!(got, out, "{input}");
    }
    for (input, out) in sweep("bs.graceMs") {
        let s = settings_with(|s| s.boost_role_grace_days = input.as_i64().unwrap_or(0) as i32);
        assert_eq!(Value::from(bs::grace_ms(Some(&s))), out, "{input}");
    }
    assert_eq!(bs::grace_ms(None), 0);
}

#[test]
fn thank_you_matches_the_js() {
    for (input, out) in sweep("bs.messageTemplate") {
        let s = settings_with(|s| s.boost_message = s_opt(&input));
        assert_eq!(Value::from(bs::message_template(Some(&s))), out, "{input}");
    }
    for (input, out) in sweep("bs.renderThankYou") {
        let boosts = input["boosts"].as_u64().unwrap_or(0);
        let tier = input["tier"].as_u64().unwrap_or(0) as u8;
        let guild = PlaceholderGuild {
            id: "G1".into(),
            name: Some("Test Guild".into()),
            member_count: Some(42.0),
            premium_subscription_count: Some(boosts.to_string()),
            premium_tier: Some(tier.to_string()),
            ..Default::default()
        };
        let user = PlaceholderUser { id: "U1".into(), username: Some("booster".into()), ..Default::default() };
        let member = PlaceholderMember { display_name: Some("Boo".into()), user: Some(user), guild: Some(guild.clone()), ..Default::default() };
        let src = bs::ThankYouSource { member: Some(member), guild: Some(guild), boosts, tier };
        let s = settings_with(|s| s.boost_message = s_opt(&input["msg"]));
        let got = bs::render_thank_you(Some(&s), &src, input["max"].as_u64().unwrap_or(2000) as usize, input["preview"].as_bool().unwrap_or(false));
        assert_eq!(Value::from(got), out, "{input}");
    }
}

fn s_opt(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

#[test]
fn problem_embeds_match_the_js() {
    for (input, out) in sweep("bs.problemEmbed") {
        let code: &'static str = Box::leak(input["code"].as_str().unwrap_or("").to_string().into_boxed_str());
        let d = &input["detail"];
        let detail = if let Some(ms) = d.get("ms").and_then(Value::as_i64) {
            Some(bs::Detail::Ms(ms))
        } else if let Some(r) = d.get("reason").and_then(Value::as_str) {
            let reason = match r {
                "too-long" => bs::NameReason::TooLong,
                "at" => bs::NameReason::At,
                "link" => bs::NameReason::Link,
                "taken" => bs::NameReason::Taken,
                _ => bs::NameReason::Empty,
            };
            Some(bs::Detail::Name(reason))
        } else {
            d.get("text").and_then(Value::as_str).map(|t| bs::Detail::Text(t.into()))
        };
        let p = bs::Problem { code, detail };
        let j = embed_json(&bs::problem_embed(&p));
        assert_eq!((&j["title"], &j["description"], &j["color"]), (&out["title"], &out["description"], &out["color"]), "{input}");
    }
}

#[test]
fn boost_transitions_and_member_problems() {
    use bs::Transition::*;
    assert_eq!(bs::boost_transition(false, true), Some(Start));
    assert_eq!(bs::boost_transition(true, false), Some(End));
    assert_eq!(bs::boost_transition(true, true), None);
    assert_eq!(bs::boost_transition(false, false), None);
    assert!(!bs::is_boosting(None));
    let on = settings_with(|s| {
        s.boosts_enabled = true;
        s.boost_custom_roles = true;
    });
    assert_eq!(bs::member_problem(None, true, true).map(|p| p.code), Some("SETTINGS"));
    let mut off = on.clone();
    off.boosts_enabled = false;
    assert_eq!(bs::member_problem(Some(&off), true, true).map(|p| p.code), Some("MODULE_OFF"));
    let mut roles_off = on.clone();
    roles_off.boost_custom_roles = false;
    assert_eq!(bs::member_problem(Some(&roles_off), true, true).map(|p| p.code), Some("ROLES_OFF"));
    assert_eq!(bs::member_problem(Some(&on), false, true).map(|p| p.code), Some("NOT_BOOSTING"));
    assert_eq!(bs::member_problem(Some(&on), false, false), None);
    assert_eq!(bs::member_problem(Some(&on), true, true), None);
    assert_eq!(bs::discord_text(Some(30005)), "This server has reached Discord's limit of 250 roles.");
    assert!(bs::reason(&"x".repeat(500)).chars().count() <= 400);
    assert_eq!(bs::reason("the boost ended"), "Server Boosts: the boost ended");
}

#[test]
fn role_bodies_are_the_discord_js_bodies() {
    let look = bs::Look { name: Some("Cool".into()), color: Some(0xff8800), unicode_emoji: Some("💎".into()), icon_attachment: None };
    let body = bs::create_role_body(&look, None);
    assert_eq!(body, json!({ "name": "Cool", "colors": { "primary_color": 0xff8800 }, "hoist": false, "permissions": "0", "mentionable": false, "unicode_emoji": "💎" }));
    let keys: Vec<&String> = body.as_object().map(|o| o.keys().collect()).unwrap_or_default();
    assert_eq!(keys, vec!["name", "colors", "hoist", "permissions", "mentionable", "unicode_emoji"], "discord.js key order");
    let plain = bs::Look { name: Some("N".into()), ..Default::default() };
    assert_eq!(bs::create_role_body(&plain, Some(&[0x89, 0x50])), json!({ "name": "N", "hoist": false, "permissions": "0", "mentionable": false, "icon": "data:image/jpg;base64,iVA=" }));
    assert_eq!(bs::edit_role_body(&bs::Look { color: Some(1), ..Default::default() }, None), json!({ "colors": { "primary_color": 1 } }));
    // Util.setPosition: sorted by raw position (ties: the newer role lower), moved, every index sent.
    use serenity::all::RoleId;
    let r = |n| RoleId::new(n);
    let roles = [(r(1), 0), (r(5), 1), (r(4), 1), (r(3), 2), (r(9), 3)];
    assert_eq!(bs::role_positions_body(&roles, r(5), 3), vec![(r(1), 0), (r(4), 1), (r(3), 2), (r(5), 3), (r(9), 4)]);
}

#[test]
fn icon_attachment_checks() {
    let att = |ct: Option<&str>, size: u64| bs::IconAttachment { url: "https://cdn.example/i.png".into(), content_type: ct.map(String::from), size };
    assert_eq!(bs::icon_attachment_problem(&att(Some("image/png"), 1000)), None);
    assert_eq!(bs::icon_attachment_problem(&att(Some("IMAGE/JPEG; charset=x"), 1000)), None);
    assert_eq!(bs::icon_attachment_problem(&att(Some("image/gif"), 10)).as_deref(), Some("The icon must be a PNG or JPEG image of at most 256 KB."));
    assert_eq!(bs::icon_attachment_problem(&att(None, 10)).as_deref(), Some("The icon must be a PNG or JPEG image of at most 256 KB."));
    assert_eq!(bs::icon_attachment_problem(&att(Some("image/png"), 300_000)).as_deref(), Some("The icon is too big: at most 256 KB."));
    let blank = bs::LookInput { name: Some("  ".into()), ..Default::default() };
    assert!(!bs::asked_anything(&blank));
    assert!(bs::asked_anything(&bs::LookInput { color: Some("#fff".into()), ..Default::default() }));
}

#[tokio::test]
async fn the_edit_cooldown_is_one_minute_per_member_and_server() {
    let t = TestApp::new();
    bs::reset_cooldowns(&t);
    assert_eq!(bs::cooldown_left_ms(&t, "G", "U"), 0);
    bs::note_edit(&t, "G", "U");
    t.clock.advance(10_000);
    assert_eq!(bs::cooldown_left_ms(&t, "G", "U"), bs::consts::EDIT_COOLDOWN_MS - 10_000);
    assert_eq!(bs::cooldown_left_ms(&t, "G", "OTHER"), 0);
    assert_eq!(bs::cooldown_left_ms(&t, "OTHER", "U"), 0);
    t.clock.advance(50_000);
    assert_eq!(bs::cooldown_left_ms(&t, "G", "U"), 0);
    for i in 0..bs::consts::COOLDOWN_MAX + 10 {
        bs::note_edit(&t, "G", &format!("U{i}"));
    }
    assert!(bs::cooldown_entries(&t) <= bs::consts::COOLDOWN_MAX, "bounded");
    bs::reset_cooldowns(&t);
    assert_eq!(bs::cooldown_entries(&t), 0);
}

// ── Auto Threads: pure rules ──────────────────────────────────────────────────

#[test]
fn entries_and_publish_ids_match_the_js() {
    for (input, out) in sweep("at.normalizeEntry") {
        assert_eq!(json!(at::normalize_entry(&input)), out, "{input}");
    }
    for (input, out) in sweep("at.readEntries") {
        assert_eq!(json!(at::read_entries(&input)), out, "{input}");
    }
    for (input, out) in sweep("at.readPublishIds") {
        let ids: Vec<String> = input.as_array().into_iter().flatten().filter_map(s).collect();
        assert_eq!(json!(at::read_publish_ids(&ids)), out, "{input}");
    }
}

#[test]
fn plain_text_and_content_title_match_the_js() {
    for (input, out) in sweep("at.plainText") {
        assert_eq!(Value::from(at::plain_text(input.as_str().unwrap_or(""))), out, "{input}");
    }
    for (input, out) in sweep("at.contentTitle") {
        assert_eq!(Value::from(at::content_title(input.as_str().unwrap_or(""))), out, "{input}");
    }
}

#[test]
fn auto_thread_names_match_the_js() {
    let created = bot_util::js::date_parse("2026-10-21T23:59:00Z");
    for (input, out) in sweep("at.threadName") {
        let tpl = input[0].as_str().unwrap_or("");
        let over = &input[1];
        let mut src = at::NameSource {
            member: Some(PlaceholderMember { display_name: Some("Alice".into()), ..Default::default() }),
            user: Some(PlaceholderUser { id: "U1".into(), username: Some("alice".into()), ..Default::default() }),
            guild: Some(PlaceholderGuild { id: "G".into(), name: Some("My Server".into()), ..Default::default() }),
            channel: Some(PlaceholderChannel { id: "C1".into(), name: Some("help-desk".into()), kind: Some(0) }),
            content: s(&over["content"]).unwrap_or_default(),
            created_at_ms: Some(created),
            now_ms: created,
        };
        if over.get("bare").is_some() {
            src = at::NameSource { content: src.content, now_ms: created, ..Default::default() };
        }
        if let Some(m) = over.get("member") {
            src.member = m.get("displayName").and_then(Value::as_str).map(|d| PlaceholderMember { display_name: Some(d.into()), ..Default::default() });
        }
        if let Some(u) = over.get("user") {
            src.user = Some(PlaceholderUser { id: "U".into(), username: s(&u["username"]), global_name: s(&u["globalName"]), ..Default::default() });
        }
        if over.get("createdAt").is_some() {
            // No creation time: the date of "now" (the JS ran at its own now; use that day).
            src.created_at_ms = None;
            src.now_ms = bot_util::js::date_parse(&format!("{}T12:00:00Z", out.as_str().unwrap_or("")));
        }
        assert_eq!(Value::from(at::thread_name(tpl, &src)), out, "{input}");
    }
    for (input, out) in sweep("at.unknownTokens") {
        assert_eq!(json!(at::unknown_tokens(input.as_str().unwrap_or(""))), out, "{input}");
    }
    for (input, out) in sweep("at.threadNeeds") {
        assert_eq!(json!(at::thread_needs(input.as_bool().unwrap_or(false))), out);
    }
}

#[test]
fn is_publishable_matches_the_js() {
    use serenity::all::{MessageFlags, MessageType};
    // MessageType numbers of the sweep: Default 0, Reply 19, ChatInputCommand 20, ContextMenuCommand 23, pins 6, join 7.
    for (input, out) in sweep("at.isPublishable") {
        let kind = MessageType::from(input["type"].as_u64().unwrap_or(0) as u8);
        let info = at::PublishInfo {
            kind,
            has_poll: input.get("poll").is_some(),
            forward: input.get("forward").is_some(),
            flags: Some(MessageFlags::from_bits_truncate(input.get("flags").and_then(Value::as_u64).unwrap_or(0))),
        };
        assert_eq!(Value::from(at::is_publishable(&info)), out, "{input}");
    }
}

#[test]
fn the_publish_limiter_matches_the_js() {
    let lim = at::PublishLimiter::new(at::consts::PUBLISH_MAX, at::consts::PUBLISH_WINDOW_MS, at::consts::PUBLISH_MAX_CHANNELS);
    let mut t = 1_000_000i64;
    for (input, out) in sweep("at.limiter") {
        t += input[2].as_i64().unwrap_or(0);
        let ch = input[1].as_str().unwrap_or("");
        let got = match input[0].as_str() {
            Some("take") => Value::from(lim.take(ch, t)),
            Some("remaining") => Value::from(lim.remaining(ch, t)),
            _ => {
                lim.release(ch);
                Value::Null
            }
        };
        assert_eq!(got, out, "{input}");
    }
    let small = at::PublishLimiter::new(10, 60_000, 5);
    for i in 0..50 {
        small.take(&format!("C{i}"), 1);
    }
    assert!(small.size() <= 5, "bounded: {}", small.size());
    small.clear();
    assert_eq!(small.size(), 0);
}

#[test]
fn autothread_reply_texts() {
    use feat_community_b::commands::autothread::{notes, shown};
    assert_eq!(shown("  a \n b "), "a b");
    assert_eq!(shown("`tick`\\"), "tick");
    assert_eq!(shown(""), "(empty)");
    assert_eq!(shown(&"x".repeat(80)).chars().count(), 60);
    let on = settings_with(|s| s.auto_threads_enabled = true);
    let off = settings_with(|s| s.auto_threads_enabled = false);
    assert_eq!(notes(Some(&on), &[]), "");
    assert_eq!(notes(Some(&on), &["p".into()]), "\n\n⚠️ p");
    assert_eq!(notes(Some(&off), &[]), format!("\n\n{}", feat_community_b::commands::autothread::OFF_NOTE));
    assert_eq!(at::archive_label("4320"), "3 days");
}

// ── Against a cached (offline) guild ──────────────────────────────────────────

const G: &str = "930000000000000001";
const OWNER: &str = "600000000000000099";
const ADMIN: &str = "600000000000000001";
const MEMBER: &str = "600000000000000002";
const STAFF_CH: &str = "810000000000000001";
const ANN_CH: &str = "810000000000000002";
const THREAD: &str = "810000000000000003";
const LOCKED_CH: &str = "810000000000000004";

fn base_guild(bot_perms: P, bot_pos: u16) -> FakeGuild {
    FakeGuild::new(G, OWNER)
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role_with("820000000000000001", 1, P::empty(), |r| r["name"] = json!("Moderator"))
        .role_with("820000000000000002", 2, P::empty(), |r| r["name"] = json!("Helper"))
        .role_with("820000000000000003", 3, P::empty(), |r| r["name"] = json!("Regular"))
        .role_with("820000000000000004", 4, P::empty(), |r| r["name"] = json!("Anchor"))
        .role_with("820000000000000009", bot_pos, bot_perms, |r| r["name"] = json!("Bot"))
        .role("820000000000000010", 10, P::MANAGE_GUILD)
        .member(TEST_BOT_ID, &["820000000000000009"])
        .member(ADMIN, &["820000000000000010"])
        .member(MEMBER, &[])
        .channel(STAFF_CH, 0, "staff", &[])
        .channel(ANN_CH, 5, "news", &[])
        .channel(LOCKED_CH, 0, "locked", &[(G, false, P::empty(), P::CREATE_PRIVATE_THREADS | P::CREATE_PUBLIC_THREADS)])
        .thread(THREAD, 12, STAFF_CH)
}

fn app_with(guild: FakeGuild) -> TestApp {
    TestApp::new().with_guilds([G]).with_fake_guilds(&[guild])
}

fn gid() -> serenity::all::GuildId {
    serenity::all::GuildId::new(G.parse().unwrap_or(1))
}

fn cid(c: &str) -> serenity::all::ChannelId {
    serenity::all::ChannelId::new(c.parse().unwrap_or(1))
}

fn cached_guild(t: &TestApp) -> serenity::all::Guild {
    bot_core::interaction_helpers::cached_guild(t, gid()).expect("cached")
}

fn bot() -> serenity::all::UserId {
    serenity::all::UserId::new(TEST_BOT_ID.parse().unwrap_or(1))
}

#[tokio::test]
async fn placement_for_needs_manage_roles_room_and_a_low_anchor() {
    use bs::PlacementKind::*;
    let t = app_with(base_guild(P::MANAGE_ROLES, 6));
    let g = cached_guild(&t);
    let anchor = |id: Option<&str>| settings_with(|s| s.boost_role_anchor_id = id.map(String::from));
    let p = bs::placement_for(&g, bot(), Some(&anchor(None))).expect("ok");
    assert_eq!((p.kind, p.base, p.target), (Bottom, None, 1), "no anchor, no Nitro Booster role");
    let p = bs::placement_for(&g, bot(), Some(&anchor(Some("820000000000000004")))).expect("ok");
    assert_eq!((p.kind, p.base.map(|r| r.to_string()), p.target), (Anchor, Some("820000000000000004".into()), 3), "just below the anchor");
    assert_eq!(bs::placement_for(&g, bot(), Some(&anchor(Some("820000000000000077")))).map(|p| p.kind), Ok(Bottom), "a missing anchor falls back");
    assert_eq!(bs::placement_for(&g, bot(), Some(&anchor(Some(G)))).map(|p| p.kind), Ok(Bottom), "@everyone is not an anchor");
    assert_eq!(bs::placement_for(&g, bot(), Some(&anchor(Some("820000000000000010")))), Err("ANCHOR_HIGH"));

    let none = cached_guild(&app_with(base_guild(P::empty(), 6)));
    assert_eq!(bs::placement_for(&none, bot(), None), Err("BOT_PERMS"));
    let low = cached_guild(&app_with(FakeGuild::new(G, OWNER).role("820000000000000009", 1, P::MANAGE_ROLES).member(TEST_BOT_ID, &["820000000000000009"])));
    assert_eq!(bs::placement_for(&low, bot(), None), Err("BOT_LOW"));

    // With no anchor a booster role goes just ABOVE the Nitro Booster role, never at or above the bot.
    let nitro = base_guild(P::MANAGE_ROLES, 6).role_with("820000000000000005", 2, P::empty(), |r| {
        r["name"] = json!("Nitro Booster");
        r["tags"] = json!({ "premium_subscriber": null });
    });
    let ng = cached_guild(&app_with(nitro));
    let p = bs::placement_for(&ng, bot(), None).expect("ok");
    assert_eq!(p.kind, Nitro);
    assert_eq!(p.target, bot_core::permissions::role_position(&ng, serenity::all::RoleId::new(820000000000000005)).unwrap_or(0));
    assert!(bs::role_editable(&g, bot(), serenity::all::RoleId::new(820000000000000004)));
    assert!(!bs::role_editable(&g, bot(), serenity::all::RoleId::new(820000000000000010)), "above the bot");
}

#[tokio::test]
async fn prepare_look_refusals() {
    let t = app_with(base_guild(P::MANAGE_ROLES, 6));
    let g = cached_guild(&t);
    let look = |name: Option<&str>, color: Option<&str>, emoji: Option<&str>| bs::LookInput {
        name: name.map(String::from),
        color: color.map(String::from),
        icon_emoji: emoji.map(String::from),
        icon_image: None,
    };
    let code = |r: Result<bs::Look, bs::Problem>| r.err().map(|p| p.code);
    assert_eq!(code(bs::prepare_look(&g, &look(None, None, None), None, true)), Some("BAD_NAME"));
    assert_eq!(code(bs::prepare_look(&g, &look(Some("moderator"), None, None), None, true)), Some("BAD_NAME"), "taken");
    assert_eq!(code(bs::prepare_look(&g, &look(Some("ok"), Some("red"), None), None, true)), Some("BAD_COLOR"));
    assert_eq!(code(bs::prepare_look(&g, &look(Some("ok"), None, Some("💎")), None, true)), Some("NO_ROLE_ICONS"), "icons need Boost level 2");
    let both = bs::LookInput {
        icon_image: Some(bs::IconAttachment { url: "u".into(), content_type: Some("image/png".into()), size: 1 }),
        ..look(Some("ok"), None, Some("💎"))
    };
    assert_eq!(code(bs::prepare_look(&g, &both, None, true)), Some("ICON_BOTH"));
    let ok = bs::prepare_look(&g, &look(Some("  Cool   One "), Some("#FF8800"), None), None, true).expect("ok");
    assert_eq!((ok.name.as_deref(), ok.color), (Some("Cool One"), Some(0xff8800)));
    assert!(bs::prepare_look(&g, &look(Some("Moderator"), None, None), Some("820000000000000001"), false).is_ok(), "own role excepted");
}

#[tokio::test]
async fn channel_problems_errors_and_warnings() {
    let all = P::VIEW_CHANNEL | P::SEND_MESSAGES_IN_THREADS | P::EMBED_LINKS | P::CREATE_PRIVATE_THREADS | P::CREATE_PUBLIC_THREADS | P::MANAGE_THREADS;
    let t = app_with(base_guild(all, 6));
    let p = mmp::channel_problems(&t, gid(), cid(STAFF_CH), Some(&["820000000000000002".into()]));
    assert!(p.errors.is_empty() && p.warnings.is_empty() && p.can_private && p.can_public, "{p:?}");
    let p = mmp::channel_problems(&t, gid(), cid(STAFF_CH), Some(&[]));
    assert_eq!(p.warnings, vec!["No role is pinged, so only members who can manage threads will see new private threads. Set one with `ping_role`.".to_string()]);
    let p = mmp::channel_problems(&t, gid(), cid(LOCKED_CH), None);
    assert_eq!(p.errors, vec![format!("I need **Create Private Threads** in <#{LOCKED_CH}> to open conversations.")]);
    let weak = app_with(base_guild(P::VIEW_CHANNEL | P::CREATE_PUBLIC_THREADS, 6));
    let p = mmp::channel_problems(&weak, gid(), cid(STAFF_CH), None);
    assert_eq!(p.errors, vec![format!("I need **Send Messages in Threads, Embed Links** in <#{STAFF_CH}>.")]);
    assert_eq!(
        p.warnings,
        vec![
            "I lack **Create Private Threads**, so conversations would open as **public** threads. Give me that permission.".to_string(),
            "I lack **Manage Threads**, so I can archive closed conversations but not lock them.".to_string(),
        ]
    );
}

#[tokio::test]
async fn auto_thread_filters_answer_from_the_memo() {
    use feat_community_b::plugins::auto_threads::{may_be_listed, may_handle, may_publish};
    let t = app_with(base_guild(P::all(), 6));
    let ch = |c: &str| bot_core::interaction_helpers::cached_channel(&t, gid(), cid(c));
    // Settings unknown: everything passes and the handler decides.
    assert!(may_handle(&t, G, ch(STAFF_CH).as_ref()));
    assert!(may_publish(&t, G, false, None, ch(ANN_CH).as_ref()));
    assert!(may_be_listed(&t, G, "123"));
    assert!(!may_handle(&t, G, ch(THREAD).as_ref()), "never in a thread");
    assert!(!may_publish(&t, G, false, None, ch(STAFF_CH).as_ref()), "announcement channels only");
    assert!(!may_publish(&t, G, true, None, ch(ANN_CH).as_ref()), "an edit is never published again");
    assert!(may_publish(&t, G, true, Some(true), ch(ANN_CH).as_ref()), "the edit that answers a deferred reply");
    let patch = SettingsPatch::new()
        .set("autoThreadsEnabled", true)
        .set("autoThreadChannels", json!([{ "channelId": STAFF_CH }]))
        .set("autoPublishChannelIds", json!([ANN_CH]));
    t.settings().update_settings(G, patch).await.expect("memory store");
    assert!(may_handle(&t, G, ch(STAFF_CH).as_ref()));
    assert!(may_handle(&t, G, ch(ANN_CH).as_ref()), "a publish channel");
    assert!(!may_handle(&t, G, ch(LOCKED_CH).as_ref()), "not listed");
    assert!(may_publish(&t, G, false, None, ch(ANN_CH).as_ref()));
    assert!(may_be_listed(&t, G, STAFF_CH) && may_be_listed(&t, G, ANN_CH) && !may_be_listed(&t, G, LOCKED_CH));
    t.settings().update_settings(G, SettingsPatch::new().set("autoThreadsEnabled", false)).await.expect("memory store");
    assert!(!may_handle(&t, G, ch(STAFF_CH).as_ref()), "module off");
    assert!(!may_publish(&t, G, false, None, ch(ANN_CH).as_ref()));
    assert!(may_be_listed(&t, G, STAFF_CH), "cleanup runs while the module is off");
    assert_eq!(at::listed_channel(None, STAFF_CH), None);
}

#[tokio::test]
async fn modmail_and_boost_filters() {
    let t = app_with(base_guild(P::all(), 6));
    let thread = bot_core::interaction_helpers::cached_channel(&t, gid(), cid(THREAD));
    let staff = bot_core::interaction_helpers::cached_channel(&t, gid(), cid(STAFF_CH));
    // The fake thread is owned by the guild owner, not the bot: modmail threads are always the bot's.
    assert!(!mmp::staff_message_filter(&t, thread.as_ref(), "hello"));
    let mut own = thread.clone().expect("thread");
    own.owner_id = Some(bot());
    assert!(mmp::staff_message_filter(&t, Some(&own), "hello"));
    assert!(!mmp::staff_message_filter(&t, Some(&own), "  // a note"), "notes stay");
    assert!(!mmp::staff_message_filter(&t, staff.as_ref(), "hello"), "not a thread");
    assert!(!mmp::known_not_modmail(&t, THREAD));
    assert!(mmp::channel_delete_filter(&t, G, STAFF_CH), "no memo: passes");
    assert!(bs::channel_delete_filter(&t, G, STAFF_CH));
    t.settings().update_settings(G, SettingsPatch::new().set("modmailChannelId", STAFF_CH).set("boostChannelId", ANN_CH)).await.expect("memory");
    assert!(mmp::channel_delete_filter(&t, G, STAFF_CH) && !mmp::channel_delete_filter(&t, G, ANN_CH));
    assert!(bs::channel_delete_filter(&t, G, ANN_CH) && !bs::channel_delete_filter(&t, G, STAFF_CH));
    assert!(bs::member_remove_filter(false) && !bs::member_remove_filter(true));
}

fn fake_member(id: &str, boosting: bool, is_bot: bool) -> serenity::all::Member {
    let since = if boosting { json!("2026-01-01T00:00:00.000000+00:00") } else { Value::Null };
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": is_bot },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0,
        "premium_since": since, "guild_id": G,
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

#[test]
fn member_update_filter_only_a_boost_change_of_a_cached_person() {
    let on = fake_member(MEMBER, true, false);
    let off = fake_member(MEMBER, false, false);
    assert!(bs::is_boosting(Some(&on)) && !bs::is_boosting(Some(&off)));
    assert!(bs::member_update_filter(true, Some(&off), &on), "start");
    assert!(bs::member_update_filter(true, Some(&on), &off), "end");
    assert!(!bs::member_update_filter(true, Some(&on), &on), "no change");
    assert!(!bs::member_update_filter(false, Some(&off), &on), "a partial old member never counts");
    assert!(!bs::member_update_filter(true, None, &on));
    assert!(!bs::member_update_filter(true, Some(&fake_member(MEMBER, false, true)), &fake_member(MEMBER, true, true)), "bots");
}

#[tokio::test]
async fn modmail_components_refuse_the_wrong_place() {
    let t = app_with(base_guild(P::all(), 6));
    t.install(registry());
    // A DM prompt's button clicked inside a server.
    bot_core::router::route_interaction(&t, FakeInteraction::button("modmail:open:930000000000000001:700000000000000001").guild(G).build()).await;
    let c = &t.responses.calls()[0];
    assert_eq!((c.embed_title(), c.ephemeral()), (Some("❌  Button Expired"), true));
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::select("modmail-select:open:7001", &["930000000000000001"]).guild(G).build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Menu Expired"));
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::select("modmail-select:open", &["1"]).build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Menu Expired"), "no message id");
    // Cancel in a DM: the prompt becomes "Cancelled" and loses its buttons.
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::button("modmail:cancel").build()).await;
    let c = &t.responses.calls()[0];
    assert_eq!(c.method, "update");
    assert_eq!(c.embed_title(), Some("ℹ️  Cancelled"));
    assert_eq!(c.embed_description(), Some("Nothing was sent."));
    // The close button outside a cached server.
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::button("modmail:close:row1").build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Server Only"));
    // ...and by a member who is not a moderator.
    t.responses.clear();
    bot_core::router::route_interaction(&t, FakeInteraction::button("modmail:close:row1").guild(G).user(MEMBER).channel(THREAD).build()).await;
    let c = &t.responses.calls()[0];
    assert_eq!((c.embed_title(), c.embed_description()), (Some("❌  Insufficient Permissions"), Some("You need the Moderator level to close a conversation.")));
}

#[tokio::test]
async fn the_autothread_close_button_refusals() {
    let t = app_with(base_guild(P::VIEW_CHANNEL | P::SEND_MESSAGES, 6));
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("autoThreadsEnabled", true)).await.expect("memory");
    let click = |id: &str, user: &str, ch: &str| FakeInteraction::button(id).guild(G).user(user).channel(ch).build();
    bot_core::router::route_interaction(&t, click("autothread:close:600000000000000005", MEMBER, STAFF_CH)).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Button Expired"), "outside a thread");
    t.responses.clear();
    bot_core::router::route_interaction(&t, click("autothread:close:600000000000000005", MEMBER, THREAD)).await;
    let c = &t.responses.calls()[0];
    assert_eq!((c.embed_title(), c.embed_description()), (Some("❌  Not Allowed"), Some("Only the person who started this thread, or a moderator, can close it.")));
    t.responses.clear();
    let own = format!("autothread:close:{MEMBER}");
    bot_core::router::route_interaction(&t, click(&own, MEMBER, THREAD)).await;
    let c = &t.responses.calls()[0];
    assert_eq!(
        (c.embed_title(), c.embed_description()),
        (Some("❌  Cannot Close"), Some("I need **Manage Threads** here to close this thread.")),
        "the starter, but the bot can neither lock it nor owns it"
    );
    // While the module is off the gated button answers "turned off".
    t.settings().update_settings(G, SettingsPatch::new().set("autoThreadsEnabled", false)).await.expect("memory");
    t.responses.clear();
    bot_core::router::route_interaction(&t, click(&own, MEMBER, THREAD)).await;
    let body = serde_json::to_string(&t.responses.calls()[0].body).unwrap_or_default();
    assert!(body.contains("turned off"), "{body}");
}

#[tokio::test]
async fn the_autothread_close_button_closes_for_the_starter() {
    let t = app_with(base_guild(P::VIEW_CHANNEL | P::SEND_MESSAGES | P::MANAGE_THREADS, 6));
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("autoThreadsEnabled", true)).await.expect("memory");
    let click = FakeInteraction::button(&format!("autothread:close:{MEMBER}")).guild(G).user(MEMBER).channel(THREAD).build();
    bot_core::router::route_interaction(&t, click).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].method, "update", "the message is edited first");
    assert_eq!(calls[0].content(), Some("🔒 Thread closed by member."), "the clicker's display name");
    // Offline the thread edit fails: the starter is told privately.
    assert_eq!(calls.get(1).and_then(|c| c.content()), Some("I could not close this thread. Check my permissions here."));
    assert!(calls[1].ephemeral());
}

// ── Modmail: the catch-up run, the intro, the greeting; the boost thank-you message ──

fn dm(id: u64, author: &str, is_bot: bool, content: &str, reactions: &[(&str, bool)]) -> serenity::all::Message {
    let reactions: Vec<Value> = reactions
        .iter()
        .map(|(e, me)| json!({ "count": 1, "count_details": { "burst": 0, "normal": 1 }, "me": me, "me_burst": false, "emoji": { "id": null, "name": e }, "burst_colors": [] }))
        .collect();
    serde_json::from_value(json!({
        "id": id.to_string(), "channel_id": "100000000000000003",
        "author": { "id": author, "username": format!("user{author}"), "discriminator": "0", "avatar": null, "bot": is_bot },
        "content": content, "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
        "type": 0, "components": [], "reactions": reactions,
    }))
    .unwrap_or_else(|e| panic!("message: {e}"))
}

const U: &str = "600000000000000021";

#[test]
fn only_the_dms_marked_waiting_in_a_row_after_the_prompt_are_sent() {
    let wait = mm::limits::WAIT_EMOJI;
    let ok = mm::limits::OK_EMOJI;
    let uid = serenity::all::UserId::new(U.parse().unwrap_or(1));
    let batch = vec![
        dm(13, U, false, "third", &[(wait, true)]),
        dm(11, U, false, "first", &[(wait, true)]),
        dm(12, U, false, "already sent", &[(wait, true), (ok, true)]),
        dm(10, "600000000000000022", false, "someone else", &[(wait, true)]),
        dm(9, U, true, "a bot", &[(wait, true)]),
        dm(8, U, false, "   ", &[(wait, true)]),
        dm(14, U, false, "refused by the rate limit", &[("❌", true)]),
        dm(15, U, false, "owned by a newer prompt", &[(wait, true)]),
    ];
    let ids: Vec<u64> = mmp::waiting_run(batch, uid).iter().map(|m| m.id.get()).collect();
    assert_eq!(ids, vec![11, 13], "oldest first, the ✅ one skipped, the run ends at the first message without ⏳");
    // Somebody else's ⏳ (not the bot's) does not count.
    assert!(mmp::waiting_run(vec![dm(20, U, false, "x", &[(wait, false)])], uid).is_empty());
    assert!(mmp::has_bot_reaction(&dm(21, U, false, "x", &[(wait, true)]), wait));
    assert!(!mmp::has_bot_reaction(&dm(21, U, false, "x", &[(wait, false)]), wait));
    // At most CATCH_UP_MAX after the first, in order.
    let many: Vec<_> = (0..15).rev().map(|i| dm(100 + i, U, false, &format!("m{i}"), &[(wait, true)])).collect();
    let ids: Vec<u64> = mmp::waiting_run(many, uid).iter().map(|m| m.id.get()).collect();
    assert_eq!(ids, (100..110).collect::<Vec<u64>>());
}

#[test]
fn the_intro_pings_only_the_configured_roles_and_says_why_not() {
    let intro = mmp::Intro {
        user_id: U.into(),
        user_created_ms: Some(1_600_000_000_000.0),
        member_joined_ms: Some(1_700_000_000_000.0),
        allowed: vec!["820000000000000001".into()],
        refused: vec!["820000000000000002".into(), "820000000000000003".into()],
        public_reason: Some("I lack **Create Private Threads**, so this conversation is in a **public** thread.".into()),
        row_id: "clrow9".into(),
    };
    let j = serde_json::to_value(mmp::intro_payload(None, &intro)).unwrap_or(Value::Null);
    assert_eq!(j["content"], "<@&820000000000000001>");
    assert_eq!(j["allowed_mentions"]["roles"], json!(["820000000000000001"]));
    assert_eq!(j["allowed_mentions"]["users"], json!([]));
    assert!(j["allowed_mentions"].get("parse").is_none_or(|p| p == &json!([])), "never parse: {}", j["allowed_mentions"]);
    assert_eq!(j["embeds"][0]["title"], "📬 New conversation");
    let expected = [
        format!("**Member:** <@{U}> (`{U}`)"),
        "**Account created:** <t:1600000000:R>".to_string(),
        "**Joined the server:** <t:1700000000:R>".to_string(),
        "⚠️ I cannot ping <@&820000000000000002>, <@&820000000000000003>: make the role mentionable or give me **Mention Everyone**.".to_string(),
        "⚠️ I lack **Create Private Threads**, so this conversation is in a **public** thread.".to_string(),
        String::new(),
        mm::STAFF_HELP.to_string(),
    ]
    .join("\n");
    assert_eq!(j["embeds"][0]["description"], Value::from(expected));
    assert_eq!(j["components"][0]["components"][0]["custom_id"], "modmail:close:clrow9");
    // Nobody to ping: no content at all.
    let quiet = mmp::Intro { allowed: vec![], refused: vec![], public_reason: None, ..intro };
    let j = serde_json::to_value(mmp::intro_payload(None, &quiet)).unwrap_or(Value::Null);
    assert!(j.get("content").is_none_or(Value::is_null), "{j}");
}

#[test]
fn the_greeting_renders_placeholders_and_falls_back_to_the_default() {
    let ctx = bot_util::placeholder_parser::RenderContext {
        user: Some(PlaceholderUser { id: U.into(), username: Some("sam".into()), ..Default::default() }),
        guild: Some(PlaceholderGuild { id: "G".into(), name: Some("My Server".into()), ..Default::default() }),
        ..Default::default()
    };
    let custom = settings_with(|s| s.modmail_greeting = Some("Hi {user.name}, {server} staff will answer.".into()));
    assert_eq!(mmp::greeting_text(Some(&custom), &ctx), "Hi sam, My Server staff will answer.");
    let blank = settings_with(|s| s.modmail_greeting = Some(String::new()));
    let expected = "Thanks for your message! The staff of **My Server** will answer you here soon.";
    assert_eq!(mmp::greeting_text(Some(&blank), &ctx), expected);
    assert_eq!(mmp::greeting_text(None, &ctx), expected);
}

#[test]
fn the_thank_you_pings_only_the_booster_and_a_preview_nobody() {
    let src = bs::ThankYouSource {
        member: Some(PlaceholderMember { user: Some(PlaceholderUser { id: U.into(), ..Default::default() }), ..Default::default() }),
        guild: Some(PlaceholderGuild { id: "G".into(), name: Some("Test Guild".into()), ..Default::default() }),
        boosts: 4,
        tier: 1,
    };
    let s = settings_with(|s| s.boost_message = Some("@everyone {user} boosted! {boosts}".into()));
    let uid = serenity::all::UserId::new(U.parse().unwrap_or(1));
    let real = serde_json::to_value(bs::thank_you_message(Some(&s), &src, uid, false)).unwrap_or(Value::Null);
    assert_eq!(real["content"], Value::from(format!("@everyone <@{U}> boosted! 4")));
    assert_eq!(real["allowed_mentions"]["users"], json!([U]), "only the booster");
    assert_eq!(real["allowed_mentions"]["roles"], json!([]));
    assert!(real["allowed_mentions"].get("parse").is_none_or(|p| p == &json!([])), "never @everyone: {}", real["allowed_mentions"]);
    let preview = serde_json::to_value(bs::thank_you_message(Some(&s), &src, uid, true)).unwrap_or(Value::Null);
    assert_eq!(preview["content"], Value::from(format!("{}@everyone <@{U}> boosted! 5", bs::PREVIEW_HEAD)));
    assert_eq!(preview["allowed_mentions"]["users"], json!([]), "a preview pings nobody");
    let long = settings_with(|s| s.boost_message = Some("x".repeat(5000)));
    let j = serde_json::to_value(bs::thank_you_message(Some(&long), &src, uid, true)).unwrap_or(Value::Null);
    assert_eq!(j["content"].as_str().map(|c| c.encode_utf16().count()), Some(2000), "the label fits inside 2000");
}

// ── Audit: behaviour checked against discord.js ───────────────────────────────

fn user_json(id: &str, avatar: Option<&str>) -> serenity::all::User {
    serde_json::from_value(json!({ "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": avatar, "global_name": null }))
        .expect("user")
}

#[test]
fn relayed_member_avatars_use_the_discord_js_default_url() {
    // memberMessageEmbed: `user.displayAvatarURL()` with no options is webp (gif when animated), without a size.
    let plain = mmp::RelayAuthor::of(&user_json("600000000000000005", Some("0123456789abcdef0123456789abcdef")));
    assert_eq!(plain.avatar_url.as_deref(), Some("https://cdn.discordapp.com/avatars/600000000000000005/0123456789abcdef0123456789abcdef.webp"));
    let animated = mmp::RelayAuthor::of(&user_json("600000000000000005", Some("a_0123456789abcdef0123456789abcdef")));
    assert_eq!(animated.avatar_url.as_deref(), Some("https://cdn.discordapp.com/avatars/600000000000000005/a_0123456789abcdef0123456789abcdef.gif"));
    // No avatar: the default avatar, always png.
    let none = mmp::RelayAuthor::of(&user_json("600000000000000005", None));
    assert!(none.avatar_url.as_deref().is_some_and(|u| u.starts_with("https://cdn.discordapp.com/embed/avatars/") && u.ends_with(".png")), "{none:?}");
}

#[tokio::test]
async fn thread_names_name_mentions_like_clean_content() {
    // createThread names the thread from `message.cleanContent`: a mentioned member is `@nickname`, also one the cache
    // did not know (discord.js caches the mentioned members); plainText then drops the "@".
    const NICKED: &str = "600000000000000006";
    const REMOTE: &str = "600000000000000007";
    let rest = bot_core::testing::FakeRest::new();
    let guild = base_guild(P::all(), 6).member_with(NICKED, &[], |m| m["nick"] = json!("Nicky"));
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[guild], &rest);
    t.install(registry());
    t.settings()
        .update_settings(G, SettingsPatch::new().set("autoThreadsEnabled", true).set("autoThreadChannels", json!([{ "channelId": STAFF_CH, "name": "{content}" }])))
        .await
        .expect("memory store");
    rest.on("POST", "/channels/*/messages/*/threads", 201, json!({
        "id": "819000000000000001", "type": 11, "guild_id": G, "name": "x", "parent_id": STAFF_CH, "position": 0, "permission_overwrites": [], "nsfw": false,
        "thread_metadata": { "archived": false, "auto_archive_duration": 1440, "archive_timestamp": "2025-01-01T00:00:00.000000+00:00", "locked": false }
    }));
    let mut message = bot_core::testing::fake_message("700000000000000009", G, STAFF_CH, MEMBER, &format!("<@{NICKED}> and <@!{REMOTE}> hi"));
    message.mentions = vec![
        user_json(NICKED, None),
        serde_json::from_value(json!({ "id": REMOTE, "username": "remoteuser", "discriminator": "0", "avatar": null, "member": { "nick": "Remote", "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 } }))
            .expect("mention"),
    ];
    let channel = bot_core::interaction_helpers::cached_channel(&t, gid(), cid(STAFF_CH));
    t.run_hooks(MessageCreate { guild_id: gid(), message, member: None, channel_id: cid(STAFF_CH), channel }).await;
    let calls = rest.calls_to("POST", "/channels/*/messages/*/threads");
    assert_eq!(calls.len(), 1, "{:?}", rest.calls());
    assert_eq!(calls[0].body["name"], json!("Nicky and Remote hi"));
    assert_eq!(calls[0].reason.as_deref(), Some("Auto Threads: new message"));
}

#[tokio::test]
async fn resolve_thread_is_guild_channels_fetch() {
    // `guild.channels.fetch(id)`: a channel of another guild is an error (GuildChannelUnowned), not this guild's thread.
    const OTHER: &str = "930000000000000077";
    const FOREIGN: &str = "819000000000000002";
    const FRESH: &str = "819000000000000003";
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[base_guild(P::all(), 6)], &rest);
    let foreign = FakeGuild::new(OTHER, OWNER).thread(FOREIGN, 11, "819000000000000099").json()["threads"][0].clone();
    rest.on("GET", &format!("/channels/{FOREIGN}"), 200, foreign);
    assert!(matches!(mmp::resolve_thread(&t, gid(), FOREIGN).await, mmp::ThreadLookup::Error));
    // A thread of this guild is found over REST once, then from the cache.
    let fresh = FakeGuild::new(G, OWNER).thread(FRESH, 12, STAFF_CH).json()["threads"][0].clone();
    rest.on("GET", &format!("/channels/{FRESH}"), 200, fresh);
    assert!(matches!(mmp::resolve_thread(&t, gid(), FRESH).await, mmp::ThreadLookup::Ok(_)));
    assert!(matches!(mmp::resolve_thread(&t, gid(), FRESH).await, mmp::ThreadLookup::Ok(_)));
    assert_eq!(rest.calls_to("GET", &format!("/channels/{FRESH}")).len(), 1, "cached after the first fetch");
    // Unknown Channel is "gone".
    rest.fail("GET", "/channels/819000000000000004", 404, 10003, "Unknown Channel");
    assert!(matches!(mmp::resolve_thread(&t, gid(), "819000000000000004").await, mmp::ThreadLookup::Gone));
}

#[tokio::test]
async fn the_autothread_close_button_in_an_uncached_archived_thread_says_already_closed() {
    // discord.js (Partials.Channel) builds `interaction.channel` from the interaction when the cache has no such thread,
    // so an archived thread answers "Already Closed", not "Button Expired".
    const ARCHIVED: &str = "819000000000000031";
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[base_guild(P::VIEW_CHANNEL | P::MANAGE_THREADS, 6)], &rest);
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("autoThreadsEnabled", true)).await.expect("memory");
    let mut thread = FakeGuild::new(G, OWNER).thread(ARCHIVED, 11, STAFF_CH).json()["threads"][0].clone();
    thread["thread_metadata"]["archived"] = json!(true);
    rest.on("GET", &format!("/channels/{ARCHIVED}"), 200, thread);
    let mut click = FakeInteraction::button(&format!("autothread:close:{MEMBER}")).guild(G).user(MEMBER).channel(ARCHIVED);
    click.json_mut()["channel"] = json!({ "id": ARCHIVED, "type": 11, "name": "t", "parent_id": STAFF_CH, "thread_metadata": { "archived": true, "auto_archive_duration": 1440, "archive_timestamp": "2025-01-01T00:00:00.000000+00:00", "locked": false } });
    bot_core::router::route_interaction(&t, click.build()).await;
    let c = &t.responses.calls()[0];
    assert_eq!((c.embed_title(), c.embed_description()), (Some("❌  Already Closed"), Some("This thread is already closed.")));
}

#[tokio::test]
async fn create_thread_names_the_thread_archives_per_the_entry_and_posts_a_close_button() {
    // autoThreadService.createThread: POST /channels/{c}/messages/{m}/threads with the rendered name and the entry's
    // archive time, then the close button in the background, never pinging.
    const MSG: &str = "819000000000000041";
    const NEW: &str = "819000000000000042";
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[base_guild(P::all(), 6)], &rest);
    let thread = FakeGuild::new(G, OWNER).thread(NEW, 11, STAFF_CH).json()["threads"][0].clone();
    rest.on("POST", &format!("/channels/{STAFF_CH}/messages/{MSG}/threads"), 201, thread);
    let entry = at::Entry { channel_id: STAFF_CH.into(), name: "{user}: {content}".into(), archive_minutes: "4320".into(), close_button: true };
    let src = at::ThreadSource {
        guild_id: gid(),
        channel_id: cid(STAFF_CH),
        message_id: serenity::all::MessageId::new(MSG.parse().unwrap_or(1)),
        author_id: MEMBER.into(),
        name: at::NameSource {
            user: Some(PlaceholderUser { username: Some("member".into()), ..Default::default() }),
            content: "**Hello** there".into(),
            now_ms: 1_767_225_600_000.0,
            ..Default::default()
        },
        in_thread: false,
    };
    assert_eq!(at::create_thread(&t, &src, &entry).await, at::Outcome::Created);
    at::settled(&t).await;
    let creates = rest.calls_to("POST", &format!("/channels/{STAFF_CH}/messages/{MSG}/threads"));
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].body, json!({ "name": "member: Hello there", "auto_archive_duration": 4320 }));
    assert_eq!(creates[0].reason.as_deref(), Some("Auto Threads: new message"));
    let posts = rest.calls_to("POST", &format!("/channels/{NEW}/messages"));
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["content"], json!(at::consts::CLOSE_TEXT));
    assert_eq!(posts[0].body["components"][0]["components"][0]["custom_id"], json!(format!("autothread:close:{MEMBER}")));
    assert_eq!(posts[0].body["allowed_mentions"]["parse"], json!([]));
}

#[tokio::test]
async fn publish_crossposts_and_a_refused_message_type_gives_its_slot_back() {
    // publishMessage: POST /channels/{c}/messages/{m}/crosspost; 50068 (invalid message type) releases the limiter slot.
    let rest = bot_core::testing::FakeRest::new();
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[base_guild(P::all(), 6)], &rest);
    let info = at::PublishInfo { kind: serenity::all::MessageType::Regular, has_poll: false, forward: false, flags: None };
    let mid = |n: u64| serenity::all::MessageId::new(819_000_000_000_000_050 + n);
    rest.on("POST", &format!("/channels/{ANN_CH}/messages/{}/crosspost", mid(1)), 200, bot_core::testing::message_json(&mid(1).to_string()));
    assert_eq!(at::publish_message(&t, gid(), cid(ANN_CH), mid(1), &info).await, at::Outcome::Published);
    let state = t.ext::<at::AutoThreadState>();
    let now = t.clock().now_ms();
    assert_eq!(state.limiter.remaining(ANN_CH, now), at::consts::PUBLISH_MAX - 1);
    rest.fail("POST", &format!("/channels/{ANN_CH}/messages/{}/crosspost", mid(2)), 400, 50068, "Invalid message type");
    assert_eq!(at::publish_message(&t, gid(), cid(ANN_CH), mid(2), &info).await, at::Outcome::Failed);
    assert_eq!(state.limiter.remaining(ANN_CH, now), at::consts::PUBLISH_MAX - 1, "the slot was given back");
    rest.fail("POST", &format!("/channels/{ANN_CH}/messages/{}/crosspost", mid(3)), 403, 50013, "Missing Permissions");
    assert_eq!(at::publish_message(&t, gid(), cid(ANN_CH), mid(3), &info).await, at::Outcome::Perms);
    assert_eq!(state.limiter.remaining(ANN_CH, now), at::consts::PUBLISH_MAX - 2, "a refused attempt keeps its slot");
}
