//! Port of `test/captcha.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: the pure helpers against `golden/feat-captcha/golden.json` (produced from the JS), the payloads, the forms,
//! the image, the plugin and command declarations and the hook filters. With `TEST_DATABASE_URL`: the join gate, Verify
//! (both modes), the answer form, the timeout job, the commands, the raw SQL, and both cross-language directions.

use std::sync::{Arc, Mutex};

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, load_golden, redis_url, run_node};
use chrono::{DateTime, Utc};
use feat_captcha::captcha_service::{self as svc, Challenge, KickOutcome, Reply, Verdict};
use serde_json::{Value, json};
use serenity::all::Permissions as P;

fn msg_json(id: &str) -> Value {
    serde_json::to_value(bot_core::testing::fake_message(id, "955029000000000001", "955029000000000200", TEST_BOT_ID, "")).unwrap_or(Value::Null)
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_captcha::register(&mut reg);
    reg
}

fn golden() -> Value {
    load_golden("feat-captcha/golden.json")
}

fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

/// The fields of an embed the JS and Rust both set (the timestamp is dropped).
fn embed_core(e: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["title", "description", "color", "fields"] {
        if let Some(v) = e.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(t) = e.get("footer").and_then(|f| f.get("text")) {
        out.insert("footer".into(), t.clone());
    }
    if let Some(u) = e.get("image").and_then(|f| f.get("url")) {
        out.insert("image".into(), u.clone());
    }
    Value::Object(out)
}

/// The buttons of rows, as (custom_id or url, label, style, emoji name).
fn buttons(rows: &Value) -> Vec<(String, String, u64, String)> {
    rows.as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["components"].as_array().cloned().unwrap_or_default())
        .map(|b| {
            (
                b["custom_id"].as_str().or(b["url"].as_str()).unwrap_or("").to_string(),
                b["label"].as_str().unwrap_or("").to_string(),
                b["style"].as_u64().unwrap_or(0),
                b["emoji"]["name"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn reply_json(r: &Reply) -> Value {
    let embeds: Vec<Value> = r.embeds_json().iter().map(embed_core).collect();
    let comps: Vec<Value> = r.components.iter().flatten().map(|c| serde_json::to_value(c).unwrap_or(Value::Null)).collect();
    json!({ "content": r.content, "embeds": embeds, "buttons": buttons(&Value::Array(comps)) })
}

fn js_reply(v: &Value) -> Value {
    let embeds: Vec<Value> = v["embeds"].as_array().into_iter().flatten().map(embed_core).collect();
    json!({ "content": v["content"], "embeds": embeds, "buttons": buttons(&v["components"]) })
}

// ── Pure helpers (ported from test/captcha.test.js, checked against the JS) ───

#[test]
fn generate_code_alphabet_and_lengths() {
    assert_eq!(svc::generate_code_with(|_| 0), "AAAAAA");
    for _ in 0..1000 {
        let c = svc::generate_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| svc::CAPTCHA_ALPHABET.contains(ch)), "{c}");
    }
    assert_eq!(svc::CAPTCHA_ALPHABET.len(), 20);
    let set: std::collections::HashSet<char> = svc::CAPTCHA_ALPHABET.chars().collect();
    assert_eq!(set.len(), 20);
    for bad in "0OQD1IL2Z5S8BGUV".chars() {
        assert!(!svc::CAPTCHA_ALPHABET.contains(bad), "{bad}");
    }
    assert_eq!(svc::CODE_LENGTH, 6);
    assert_eq!(svc::MAX_ATTEMPTS, 3);
}

#[test]
fn normalize_hash_and_match_agree_with_the_js() {
    let g = golden();
    for case in g["normalize"].as_array().into_iter().flatten() {
        assert_eq!(svc::normalize_answer(s(&case[0])), case[1].as_str().unwrap_or_default(), "{case}");
    }
    for case in g["hash"].as_array().into_iter().flatten() {
        let (gid, uid, a) = (s(&case[0]).unwrap_or_default(), s(&case[1]).unwrap_or_default(), s(&case[2]).unwrap_or_default());
        assert_eq!(svc::hash_answer(gid, uid, a), case[3].as_str().unwrap_or_default());
    }
    for case in g["matches"].as_array().into_iter().flatten() {
        assert_eq!(svc::answer_matches(s(&case[0]), "G", "U", s(&case[1])), case[2].as_bool().unwrap_or(true), "{case}");
    }
    let h = svc::hash_answer("G", "U", "7KX4PM");
    assert_ne!(h, svc::hash_answer("G", "V", "7KX4PM"));
    assert_ne!(h, svc::hash_answer("H", "U", "7KX4PM"));
}

fn challenge_of(row: &Value) -> Option<Challenge> {
    if row.is_null() {
        return None;
    }
    Some(Challenge {
        answer_hash: row["answerHash"].as_str().map(str::to_string),
        attempts: row["attempts"].as_i64().unwrap_or(0) as i32,
        expires_at: row["expiresAt"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc)),
    })
}

#[test]
fn evaluate_answer_matrix_agrees_with_the_js() {
    for case in golden()["evaluate"].as_array().into_iter().flatten() {
        let row = challenge_of(&case[0]);
        let ev = svc::evaluate_answer(row.as_ref(), "G", "U", s(&case[1]), 1_000_000);
        assert_eq!(ev.result.as_str(), case[2]["result"].as_str().unwrap_or_default(), "{case}");
        assert_eq!(ev.attempts_left.map(i64::from), case[2]["attemptsLeft"].as_i64(), "{case}");
    }
}

#[test]
fn timeout_validation_and_deadline() {
    let g = golden();
    for case in g["timeout"].as_array().into_iter().flatten() {
        let got = svc::validate_timeout_value(&case[0]);
        match case[1].get("minutes") {
            Some(m) => assert_eq!(got, Ok(m.as_i64().unwrap_or(-1)), "{case}"),
            None => assert_eq!(got, Err(case[1]["error"].as_str().unwrap_or_default().to_string()), "{case}"),
        }
    }
    for case in g["runAt"].as_array().into_iter().flatten() {
        let at = svc::timeout_run_at(case[0].as_i64(), case[1].as_i64().unwrap_or(0), 5_000_000_000);
        assert_eq!(at.timestamp_millis(), case[2].as_i64().unwrap_or(0), "{case}");
    }
}

#[test]
fn verify_button_and_panel_template_agree_with_the_js() {
    let g = golden();
    for case in g["verifyButton"].as_array().into_iter().flatten() {
        let b = svc::parse_verify_button(Some(&case[0]).filter(|v| !v.is_null()));
        assert_eq!(b.to_json(), case[1], "{case}");
    }
    for case in g["panelTemplate"].as_array().into_iter().flatten() {
        assert_eq!(svc::panel_template_of(Some(&case[0]).filter(|v| !v.is_null())), case[1], "{case}");
    }
    assert_eq!(svc::default_panel_template(), g["defaultPanel"]);
}

fn settings_from(v: &Value) -> bot_db::GuildSettings {
    let mut s = bot_db::GuildSettings { guild_id: "900000000000000001".into(), ..Default::default() };
    s.embed_color = v["embedColor"].as_i64().map(|c| c as i32);
    s.captcha_template = v.get("captchaTemplate").filter(|t| !t.is_null()).cloned();
    s
}

fn panel_ctx() -> bot_util::placeholder_parser::RenderContext {
    use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild};
    bot_util::placeholder_parser::RenderContext {
        guild: Some(PlaceholderGuild { id: "900000000000000001".into(), name: Some("Test".into()), member_count: Some(3.0), ..Default::default() }),
        channel: Some(PlaceholderChannel { id: "800000000000000001".into(), name: Some("verification".into()), kind: Some(0) }),
        ..Default::default()
    }
}

#[test]
fn panel_payload_agrees_with_the_js() {
    for case in golden()["panel"].as_array().into_iter().flatten() {
        let s = settings_from(&case[0]);
        let r = svc::build_panel_payload(Some(&s), &panel_ctx());
        let ours = r.to_json();
        let embeds: Vec<Value> = ours["embeds"].as_array().into_iter().flatten().map(embed_core).collect();
        let theirs: Vec<Value> = case[1]["embeds"].as_array().into_iter().flatten().map(embed_core).collect();
        assert_eq!(embeds, theirs, "{case}");
        assert_eq!(ours.get("content").cloned().unwrap_or(Value::Null), case[1].get("content").cloned().unwrap_or(Value::Null), "{case}");
        assert_eq!(buttons(&ours["components"]), buttons(&case[1]["components"]), "{case}");
        let last = buttons(&ours["components"]).pop().unwrap_or_default();
        assert_eq!(last.0, "captcha:start", "the Verify button is last");
    }
}

#[test]
fn challenge_payload_agrees_with_the_js_and_leaks_nothing() {
    let g = golden();
    let at = DateTime::from_timestamp_millis(1_800_000_000_000).unwrap_or_default();
    let s = bot_db::GuildSettings { embed_color: Some(0x00ff00), ..Default::default() };
    let r = svc::build_challenge_payload(Some(&s), &at, b"png".to_vec(), "prefix");
    assert_eq!(reply_json(&r), js_reply(&g["challenge"]));
    assert_eq!(r.files.len(), 1);
    assert_eq!(r.files[0].filename, "captcha.png");
    assert!(r.files[0].description.is_none(), "no alt text");
    let edit = serde_json::to_value(r.to_edit()).unwrap_or(Value::Null);
    assert_eq!(edit["attachments"], json!([{ "id": 0, "filename": "captcha.png", "description": null }]), "only the new image is kept");
    let full = serde_json::to_value(svc::msg::already().to_edit()).unwrap_or(Value::Null);
    assert_eq!(full["attachments"], json!([]), "a full payload removes the stale image");
    assert_eq!(full["content"], "");
    assert_eq!(full["components"], json!([]));
}

#[test]
fn member_messages_agree_with_the_js() {
    let g = golden();
    let m = &g["msg"];
    assert_eq!(svc::msg::module_off(), m["MODULE_OFF"].as_str().unwrap_or_default());
    assert_eq!(svc::msg::FONT_MISSING, m["FONT_MISSING"].as_str().unwrap_or_default());
    let pairs: [(&str, Reply); 9] = [
        ("TEMP_UNAVAILABLE", svc::msg::temp_unavailable()),
        ("NOT_SET_UP", svc::msg::not_set_up()),
        ("ALREADY", svc::msg::already()),
        ("SCREENING_FIRST", svc::msg::screening_first()),
        ("RATE_USER", svc::msg::rate_user()),
        ("RATE_GUILD", svc::msg::rate_guild()),
        ("IMAGE_FAILED", svc::msg::image_failed()),
        ("ROLE_FAILED", svc::msg::role_failed()),
        ("VERIFIED", svc::msg::verified("Cool **Server**")),
    ];
    for (k, r) in pairs {
        assert_eq!(reply_json(&r), js_reply(&m[k]), "{k}");
    }
    for case in g["needsRole"].as_array().into_iter().flatten() {
        let ids = |v: &Value| v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect::<Vec<_>>();
        let button = case[2]
            .as_bool()
            .unwrap_or(false)
            .then(|| feat_welcome::application_service::build_apply_button("FORM1", Some(svc::FORM_BUTTON_LABEL)));
        let r = svc::msg::needs_role(svc::NeedsRole { role_ids: ids(&case[0]), linked_role_ids: ids(&case[1]), form_button: button });
        assert_eq!(reply_json(&r), js_reply(&case[3]), "{case}");
    }
}

#[test]
fn required_roles_and_the_fallback_form_agree_with_the_js() {
    let g = golden();
    let strs = |v: &Value| v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect::<Vec<String>>();
    for case in g["missingRequirement"].as_array().into_iter().flatten() {
        let got = svc::missing_requirement(&strs(&case[0]), &strs(&case[1]), &strs(&case[2]), s(&case[3]));
        assert_eq!(got.map(|v| json!(v)).unwrap_or(Value::Null), case[4], "{case}");
    }
    for case in g["checkRequired"].as_array().into_iter().flatten() {
        let role = &case[0];
        let chk = svc::check_required_role(
            role["id"].as_str().unwrap_or_default(),
            role["managed"].as_bool().unwrap_or(false),
            role["tags"]["guildConnections"].as_bool().unwrap_or(false),
            s(&case[1]).unwrap_or_default(),
            s(&case[2]),
        );
        assert_eq!(chk.ok, case[3]["ok"].as_bool().unwrap_or(false), "{case}");
        assert_eq!(chk.reason.map(Value::from).unwrap_or(Value::Null), case[3].get("reason").cloned().unwrap_or(Value::Null), "{case}");
    }
    for case in g["planRequired"].as_array().into_iter().flatten() {
        let c = &case[0];
        let (current, roles) = (strs(&c["current"]), strs(&c["guildRoleIds"]));
        let got = svc::plan_required_roles(&svc::RequireChange {
            current: &current,
            guild_role_ids: &roles,
            add_id: s(&c["addId"]),
            remove_id: s(&c["removeId"]),
            clear: c["clear"].as_bool().unwrap_or(false),
            max: None,
        });
        let js = &case[1];
        match got {
            Ok(p) => {
                assert_eq!(js["ok"], true, "{case}");
                assert_eq!(json!(p.role_ids), js["roleIds"], "{case}");
                assert_eq!(p.outcome.as_str(), js["outcome"].as_str().unwrap_or_default(), "{case}");
                assert_eq!(p.changed, js["changed"].as_bool().unwrap_or(false), "{case}");
                assert_eq!(p.dropped as i64, js["dropped"].as_i64().unwrap_or(-1), "{case}");
            }
            Err(reason) => assert_eq!(Some(reason.as_str()), js["reason"].as_str(), "{case}"),
        }
    }
    for case in g["fallbackProblem"].as_array().into_iter().flatten() {
        let form: Option<bot_db::ApplicationForm> = (!case[0].is_null()).then(|| {
            let f = &case[0];
            bot_db::ApplicationForm {
                id: "F".into(),
                guild_id: "G".into(),
                name: "Apply".into(),
                description: None,
                questions: f["questions"].clone(),
                review_channel_id: f["reviewChannelId"].as_str().map(str::to_string),
                accept_role_ids: Vec::new(),
                required_role_ids: strs(&f["requiredRoleIds"]),
                cooldown_hours: 0,
                accept_message: None,
                deny_message: None,
                panel_channel_id: None,
                panel_message_id: None,
                enabled: f["enabled"].as_bool().unwrap_or(false),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        });
        let got = svc::fallback_form_problem(form.as_ref(), case[1].as_bool().unwrap_or(false), &strs(&case[3]), case[2].as_str().map(str::to_string), Utc::now());
        assert_eq!(got.map(Value::from).unwrap_or(Value::Null), case[4], "{case}");
    }
    for case in g["describe"].as_array().into_iter().flatten() {
        let st = bot_db::GuildSettings {
            captcha_required_role_ids: strs(&case[0]["captchaRequiredRoleIds"]),
            captcha_role_id: case[0]["captchaRoleId"].as_str().map(str::to_string),
            ..Default::default()
        };
        let fb = svc::FallbackForm {
            form_id: case[1]["formId"].as_str().map(str::to_string),
            form: case[1]["form"].get("name").and_then(Value::as_str).map(|n| bot_db::ApplicationForm {
                id: "F".into(),
                guild_id: "G".into(),
                name: n.into(),
                description: None,
                questions: json!([]),
                review_channel_id: None,
                accept_role_ids: Vec::new(),
                required_role_ids: Vec::new(),
                cooldown_hours: 0,
                accept_message: None,
                deny_message: None,
                panel_channel_id: None,
                panel_message_id: None,
                enabled: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }),
            problem: case[1]["problem"].as_str().map(str::to_string),
        };
        let (roles, form) = svc::describe_requirements(|id| id == "R1" || id == "V", &st, &fb);
        assert_eq!(json!({ "roles": roles, "form": form }), case[2], "{case}");
    }
}

#[test]
fn texts_keys_and_forms_agree_with_the_js() {
    let g = golden();
    for case in g["progress"].as_array().into_iter().flatten() {
        let r = &case[0];
        let n = |k: &str| r[k].as_u64().unwrap_or(0) as usize;
        let run = svc::BulkRun { done: n("done"), total: n("total"), ok: n("ok"), failed: n("failed"), left: n("left"), ..Default::default() };
        assert_eq!(svc::progress_text(&run), case[1].as_str().unwrap_or_default());
    }
    for case in g["discordError"].as_array().into_iter().flatten() {
        let code = case[0].as_u64();
        let msg = code.map(|_| "boom");
        assert_eq!(svc::discord_error_text(code, msg), case[1].as_str().unwrap_or_default(), "{case}");
    }
    assert_eq!(svc::timeout_job_key("900000000000000001", "600000000000000001"), g["keys"]["timeoutJob"].as_str().unwrap_or_default());
    let ids = &g["keys"]["customIds"];
    assert_eq!(svc::custom_ids::START, ids["start"].as_str().unwrap_or_default());
    assert_eq!(svc::custom_ids::ANSWER, ids["answer"].as_str().unwrap_or_default());
    assert_eq!(svc::custom_ids::MODAL_ANSWER, ids["modalAnswer"].as_str().unwrap_or_default());
    assert_eq!(svc::custom_ids::MODAL_PANEL, ids["modalPanel"].as_str().unwrap_or_default());
    assert_eq!(svc::custom_ids::ANSWER_FIELD, ids["answerField"].as_str().unwrap_or_default());
    assert_eq!(serde_json::to_value(svc::answer_modal()).unwrap_or(Value::Null), g["answerModal"]);
    assert_eq!(svc::guild_image_key("G"), "captcha:img:G");
    assert_eq!(svc::user_image_key("G", "U"), "captcha:img:G:U");
    assert_eq!(svc::kick_dm_key("G"), "captcha:dm:G");
    // The JS builds the cache keys inline: check the templates are still the ones in captchaService.js.
    let js = std::fs::read_to_string(bot_core::testing::repo_root().join("src/services/captchaService.js")).unwrap_or_default();
    for t in ["`captcha:img:${guildId}`", "`captcha:img:${guildId}:${userId}`", "`captcha:dm:${member.guild.id}`"] {
        assert!(js.contains(t), "{t}");
    }
    assert_eq!(svc::wrong_try_text(1), "❌ That code is not right. **1** try left.");
    assert_eq!(svc::wrong_try_text(2), "❌ That code is not right. **2** tries left.");
    assert_eq!(svc::fresh_image_prefix(Verdict::Expired), "⌛ That code expired. Here's a new image.");
}

#[test]
fn role_newly_added() {
    use serenity::all::RoleId;
    let r = [RoleId::new(5)];
    assert!(svc::is_role_newly_added(Some(&[]), &r, true, Some("5")));
    assert!(!svc::is_role_newly_added(Some(&r), &r, true, Some("5")));
    assert!(!svc::is_role_newly_added(Some(&r), &[], true, Some("5")));
    assert!(svc::is_role_newly_added(Some(&[]), &r, false, Some("5")));
    assert!(!svc::is_role_newly_added(Some(&[]), &r, true, None));
}

#[test]
fn the_captcha_image_is_a_small_png_without_the_code() {
    assert!(svc::is_image_captcha_available());
    let t0 = std::time::Instant::now();
    let png = svc::render_captcha_image("7KX4PM").expect("rendered");
    assert!(t0.elapsed().as_millis() < 2000);
    assert_eq!(&png[..4], &[0x89, 0x50, 0x4e, 0x47]);
    assert!(png.len() <= 200 * 1024);
    assert!(!png.windows(6).any(|w| w == b"7KX4PM"));
    let decoded = image_size(&png);
    assert_eq!(decoded, (svc::IMAGE_WIDTH, svc::IMAGE_HEIGHT));
}

fn image_size(png: &[u8]) -> (u32, u32) {
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    (w, h)
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["captcha", "sticky"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "captcha");
    bot_core::testing::assert_plugin_matches_golden(&reg, "sticky");
    assert!(bot_core::testing::persistent_handler_problems_for(&reg, &["captcha", "sticky"]).is_empty());
    let json = feat_captcha::commands::captcha::command().payload();
    let subs: Vec<&str> = json["options"].as_array().into_iter().flatten().filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(subs, ["setup", "enable", "disable", "status", "mode", "role", "require", "channel", "timeout", "message", "lock", "verify-existing"]);
    let at = |e: &str, id: &str| bot_config::hook_index(e, id);
    assert!(at("guildMemberAdd", "captcha") < at("guildMemberAdd", "welcome"), "the gate runs before welcome");
}

#[tokio::test]
async fn the_hook_filters_pass_without_a_memo_and_follow_the_settings() {
    let g = "955029000000000999";
    let t = TestApp::new().with_guilds([g]);
    assert!(feat_captcha::plugins::captcha::join_filter(&t, g));
    assert!(feat_captcha::plugins::captcha::leave_filter(&t, g));
    let gid = serenity::all::GuildId::new(g.parse().unwrap_or(1));
    let member = member_json(g, "955029111111100001", &[]);
    let upd = GuildMemberUpdate { guild_id: gid, old_member: Some(member.clone()), member: member.clone(), old_known: true };
    assert!(feat_captcha::plugins::captcha::update_filter(&t, &upd), "no memo: passes");
    t.settings().update_settings(g, SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", "955029000000000500")).await.expect("memory");
    assert!(feat_captcha::plugins::captcha::join_filter(&t, g));
    assert!(!feat_captcha::plugins::captcha::update_filter(&t, &upd), "the role was not added");
    let with = member_json(g, "955029111111100001", &["955029000000000500"]);
    let upd = GuildMemberUpdate { guild_id: gid, old_member: Some(member), member: with, old_known: true };
    assert!(feat_captcha::plugins::captcha::update_filter(&t, &upd));
    t.settings().update_settings(g, SettingsPatch::new().set("captchaEnabled", false).set("captchaRoleId", Value::Null)).await.expect("memory");
    assert!(!feat_captcha::plugins::captcha::join_filter(&t, g));
    assert!(!feat_captcha::plugins::captcha::leave_filter(&t, g));
}

fn member_json(guild: &str, id: &str, roles: &[&str]) -> serenity::all::Member {
    serde_json::from_value(json!({
        "guild_id": guild,
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null },
        "roles": roles, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "pending": false
    }))
    .expect("member json")
}

#[tokio::test]
async fn a_malformed_timeout_payload_is_a_permanent_failure() {
    let t = TestApp::new();
    for payload in [json!({}), json!({ "userId": "abc" }), json!({ "userId": 123 }), json!({ "userId": "1234" })] {
        let job = bot_core::scheduler::Job {
            id: "J".into(),
            guild_id: "955029000000000001".into(),
            job_type: "captcha.timeout".into(),
            dedupe_key: None,
            run_at: Utc::now(),
            payload,
            attempts: 0,
            max_attempts: 5,
            version: 0,
        };
        let ctx = JobCtx { app: (*t).clone(), guild_id: "955029000000000001".into(), late_by_ms: 0, attempt: 1 };
        let err = svc::run_timeout_job(job, ctx).await.expect_err("refused");
        assert!(err.downcast_ref::<PermanentJobError>().is_some(), "{err}");
    }
}

// ── Database ──────────────────────────────────────────────────────────────────

const ROLE_BOT: &str = "955029000000000100";
const ROLE_VERIFIED: &str = "955029000000000101";
const ROLE_ADMIN: &str = "955029000000000102";
const ROLE_REQ: &str = "955029000000000103";
const CH_VERIFY: &str = "955029000000000200";
const ADMIN: &str = "955029111111100009";
const OWNER: &str = "955029111111100099";

fn world(g: &str, members: &[(&str, &[&str])]) -> FakeGuild {
    let mut guild = FakeGuild::new(g, OWNER)
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role(ROLE_BOT, 10, P::MANAGE_ROLES | P::KICK_MEMBERS | P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::READ_MESSAGE_HISTORY | P::MANAGE_CHANNELS)
        .role(ROLE_VERIFIED, 2, P::empty())
        .role(ROLE_ADMIN, 5, P::MANAGE_GUILD)
        .role(ROLE_REQ, 3, P::empty())
        .member(TEST_BOT_ID, &[ROLE_BOT])
        .member(ADMIN, &[ROLE_ADMIN])
        .channel(CH_VERIFY, 0, "verification", &[]);
    for (id, roles) in members {
        guild = guild.member(id, roles);
    }
    guild
}

struct Db {
    t: DbTestApp,
    rest: FakeRest,
    g: &'static str,
}

impl Db {
    async fn new(g: &'static str, members: &[(&str, &[&str])]) -> Option<Self> {
        let url = database_url()?;
        // With Redis when it is there: the JS side reads the settings through the same cache.
        let t = match redis_url() {
            Some(r) => DbTestApp::with_redis(&url, &r).await,
            None => DbTestApp::new(&url).await,
        }
        .with_guilds([g]);
        t.reset_guilds(&[g]).await;
        t.settings().invalidate_settings(g).await;
        let rest = FakeRest::new();
        t.set_discord(rest.discord(&[world(g, members).json()]));
        t.install(registry());
        Some(Self { t, rest, g })
    }

    async fn settings(&self, patch: SettingsPatch) {
        self.t.settings().update_settings(self.g, patch).await.expect("settings");
    }

    async fn row(&self, u: &str) -> Option<bot_db::CaptchaVerification> {
        svc::find_row(self.t.db(), self.g, u).await.expect("row")
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn gid(g: &str) -> serenity::all::GuildId {
    serenity::all::GuildId::new(g.parse().unwrap_or(1))
}

#[tokio::test]
async fn db_join_gate_records_the_row_flags_welcome_and_schedules_the_kick() {
    let u = "955029111111100001";
    let Some(d) = Db::new("955029000000000001", &[(u, &[])]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaTimeoutMinutes", 30)).await;
    let member = member_json(d.g, u, &[]);
    let ctx = d.t.run_hooks(GuildMemberAdd { guild_id: gid(d.g), member: member.clone(), user: member.user.clone() }).await;
    assert_eq!(ctx.state.get::<bool>(feat_welcome::welcome_service::CAPTCHA_PENDING_KEY), Some(true));
    let row = d.row(u).await.expect("pending row");
    assert_eq!((row.answer_hash.clone(), row.attempts, row.expires_at), (None, 0, None));
    let job = d.t.scheduler().get_job(&svc::timeout_job_key(d.g, u)).await.expect("job");
    assert_eq!(job.r#type, "captcha.timeout");
    assert_eq!(job.payload, json!({ "userId": u }));
    let at = job.run_at.timestamp_millis() - Utc::now().timestamp_millis();
    assert!((55_000..=60_000).contains(&at), "a join long ago: never less than a minute away: {at}");

    // A member who already has the role is not gated; a leave cleans up the row and the job.
    let holder = member_json(d.g, "955029111111100002", &[ROLE_VERIFIED]);
    let c2 = d.t.run_hooks(GuildMemberAdd { guild_id: gid(d.g), member: holder.clone(), user: holder.user.clone() }).await;
    assert_eq!(c2.state.get::<bool>("captchaPending"), None);
    assert!(d.row("955029111111100002").await.is_none());
    d.t.run_hooks(GuildMemberRemove { guild_id: gid(d.g), member: None, user: member.user.clone() }).await;
    assert!(d.row(u).await.is_none());
    assert!(d.t.scheduler().get_job(&svc::timeout_job_key(d.g, u)).await.is_none());
    d.done().await;
}

#[tokio::test]
async fn db_join_gate_auto_verifies_bots_and_skips_a_deleted_role() {
    let Some(d) = Db::new("955029000000000002", &[]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED)).await;
    let mut bot = member_json(d.g, "955029111111100003", &[]);
    bot.user.bot = true;
    let ctx = d.t.run_hooks(GuildMemberAdd { guild_id: gid(d.g), member: bot.clone(), user: bot.user.clone() }).await;
    assert_eq!(ctx.state.get::<bool>("captchaPending"), None);
    assert!(d.row("955029111111100003").await.is_none());
    let puts = d.rest.calls_to("PUT", "/guilds/*/members/*/roles/*");
    assert_eq!(puts.len(), 1, "the bot got the verified role");
    assert_eq!(puts[0].reason.as_deref(), Some("Captcha: bots skip verification"));

    d.settings(SettingsPatch::new().set("captchaRoleId", "955029000000000999")).await;
    let m = member_json(d.g, "955029111111100004", &[]);
    d.t.run_hooks(GuildMemberAdd { guild_id: gid(d.g), member: m.clone(), user: m.user.clone() }).await;
    assert!(d.row("955029111111100004").await.is_none(), "a deleted role never gates");
    d.done().await;
}

fn press(g: &str, u: &str, custom_id: &str) -> FakeInteraction {
    FakeInteraction::button(custom_id).guild(g).user(u)
}

#[tokio::test]
async fn db_verify_in_button_mode_and_the_role_given_by_someone_else() {
    let u = "955029111111100011";
    let Some(d) = Db::new("955029000000000011", &[(u, &[])]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaMode", "BUTTON")).await;
    svc::record_pending(&d.t, d.g, u).await.expect("row");
    bot_core::router::route_interaction(&d.t, press(d.g, u, "captcha:start").build()).await;
    let calls = d.t.responses.calls();
    assert_eq!(d.t.responses.names(), vec!["deferReply", "editReply"], "{calls:?}");
    assert!(calls[0].ephemeral(), "the public panel answers ephemerally");
    assert_eq!(calls[1].embed_title(), Some("✅  Verified"));
    assert_eq!(calls[1].body["attachments"], json!([]));
    assert!(d.row(u).await.is_none(), "verified: the row is gone");
    let puts = d.rest.calls_to("PUT", "/guilds/*/members/*/roles/*");
    assert_eq!(puts[0].reason.as_deref(), Some("Captcha verification passed"));

    // Module off: the component gate answers before the handler; with it on but the role missing: not set up.
    d.settings(SettingsPatch::new().set("captchaRoleId", "955029000000000999")).await;
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, press(d.g, u, "captcha:start").build()).await;
    assert_eq!(d.t.responses.calls()[1].embed_title(), Some("❌  Verification Unavailable"));

    // A pending member who gets the role from staff is verified by the update hook.
    d.settings(SettingsPatch::new().set("captchaRoleId", ROLE_VERIFIED)).await;
    let v = "955029111111100012";
    svc::record_pending(&d.t, d.g, v).await.expect("row");
    let before = member_json(d.g, v, &[]);
    let after = member_json(d.g, v, &[ROLE_VERIFIED]);
    d.t.run_hooks(GuildMemberUpdate { guild_id: gid(d.g), old_member: Some(before), member: after, old_known: true }).await;
    assert!(d.row(v).await.is_none());
    d.done().await;
}

#[tokio::test]
async fn db_verify_refuses_screening_and_holds_back_members_without_a_required_role() {
    let u = "955029111111100021";
    let Some(d) = Db::new("955029000000000021", &[(u, &[])]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaMode", "BUTTON")).await;
    let mut screening = press(d.g, u, "captcha:start");
    screening.json_mut()["member"]["pending"] = json!(true);
    bot_core::router::route_interaction(&d.t, screening.build()).await;
    assert_eq!(d.t.responses.calls()[1].embed_title(), Some("⚠️  Rules Screening First"));

    d.t.settings().update_settings_with(d.g, |_| Ok(SettingsPatch::new().set("captchaRequiredRoleIds", json!([ROLE_REQ])))).await.expect("settings");
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, press(d.g, u, "captcha:start").build()).await;
    let call = &d.t.responses.calls()[1];
    assert_eq!(call.embed_title(), Some("⚠️  Role Required"));
    assert!(call.embed_description().unwrap_or_default().starts_with(&format!("To verify here you need one of these roles: <@&{ROLE_REQ}>.")));
    assert!(d.rest.calls_to("PUT", "/guilds/*/members/*/roles/*").is_empty(), "nothing was given");
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, press(d.g, u, "captcha:start").roles(&[ROLE_REQ]).build()).await;
    assert_eq!(d.t.responses.calls()[1].embed_title(), Some("✅  Verified"));
    d.done().await;
}

#[tokio::test]
async fn db_image_mode_challenge_answer_form_and_attempts() {
    let u = "955029111111100031";
    let Some(d) = Db::new("955029000000000031", &[(u, &[])]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaMode", "IMAGE")).await;
    bot_core::router::route_interaction(&d.t, press(d.g, u, "captcha:start").build()).await;
    let call = &d.t.responses.calls()[1];
    assert_eq!(call.embed_title(), Some("ℹ️  🔐 Verification"));
    let body = call.body.to_string();
    assert!(body.contains("attachment://captcha.png"));
    let row = d.row(u).await.expect("challenge row");
    let hash = row.answer_hash.clone().expect("hash");
    assert_eq!(hash.len(), 64);
    assert_eq!(row.attempts, 0);
    let ttl = row.expires_at.map(|t| t.timestamp_millis() - Utc::now().timestamp_millis()).unwrap_or(0);
    assert!((590_000..=600_500).contains(&ttl), "{ttl}");
    // No code anywhere in what was sent.
    let code_like = svc::CAPTCHA_ALPHABET;
    let desc = call.embed_description().unwrap_or_default();
    assert!(!desc.split(|c: char| !c.is_alphanumeric()).any(|w| w.len() == 6 && w.chars().all(|c| code_like.contains(c))), "{desc}");

    // Answer form: the button shows the modal; a known code is stored, then wrong, wrong, right.
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, press(d.g, u, "captcha:answer").build()).await;
    assert_eq!(d.t.responses.names(), vec!["showModal"]);
    let at = DateTime::from_timestamp_millis(Utc::now().timestamp_millis() + 600_000).unwrap_or_default();
    svc::store_challenge(&d.t, d.g, u, &svc::hash_answer(d.g, u, "7KX4PM"), at).await.expect("stored");
    let submit = |input: &str| {
        let mut f = FakeInteraction::modal("captcha-modal:answer", &[("captcha-answer", input)]).guild(d.g).user(u);
        f.json_mut()["message"] = msg_json("955029000000000777");
        f.build()
    };
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, submit("AAAAAA")).await;
    assert_eq!(d.t.responses.names(), vec!["deferUpdate", "editReply"]);
    assert_eq!(d.t.responses.calls()[1].content(), Some("❌ That code is not right. **2** tries left."));
    assert_eq!(d.row(u).await.map(|r| r.attempts), Some(1));
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, submit(" 7kx-4pm ")).await;
    assert_eq!(d.t.responses.calls()[1].embed_title(), Some("✅  Verified"));
    assert!(d.row(u).await.is_none());

    // Exhausted: a fresh image with the prefix.
    let v = "955029111111100032";
    d.t.settings().ensure_guild_settings(d.g).await.expect("ensure");
    svc::store_challenge(&d.t, d.g, v, &svc::hash_answer(d.g, v, "7KX4PM"), at).await.expect("stored");
    bot_db::sqlx::query(r#"UPDATE "CaptchaVerification" SET "attempts" = 2 WHERE "guildId" = $1 AND "userId" = $2"#).bind(d.g).bind(v).execute(d.t.db()).await.expect("set");
    d.t.responses.clear();
    let mut f = FakeInteraction::modal("captcha-modal:answer", &[("captcha-answer", "AAAAAA")]).guild(d.g).user(v);
    f.json_mut()["message"] = msg_json("955029000000000778");
    bot_core::router::route_interaction(&d.t, f.build()).await;
    let call = &d.t.responses.calls()[1];
    assert_eq!(call.content(), Some("❌ Too many wrong tries. Here's a new image."));
    let fresh = d.row(v).await.expect("row");
    assert_eq!(fresh.attempts, 0);
    assert_ne!(fresh.answer_hash, Some(svc::hash_answer(d.g, v, "7KX4PM")));

    // The module is off: the form says so itself (its prefix is not gated).
    d.settings(SettingsPatch::new().set("captchaEnabled", false)).await;
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, submit("AAAAAA")).await;
    let call = &d.t.responses.calls()[0];
    assert_eq!(call.method, "reply");
    assert!(call.ephemeral());
    assert_eq!(call.content(), Some(svc::msg::module_off().as_str()));
    d.done().await;
}

type Seen = Arc<Mutex<Vec<feat_moderation::moderation_service::ModActionInput>>>;

fn seam(t: &App, outcome: KickOutcome) -> Seen {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    svc::set_mod_action(
        t,
        Some(Arc::new(move |input| {
            if let Ok(mut s) = s2.lock() {
                s.push(input);
            }
            let o = outcome.clone();
            Box::pin(async move { o })
        })),
    );
    seen
}

fn timeout_job(g: &str, u: &str) -> (bot_core::scheduler::Job, String) {
    let job = bot_core::scheduler::Job {
        id: "J".into(),
        guild_id: g.into(),
        job_type: "captcha.timeout".into(),
        dedupe_key: Some(svc::timeout_job_key(g, u)),
        run_at: Utc::now(),
        payload: json!({ "userId": u }),
        attempts: 0,
        max_attempts: 5,
        version: 0,
    };
    (job, g.to_string())
}

#[tokio::test]
async fn db_timeout_job_kicks_through_moderation_and_handles_every_result() {
    let u = "955029111111100041";
    let Some(d) = Db::new("955029000000000041", &[(u, &[])]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaTimeoutMinutes", 30)).await;
    let member = json!({
        "user": { "id": u, "username": "late", "discriminator": "0", "avatar": null },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    });
    d.rest.on("GET", &format!("/guilds/{}/members/{u}", d.g), 200, member.clone());
    let run = |attempt: i32| {
        let (job, g) = timeout_job(d.g, u);
        svc::run_timeout_job(job, JobCtx { app: (*d.t).clone(), guild_id: g, late_by_ms: 0, attempt })
    };

    // Kicked: one DM first, the moderation contract, the row goes.
    let seen = seam(&d.t, KickOutcome::Kicked);
    svc::record_pending(&d.t, d.g, u).await.expect("row");
    run(1).await.expect("ran");
    assert!(d.row(u).await.is_none());
    let input = seen.lock().map(|s| s[0].clone()).expect("called");
    assert_eq!((input.action.as_str(), input.source.as_str(), input.silent, input.skip_escalation), ("KICK", "captcha", true, true));
    assert_eq!(input.reason.as_deref(), Some("Captcha: not verified within 30 minutes"));
    assert_eq!(input.target_id, u);
    let dms = d.rest.calls_to("POST", "/channels/*/messages");
    assert_eq!(dms.len(), 1);
    assert!(dms[0].body["content"].as_str().unwrap_or_default().starts_with("You were removed from **"));

    // A retry does not DM twice; a kick whose case was not saved still clears the row.
    d.rest.clear_calls();
    seam(&d.t, KickOutcome::Failed { code: Some("DISCORD_ERROR".into()), error: "db".into(), applied: true });
    svc::record_pending(&d.t, d.g, u).await.expect("row");
    run(2).await.expect("ran");
    assert!(d.rest.calls_to("POST", "/channels/*/messages").is_empty());
    assert!(d.row(u).await.is_none());

    for (code, kept, fails) in [("NOT_IN_GUILD", false, false), ("IMMUNE", true, false), ("HIERARCHY", true, false), ("NOT_PERMITTED", true, false), ("DISCORD_ERROR", true, true)] {
        seam(&d.t, KickOutcome::Failed { code: Some(code.into()), error: "no".into(), applied: false });
        svc::record_pending(&d.t, d.g, u).await.expect("row");
        let r = run(2).await;
        assert_eq!(r.is_err(), fails, "{code}");
        assert_eq!(d.row(u).await.is_some(), kept, "{code}");
        svc::delete_row(d.t.db(), d.g, u).await.expect("delete");
    }

    // Skips: no row; the member has the role meanwhile; the member left (10007); a transient error retries.
    let seen = seam(&d.t, KickOutcome::Kicked);
    run(1).await.expect("no row: nothing to do");
    svc::record_pending(&d.t, d.g, u).await.expect("row");
    let mut verified = member.clone();
    verified["roles"] = json!([ROLE_VERIFIED]);
    d.rest.on("GET", &format!("/guilds/{}/members/{u}", d.g), 200, verified);
    run(1).await.expect("ran");
    assert!(d.row(u).await.is_none(), "verified meanwhile");
    svc::record_pending(&d.t, d.g, u).await.expect("row");
    d.rest.fail("GET", &format!("/guilds/{}/members/{u}", d.g), 404, 10007, "Unknown Member");
    run(1).await.expect("ran");
    assert!(d.row(u).await.is_none(), "gone");
    svc::record_pending(&d.t, d.g, u).await.expect("row");
    d.rest.fail("GET", &format!("/guilds/{}/members/{u}", d.g), 500, 0, "Internal");
    assert!(run(1).await.is_err(), "a transient error retries");
    assert!(d.row(u).await.is_some());
    assert!(seen.lock().map(|s| s.is_empty()).unwrap_or(false), "never kicked in these paths");

    // Module off or never: no kick, the row is kept.
    d.rest.on("GET", &format!("/guilds/{}/members/{u}", d.g), 200, member.clone());
    d.settings(SettingsPatch::new().set("captchaTimeoutMinutes", 0)).await;
    run(1).await.expect("ran");
    assert!(d.row(u).await.is_some());
    // An administrator is never kicked locally.
    d.settings(SettingsPatch::new().set("captchaTimeoutMinutes", 30)).await;
    let mut admin = member.clone();
    admin["roles"] = json!([ROLE_ADMIN]);
    d.rest.on("GET", &format!("/guilds/{}/members/{u}", d.g), 200, admin);
    run(1).await.expect("ran");
    assert!(seen.lock().map(|s| s.is_empty()).unwrap_or(false));
    assert!(d.row(u).await.is_some());
    d.done().await;
}

fn admin_slash(g: &str, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("captcha").guild(g).user(ADMIN).roles(&[ROLE_ADMIN]).permissions(P::MANAGE_GUILD).subcommand(sub)
}

#[tokio::test]
async fn db_captcha_commands_timeout_mode_role_message_enable_disable_status() {
    let Some(d) = Db::new("955029000000000051", &[]).await else { return };
    use bot_core::command::option_type as ot;
    let route = |f: FakeInteraction| {
        d.t.responses.clear();
        bot_core::router::route_interaction(&d.t, f.build())
    };
    let last = || d.t.responses.calls().last().cloned().expect("an answer");

    d.t.scheduler()
        .schedule_job(
            ScheduleJob::new(d.g, JobType::CaptchaTimeout, Utc::now() + chrono::Duration::hours(1))
                .dedupe_key(svc::timeout_job_key(d.g, "955029111111100051"))
                .payload(json!({ "userId": "955029111111100051" })),
        )
        .await
        .expect("job");
    route(admin_slash(d.g, "timeout").option("minutes", ot::INTEGER, json!(30))).await;
    assert_eq!(last().embed_title(), Some("✅  Timeout Set"));
    assert!(last().embed_description().unwrap_or_default().starts_with("Members who haven't verified **30 minutes** after joining are kicked"));
    route(admin_slash(d.g, "timeout").option("minutes", ot::INTEGER, json!(3))).await;
    assert_eq!(last().embed_title(), Some("❌  Not Saved"));
    assert_eq!(last().embed_description(), Some("Use 0 (never) or between 5 and 10080 minutes (7 days)."));
    route(admin_slash(d.g, "timeout").option("minutes", ot::INTEGER, json!(0))).await;
    assert_eq!(last().embed_description(), Some("Unverified members are never kicked. Cancelled 1 scheduled kick(s)."));

    route(admin_slash(d.g, "mode").option("type", ot::STRING, json!("BUTTON"))).await;
    assert_eq!(last().embed_title(), Some("✅  Verification Mode Set"));
    assert_eq!(d.t.settings().get_settings(d.g).await.expect("s").captcha_mode, bot_db::CaptchaMode::Button);

    route(admin_slash(d.g, "role").option("role", ot::ROLE, json!(ROLE_VERIFIED))).await;
    assert_eq!(last().embed_title(), Some("✅  Verified Role Set"), "{:?}", last());
    route(admin_slash(d.g, "role").option("role", ot::ROLE, json!(ROLE_VERIFIED))).await;
    assert_eq!(last().embed_title(), Some("ℹ️  No Change"));
    route(admin_slash(d.g, "role").option("role", ot::ROLE, json!(ROLE_BOT))).await;
    assert_eq!(last().embed_title(), Some("❌  Cannot Use This Role"));

    route(admin_slash(d.g, "message").option("button_label", ot::STRING, json!(" Let me in ")).option("button_color", ot::STRING, json!("DANGER"))).await;
    assert_eq!(last().embed_description(), Some("Label **Let me in**, colour **Red**.\nNo panel has been posted yet: run `/captcha setup`."));
    let s = d.t.settings().get_settings(d.g).await.expect("s");
    assert_eq!(s.captcha_template.as_ref().map(|t| t["verifyButton"].clone()), Some(json!({ "label": "Let me in", "style": "DANGER" })));
    route(admin_slash(d.g, "message").option("reset", ot::BOOLEAN, json!(true)).option("button_color", ot::STRING, json!("DANGER"))).await;
    assert_eq!(last().embed_description(), Some("Use reset on its own."));
    route(admin_slash(d.g, "message").option("reset", ot::BOOLEAN, json!(true))).await;
    assert_eq!(last().embed_title(), Some("✅  Panel Reset"));
    let null: Option<bool> = bot_db::sqlx::query_scalar(r#"SELECT "captchaTemplate" IS NULL FROM "GuildSettings" WHERE "guildId" = $1"#)
        .bind(d.g)
        .fetch_one(d.t.db())
        .await
        .expect("query");
    assert_eq!(null, Some(true), "reset stores SQL NULL (Prisma.DbNull)");
    route(admin_slash(d.g, "message")).await;
    assert_eq!(d.t.responses.names(), vec!["showModal"]);
    assert_eq!(d.t.responses.calls()[0].body["custom_id"], "captcha-modal:panel");

    route(admin_slash(d.g, "enable")).await;
    assert_eq!(last().embed_title(), Some("✅  Verification Enabled"));
    assert!(last().embed_description().unwrap_or_default().contains("**Fix these first:**"));
    svc::record_pending(&d.t, d.g, "955029111111100052").await.expect("row");
    route(admin_slash(d.g, "disable")).await;
    assert!(last().embed_description().unwrap_or_default().contains("**1** member(s) were still waiting to verify."));
    route(admin_slash(d.g, "status")).await;
    let names: Vec<String> = last().body["embeds"][0]["fields"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(str::to_string)).collect();
    assert_eq!(names, ["Status", "Mode", "Verified role", "Channel", "Kick unverified", "Lock-down", "Waiting to verify", "Required roles", "Fallback form", "Problems"]);
    route(admin_slash(d.g, "verify-existing").option("stop", ot::BOOLEAN, json!(true))).await;
    assert_eq!(last().embed_title(), Some("ℹ️  Nothing Running"));
    route(admin_slash(d.g, "lock").option("state", ot::STRING, json!("off"))).await;
    assert_eq!(last().embed_title(), Some("ℹ️  Not Locked"));
    d.done().await;
}

#[tokio::test]
async fn db_captcha_require_and_setup() {
    let Some(d) = Db::new("955029000000000061", &[]).await else { return };
    use bot_core::command::option_type as ot;
    let route = |f: FakeInteraction| {
        d.t.responses.clear();
        bot_core::router::route_interaction(&d.t, f.build())
    };
    let last = || d.t.responses.calls().last().cloned().expect("an answer");
    d.settings(SettingsPatch::new().set("captchaRoleId", ROLE_VERIFIED)).await;
    route(admin_slash(d.g, "require").option("role", ot::ROLE, json!(ROLE_REQ)).resolve_from(&world(d.g, &[]))).await;
    assert_eq!(last().embed_title(), Some("✅  Requirements Updated"));
    assert!(last().embed_description().unwrap_or_default().starts_with(&format!("<@&{ROLE_REQ}> is now required.")));
    route(admin_slash(d.g, "require").option("role", ot::ROLE, json!(ROLE_REQ))).await;
    assert_eq!(last().embed_title(), Some("ℹ️  No Change"));
    route(admin_slash(d.g, "require").option("role", ot::ROLE, json!(ROLE_VERIFIED))).await;
    assert_eq!(last().embed_title(), Some("❌  Cannot Require This Role"));
    route(admin_slash(d.g, "require").option("clear", ot::BOOLEAN, json!(true)).option("role", ot::ROLE, json!(ROLE_REQ))).await;
    assert_eq!(last().embed_description(), Some("Use `clear` on its own (it can go with `form`)."));
    route(admin_slash(d.g, "require").option("form", ot::STRING, json!("nothing-like-this"))).await;
    assert_eq!(last().embed_title(), Some("❌  Form Not Found"));
    route(admin_slash(d.g, "require")).await;
    assert_eq!(last().embed_title(), Some("ℹ️  Verification Requirements"));
    route(admin_slash(d.g, "require").option("clear", ot::BOOLEAN, json!(true))).await;
    assert!(last().embed_description().unwrap_or_default().starts_with("Every required role was removed: anyone can verify again."));
    assert!(d.t.settings().get_settings(d.g).await.expect("s").captcha_required_role_ids.is_empty());

    // Setup with the stored role and the existing channel: posts the panel and saves.
    route(admin_slash(d.g, "setup").option("channel", ot::CHANNEL, json!(CH_VERIFY)).option("mode", ot::STRING, json!("BUTTON"))).await;
    assert_eq!(last().embed_title(), Some("✅  Verification Ready"), "{:?}", last());
    let s = d.t.settings().get_settings(d.g).await.expect("s");
    assert!(s.captcha_enabled);
    assert_eq!(s.captcha_channel_id.as_deref(), Some(CH_VERIFY));
    let posts = d.rest.calls_to("POST", "/channels/*/messages");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["components"][0]["components"][0]["custom_id"], "captcha:start");
    assert_eq!(s.captcha_message_id, posts[0].body.get("id").and_then(Value::as_str).map(str::to_string).or(s.captcha_message_id.clone()));
    // Moving it edits nothing here and posts again (the old one is in the same channel: edited in place).
    route(admin_slash(d.g, "channel").option("channel", ot::CHANNEL, json!(CH_VERIFY))).await;
    assert_eq!(last().embed_title(), Some("✅  Panel Moved"));
    assert_eq!(d.rest.calls_to("PATCH", "/channels/*/messages/*").len(), 1, "edited in place");
    d.done().await;
}

#[tokio::test]
async fn db_raw_sql_runs() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.expect("tx");
    for (name, sql) in svc::RAW_SQL {
        let mut q = bot_db::sqlx::query(sql).bind("955029000000000071");
        if sql.contains("$2") {
            q = if name == "deleteStale" { q.bind(vec!["1".to_string()]) } else { q.bind("955029111111100071") };
        }
        if sql.contains("$3") {
            q = q.bind("x");
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    for (name, sql) in feat_captcha::sticky_service::RAW_SQL {
        let mut q = bot_db::sqlx::query(sql).bind("955029000000000071");
        if sql.contains("$2") {
            q = q.bind("955029000000000072");
        }
        if sql.contains("$3") {
            q = if name == "setEnabled" { q.bind(true) } else { q.bind("x") };
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    tx.rollback().await.expect("rollback");
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_share_rows_jobs_and_challenges() {
    let u = "955029111111100081";
    let Some(d) = Db::new("955029000000000081", &[(u, &[])]).await else { return };
    // JS writes the join row and the job; Rust reads them and runs the job.
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["join", d.g, ROLE_VERIFIED, u, "30"]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["state"]["captchaPending"], true);
    let row = d.row(u).await.expect("JS row");
    assert_eq!(serde_json::to_value(&row).unwrap_or(Value::Null), js["row"], "the row reads the same");
    let job = d.t.scheduler().get_job(&svc::timeout_job_key(d.g, u)).await.expect("JS job");
    assert_eq!(job.payload, json!({ "userId": u }));
    assert_eq!(job.r#type, "captcha.timeout");
    let seen = seam(&d.t, KickOutcome::Kicked);
    d.rest.on(
        "GET",
        &format!("/guilds/{}/members/{u}", d.g),
        200,
        json!({ "user": { "id": u, "username": "x", "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 }),
    );
    let ctx = JobCtx { app: (*d.t).clone(), guild_id: d.g.into(), late_by_ms: 0, attempt: 1 };
    svc::run_timeout_job(bot_core::scheduler::Job::from_row(&job), ctx).await.expect("ran");
    assert_eq!(seen.lock().map(|s| s.len()).unwrap_or(0), 1);
    assert!(d.row(u).await.is_none());

    // JS issues a real challenge; Rust judges a wrong answer and counts it; JS sees the count.
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["challenge", d.g, u]);
    assert!(js.get("error").is_none(), "{js}");
    let row = d.row(u).await.expect("challenge");
    let ev = svc::evaluate_answer(Some(&Challenge::from(&row)), d.g, u, Some("AAAAAA"), Utc::now().timestamp_millis());
    assert_eq!(ev.result, Verdict::Wrong);
    svc::count_wrong_try(d.t.db(), d.g, u, row.answer_hash.as_deref().unwrap_or_default()).await.expect("counted");
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["check", d.g, u, "AAAAAA"]);
    assert_eq!(js["row"]["attempts"], 1);
    assert_eq!(js["verdict"], json!({ "result": "wrong", "attemptsLeft": 1 }));

    // Rust stores a challenge; the JS accepts its code.
    let at = DateTime::from_timestamp_millis(Utc::now().timestamp_millis() + 600_000).unwrap_or_default();
    svc::store_challenge(&d.t, d.g, u, &svc::hash_answer(d.g, u, "7KX4PM"), at).await.expect("stored");
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["check", d.g, u, "7kx 4pm"]);
    assert_eq!(js["verdict"], json!({ "result": "ok" }));
    assert_eq!(js["row"]["attempts"], 0);
    assert_eq!(js["row"]["expiresAt"], bot_db::format_js_date(&at));

    // Rust records a pending member and schedules the kick; the JS leave hook removes both.
    let v = "955029111111100082";
    svc::record_pending(&d.t, d.g, v).await.expect("row");
    d.t.scheduler()
        .schedule_job(
            ScheduleJob::new(d.g, JobType::CaptchaTimeout, svc::timeout_run_at(None, 30, Utc::now().timestamp_millis()))
                .dedupe_key(svc::timeout_job_key(d.g, v))
                .payload(json!({ "userId": v })),
        )
        .await
        .expect("job");
    assert_eq!(run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["count", d.g]), json!(2));
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["leave", d.g, v]);
    assert_eq!(js["job"], Value::Null);
    assert!(d.row(v).await.is_none());
    assert_eq!(svc::count_pending(d.t.db(), d.g).await.expect("count"), 1);

    // Rust writes the panel settings; the JS reads the same template and button.
    let g = d.g;
    d.t.settings()
        .update_settings_with(g, |cur| {
            let mut t = svc::panel_template(Some(&cur));
            t["verifyButton"] = json!({ "label": "Go", "style": "PRIMARY" });
            Ok(SettingsPatch::new().set("captchaTemplate", t).set("captchaLockdownPrior", true).set("captchaRequiredRoleIds", json!([ROLE_REQ])))
        })
        .await
        .expect("saved");
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["settings", d.g]);
    assert_eq!(js["button"], json!({ "label": "Go", "style": "PRIMARY" }));
    assert_eq!(js["panel"], svc::panel_template(Some(&d.t.settings().get_settings(d.g).await.expect("s"))));
    assert_eq!(js["captchaLockdownPrior"], true);
    assert_eq!(js["captchaRequiredRoleIds"], json!([ROLE_REQ]));
    svc::reset_panel_template(&d.t, d.g).await.expect("reset");
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["settings", d.g]);
    assert_eq!(js["captchaTemplate"], Value::Null);
    d.done().await;
}

#[tokio::test]
async fn js_and_rust_share_the_image_rate_window() {
    let (Some(db), Some(redis)) = (database_url(), redis_url()) else { return };
    let t = DbTestApp::with_redis(&db, &redis).await;
    let key = svc::guild_image_key("955029000000000091");
    t.cache().del_one(&key).await;
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["window", &key, "2"]);
    assert_eq!(js, json!(2));
    let ours = t.cache().sliding_window(&key, svc::GUILD_IMAGE_LIMIT.window_ms, None).await;
    assert_eq!(ours, Some(3), "one window, written by both");
    assert!(svc::check_image_limits(&t, "955029000000000091", "955029111111100091").await.is_none(), "under the caps");
    t.cache().del_one(&key).await;
    t.cache().del_one(&svc::user_image_key("955029000000000091", "955029111111100091")).await;
}

fn role_json(id: &str) -> Value {
    json!({ "id": id, "name": "r", "color": 0, "hoist": false, "position": 1, "permissions": "1024", "managed": false, "mentionable": false, "flags": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null } })
}

#[tokio::test]
async fn db_lock_on_asks_first_then_locks_and_stores_the_prior_state() {
    let Some(d) = Db::new("955029000000000131", &[]).await else { return };
    use bot_core::command::option_type as ot;
    d.settings(SettingsPatch::new().set("captchaRoleId", ROLE_VERIFIED).set("captchaChannelId", CH_VERIFY)).await;
    d.rest.on("PATCH", "/guilds/*/roles/*", 200, role_json(ROLE_VERIFIED));
    let t = d.t.clone();
    let g = d.g;
    let run = tokio::spawn(async move {
        bot_core::router::route_interaction(&t, admin_slash(g, "lock").option("state", ot::STRING, json!("on")).build()).await;
    });
    for _ in 0..200 {
        if !d.t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = d.t.responses.calls()[0].clone();
    assert_eq!(prompt.method, "reply");
    assert!(!prompt.ephemeral(), "the prompt is public");
    assert_eq!(prompt.embed_title(), Some("⚠️  🔒 Lock the server until members verify?"));
    let desc = prompt.embed_description().unwrap_or_default().to_string();
    assert!(desc.starts_with(&format!("This will:\n• let @everyone see <#{CH_VERIFY}>, so new members can verify\n• give <@&{ROLE_VERIFIED}> the **View Channels** permission\n• remove **View Channels** from @everyone, server-wide")), "{desc}");
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or_default().to_string();
    bot_core::router::route_interaction(&d.t, FakeInteraction::button(&yes).guild(d.g).user(ADMIN).message_id("1000000000000000100").build()).await;
    run.await.expect("finished");
    let last = d.t.responses.calls().last().cloned().expect("answer");
    assert_eq!(last.method, "editReply");
    assert_eq!(last.embed_title(), Some("✅  Server Locked"), "{last:?}");
    let perm = d.rest.calls_to("PUT", "/channels/*/permissions/*");
    assert_eq!(perm.len(), 1);
    assert_eq!(perm[0].body["allow"], json!((P::VIEW_CHANNEL | P::READ_MESSAGE_HISTORY).bits().to_string()));
    let roles = d.rest.calls_to("PATCH", "/guilds/*/roles/*");
    assert_eq!(roles.len(), 2);
    assert_eq!(roles[1].path, format!("/guilds/{}/roles/{}", d.g, d.g), "@everyone last");
    assert_eq!(roles[1].body["permissions"], json!(P::SEND_MESSAGES.bits().to_string()));
    assert_eq!(roles[0].reason.as_deref(), Some("captcha lock-down by member"), "auditReason: the user tag");
    assert_eq!(d.t.settings().get_settings(d.g).await.expect("s").captcha_lockdown_prior, Some(true));
    d.done().await;
}

#[tokio::test]
async fn db_verify_existing_gives_the_role_in_the_background_and_reports() {
    let (a, b, p) = ("955029111111100111", "955029111111100112", "955029111111100113");
    let Some(d) = Db::new("955029000000000111", &[(a, &[]), (b, &[ROLE_VERIFIED]), (p, &[])]).await else { return };
    use bot_core::command::option_type as ot;
    d.settings(SettingsPatch::new().set("captchaRoleId", ROLE_VERIFIED)).await;
    svc::record_pending(&d.t, d.g, p).await.expect("pending");
    svc::record_pending(&d.t, d.g, "955029111111100119").await.expect("stale: left while the bot was away");
    let member = |id: &str, roles: &[&str]| json!({ "user": { "id": id, "username": "m", "discriminator": "0", "avatar": null }, "roles": roles, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 });
    let list = json!([member(TEST_BOT_ID, &[ROLE_BOT]), member(ADMIN, &[ROLE_ADMIN]), member(a, &[]), member(b, &[ROLE_VERIFIED]), member(p, &[])]);
    d.rest.on("GET", "/guilds/*/members", 200, list);
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").build()).await;
    let start = d.t.responses.calls().iter().filter(|c| c.method == "editReply").map(|c| c.embed_title().unwrap_or_default().to_string()).collect::<Vec<_>>();
    assert_eq!(start, ["ℹ️  Loading Members", "ℹ️  Verifying Existing Members"]);
    let text = d.t.responses.calls().last().and_then(|c| c.embed_description().map(str::to_string)).unwrap_or_default();
    assert_eq!(text, format!("Giving <@&{ROLE_VERIFIED}> to **3** member(s) in the background. Progress shows here. Stop with `/captcha verify-existing stop:true`.\nSkipped 1 member(s) still waiting to verify."));
    for _ in 0..300 {
        if svc::get_bulk_run(&d.t, d.g).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(svc::get_bulk_run(&d.t, d.g).is_none(), "the guild is released");
    let calls = d.t.responses.calls();
    let fin = calls.iter().find(|c| c.method == "followUp").expect("the summary");
    assert_eq!(fin.embed_title(), Some("✅  Existing Members Verified"));
    assert_eq!(fin.embed_description(), Some(format!("**3/3** processed · ✅ 3 given · ⚠️ 0 failed · 🚪 0 left\nGave <@&{ROLE_VERIFIED}> to 3 member(s).").as_str()));
    assert_eq!(calls.last().map(|c| c.method), Some("deleteReply"));
    assert!(d.row("955029111111100119").await.is_none(), "the stale row was pruned");
    assert!(d.row(p).await.is_some(), "a pending member is spared without include_pending");
    assert_eq!(d.rest.calls_to("PUT", "/guilds/*/members/*/roles/*").len(), 3, "bots included");

    // include_pending finishes their verification too.
    d.t.responses.clear();
    d.rest.clear_calls();
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").option("include_pending", ot::BOOLEAN, json!(true)).build()).await;
    for _ in 0..300 {
        if svc::get_bulk_run(&d.t, d.g).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(d.row(p).await.is_none());
    d.done().await;
}

// ── More of test/captcha.test.js: panel, rules on a guild, failures ───────────

const CH_OTHER: &str = "955029000000000201";

fn matrix_guild(g: &str) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role(ROLE_BOT, 10, P::MANAGE_ROLES | P::KICK_MEMBERS | P::VIEW_CHANNEL)
        .role("955029000000000300", 2, P::empty())
        .role("955029000000000301", 2, P::empty())
        .role("955029000000000302", 2, P::empty())
        .role("955029000000000303", 2, P::empty())
        .role("955029000000000304", 2, P::MANAGE_MESSAGES)
        .role("955029000000000305", 11, P::empty())
        .role("955029000000000306", 3, P::ADMINISTRATOR)
        .role("955029000000000307", 3, P::KICK_MEMBERS)
        .member(TEST_BOT_ID, &[ROLE_BOT])
        .member("955029111111100301", &[])
        .member("955029111111100302", &["955029000000000306"])
        .member("955029111111100303", &["955029000000000307"])
        .member("955029111111100304", &["955029000000000301"])
        .member("955029111111100305", &["955029000000000302"])
        .member(OWNER, &[])
        .channel(CH_VERIFY, 0, "verification", &[])
}

#[tokio::test]
async fn verified_role_rules_lock_facts_and_kick_exemptions_on_a_guild() {
    use serenity::all::{GuildId, RoleId, UserId};
    let g = "955029000000000141";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[matrix_guild(g)]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, GuildId::new(g.parse().unwrap_or(1))).expect("cached");
    let bot = UserId::new(TEST_BOT_ID.parse().unwrap_or(1));
    let role = |id: &str| guild.roles.get(&RoleId::new(id.parse().unwrap_or(1))).cloned();
    let st = bot_db::GuildSettings {
        mod_role_ids: vec!["955029000000000301".into()],
        immune_role_ids: vec!["955029000000000302".into()],
        mute_role_id: Some("955029000000000303".into()),
        ..Default::default()
    };
    let chk = |id: &str| svc::check_verified_role(&guild, role(id).as_ref(), bot, Some(&st), None);
    assert_eq!(chk("955029000000000300"), svc::Check::ok());
    assert!(chk("955029000000000301").reason.unwrap_or_default().contains("moderator role"));
    assert!(chk("955029000000000302").reason.unwrap_or_default().contains("immune role"));
    assert_eq!(chk("955029000000000303").reason.as_deref(), Some("That's the mute role."));
    assert_eq!(
        chk("955029000000000304").reason.as_deref(),
        Some("That role has dangerous permissions: Manage Messages. Everyone who verifies gets this role, so roles with these permissions are refused.")
    );
    assert_eq!(chk("955029000000000305").reason.as_deref(), Some("That role is above my highest role. Move my highest role above it in **Server Settings → Roles**."));
    assert!(!svc::check_verified_role(&guild, None, bot, Some(&st), None).ok);

    assert!(!svc::is_locked(&guild));
    assert!(svc::bot_sees_without_everyone(&guild, bot));
    let channel = bot_core::permissions::find_channel(&guild, serenity::all::ChannelId::new(CH_VERIFY.parse().unwrap_or(1))).cloned().expect("channel");
    assert_eq!(svc::lock_plan(&guild, &role("955029000000000300").expect("role"), &channel), svc::LockPlan { channel: true, role: true, everyone: true });
    assert!(svc::everyone_can_view(&guild, &channel));

    let member = |id: &str| guild.members.get(&UserId::new(id.parse().unwrap_or(1))).cloned().expect("member");
    let exempt = |id: &str| svc::is_kick_exempt(&guild, &member(id), Some(&st), None);
    assert!(!exempt("955029111111100301"), "a plain member");
    assert!(exempt(OWNER));
    assert!(exempt(TEST_BOT_ID), "bots");
    assert!(exempt("955029111111100302"), "administrators");
    assert!(exempt("955029111111100303"), "Kick Members makes a moderator");
    assert!(exempt("955029111111100304"), "a moderator role");
    assert!(exempt("955029111111100305"), "an immune role");

    // @everyone with Administrator but without View Channels still counts as locked; a bot without its own View
    // Channels would be hidden by a lock.
    let locked = FakeGuild::new("955029000000000142", OWNER)
        .everyone(P::ADMINISTRATOR | P::SEND_MESSAGES)
        .role(ROLE_BOT, 10, P::MANAGE_ROLES)
        .member(TEST_BOT_ID, &[ROLE_BOT]);
    let t2 = TestApp::new().with_guilds(["955029000000000142"]).with_fake_guilds(&[locked]);
    let lg = bot_core::interaction_helpers::cached_guild(&t2, GuildId::new(955029000000000142)).expect("cached");
    assert!(svc::is_locked(&lg));
    assert!(!svc::bot_sees_without_everyone(&lg, bot));
}

#[tokio::test]
async fn db_publish_and_refresh_panel_paths() {
    let Some(d) = Db::new("955029000000000151", &[]).await else { return };
    use bot_core::command::option_type as ot;
    let gid = gid(d.g);
    let cid = serenity::all::ChannelId::new(CH_VERIFY.parse().unwrap_or(1));
    let s = || async { d.t.settings().get_settings(d.g).await.expect("s") };
    assert_eq!(svc::refresh_panel(&d.t, gid, &*s().await).await, Err("No panel has been posted yet: run `/captcha setup`.".into()));
    d.settings(SettingsPatch::new().set("captchaChannelId", "955029000000000299").set("captchaMessageId", "955029000000000900")).await;
    assert_eq!(svc::refresh_panel(&d.t, gid, &*s().await).await, Err("The verification channel was deleted: run `/captcha setup`.".into()));
    d.settings(SettingsPatch::new().set("captchaChannelId", CH_VERIFY)).await;
    d.rest.fail("PATCH", "/channels/*/messages/*", 404, 10008, "Unknown Message");
    assert_eq!(svc::refresh_panel(&d.t, gid, &*s().await).await, Err("The panel message was deleted: run `/captcha setup` to post it again.".into()));
    d.rest.fail("PATCH", "/channels/*/messages/*", 403, 50013, "Missing Permissions");
    assert_eq!(svc::refresh_panel(&d.t, gid, &*s().await).await, Err(format!("I can't edit the panel: check my permissions in <#{CH_VERIFY}>.")));

    // The old panel was deleted: posted again.
    d.rest.fail("PATCH", "/channels/*/messages/*", 404, 10008, "Unknown Message");
    d.rest.clear_calls();
    let msg = svc::publish_panel(&d.t, gid, cid, &*s().await, Some(CH_VERIFY), Some("955029000000000900")).await.expect("posted");
    assert_eq!(d.rest.calls().iter().map(|c| c.method.clone()).collect::<Vec<_>>(), ["PATCH", "POST"]);
    assert_eq!(msg.channel_id, cid);

    // A refused post is a config error with the JS text.
    d.rest.fail("POST", "/channels/*/messages", 403, 50013, "Missing Permissions");
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "channel").option("channel", ot::CHANNEL, json!(CH_VERIFY)).build()).await;
    let last = d.t.responses.calls().last().cloned().expect("answer");
    assert_eq!(last.embed_title(), Some("❌  Cannot Post Panel"));
    assert_eq!(
        last.embed_description(),
        Some(format!("I can't post in <#{CH_VERIFY}>: I need **View Channel, Send Messages, Embed Links and Read Message History** there.").as_str())
    );
    d.done().await;
}

#[tokio::test]
async fn db_moving_the_panel_deletes_the_old_message() {
    let Some(url) = database_url() else { return };
    let g = "955029000000000161";
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let rest = FakeRest::new();
    t.set_discord(rest.discord(&[world(g, &[]).channel(CH_OTHER, 0, "other", &[]).json()]));
    t.install(registry());
    let s = t.settings().get_settings(g).await.expect("s");
    let other = serenity::all::ChannelId::new(CH_OTHER.parse().unwrap_or(1));
    svc::publish_panel(&t, gid(g), other, &s, Some(CH_VERIFY), Some("955029000000000901")).await.expect("posted");
    for _ in 0..100 {
        if !rest.calls_to("DELETE", "/channels/*/messages/*").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let del = rest.calls_to("DELETE", "/channels/*/messages/*");
    assert_eq!(del.len(), 1);
    assert_eq!(del[0].path, format!("/channels/{CH_VERIFY}/messages/955029000000000901"));
    t.reset_guilds(&[g]).await;
}

async fn wait_released(t: &App, g: &str) {
    for _ in 0..500 {
        if svc::get_bulk_run(t, g).is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_verify_existing_failures_abort_and_one_run_per_guild() {
    let ids: Vec<String> = (0..7).map(|i| format!("95502911111110{i:04}")).collect();
    let members: Vec<(&str, &[&str])> = ids.iter().map(|i| (i.as_str(), &[][..])).collect();
    let Some(d) = Db::new("955029000000000171", &members).await else { return };
    use bot_core::command::option_type as ot;
    d.settings(SettingsPatch::new().set("captchaRoleId", ROLE_VERIFIED)).await;
    let member = |id: &str| json!({ "user": { "id": id, "username": "m", "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 });
    let list: Vec<Value> = ids.iter().map(|i| member(i)).collect();
    d.rest.on("GET", "/guilds/*/members", 200, Value::Array(list));

    // The role was deleted: the batch stops at once with a warning.
    d.rest.fail("PUT", "/guilds/*/members/*/roles/*", 404, 10011, "Unknown Role");
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").build()).await;
    wait_released(&d.t, d.g).await;
    let fin = d.t.responses.calls().into_iter().find(|c| c.method == "followUp").expect("summary");
    assert_eq!(fin.embed_title(), Some("⚠️  Batch Stopped"));
    let text = fin.embed_description().unwrap_or_default().to_string();
    assert!(text.contains("\n⛔ The verified role was deleted."), "{text}");
    assert!(text.ends_with(" 6 more still need it: run `/captcha verify-existing` again to continue."), "{text}");

    // Five refusals in a row: lost Manage Roles.
    d.t.responses.clear();
    d.rest.fail("PUT", "/guilds/*/members/*/roles/*", 403, 50013, "Missing Permissions");
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").build()).await;
    wait_released(&d.t, d.g).await;
    let fin = d.t.responses.calls().into_iter().find(|c| c.method == "followUp").expect("summary");
    let text = fin.embed_description().unwrap_or_default().to_string();
    assert!(text.starts_with("**5/7** processed · ✅ 0 given · ⚠️ 5 failed"), "{text}");
    assert!(text.contains("⛔ I lost the Manage Roles permission, or the role is now above mine."));

    // One run per guild: a second start while a slow batch runs answers "Already Running".
    d.t.responses.clear();
    d.rest.on_with("PUT", "/guilds/*/members/*/roles/*", |_| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        (204, Value::Null)
    });
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").build()).await;
    assert!(svc::get_bulk_run(&d.t, d.g).is_some(), "running");
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").build()).await;
    let calls = d.t.responses.calls();
    assert!(calls.iter().any(|c| c.embed_title() == Some("⚠️  Already Running")), "{calls:?}");
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "verify-existing").option("stop", ot::BOOLEAN, json!(true)).build()).await;
    assert_eq!(d.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).as_deref(), Some("ℹ️  Stopping"));
    wait_released(&d.t, d.g).await;
    d.done().await;
}

#[tokio::test]
async fn db_lock_off_keeps_a_private_server_private_and_unlocks_after_confirming() {
    let Some(url) = database_url() else { return };
    use bot_core::command::option_type as ot;
    let g = "955029000000000181";
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let rest = FakeRest::new();
    let guild = world(g, &[]).everyone(P::SEND_MESSAGES);
    t.set_discord(rest.discord(&[guild.json()]));
    t.install(registry());
    t.settings().update_settings(g, SettingsPatch::new().set("captchaLockdownPrior", false)).await.expect("s");
    bot_core::router::route_interaction(&t, admin_slash(g, "lock").option("state", ot::STRING, json!("off")).build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("ℹ️  Lock-down Cleared"));
    assert!(rest.calls_to("PATCH", "/guilds/*/roles/*").is_empty());
    assert_eq!(t.settings().get_settings(g).await.expect("s").captcha_lockdown_prior, None);

    t.settings().update_settings(g, SettingsPatch::new().set("captchaLockdownPrior", true)).await.expect("s");
    rest.on("PATCH", "/guilds/*/roles/*", 200, role_json(g));
    t.responses.clear();
    let t2 = t.clone();
    let run = tokio::spawn(async move {
        bot_core::router::route_interaction(&t2, admin_slash(g, "lock").option("state", ot::STRING, json!("off")).build()).await;
    });
    for _ in 0..200 {
        if !t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = t.responses.calls()[0].clone();
    assert_eq!(prompt.embed_title(), Some("ℹ️  🔓 Unlock the server?"));
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or_default().to_string();
    bot_core::router::route_interaction(&t, FakeInteraction::button(&yes).guild(g).user(ADMIN).message_id("1000000000000000100").build()).await;
    run.await.expect("done");
    assert_eq!(t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).as_deref(), Some("✅  Server Unlocked"));
    let patch = rest.calls_to("PATCH", "/guilds/*/roles/*");
    assert_eq!(patch.len(), 1);
    assert_eq!(patch[0].body["permissions"], json!((P::SEND_MESSAGES | P::VIEW_CHANNEL).bits().to_string()));
    assert_eq!(t.settings().get_settings(g).await.expect("s").captcha_lockdown_prior, None);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_role_required_reply_explains_linked_roles_and_offers_a_usable_form() {
    let u = "955029111111100191";
    let Some(url) = database_url() else { return };
    let g = "955029000000000191";
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let rest = FakeRest::new();
    let guild = world(g, &[(u, &[])]).role_with("955029000000000199", 1, P::empty(), |r| {
        r["managed"] = json!(true);
        r["tags"] = json!({ "guild_connections": null });
    });
    t.set_discord(rest.discord(&[guild.json()]));
    t.install(registry());
    t.settings()
        .update_settings(g, SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaMode", "BUTTON").set("applicationsEnabled", true))
        .await
        .expect("s");
    t.settings().update_settings_with(g, |_| Ok(SettingsPatch::new().set("captchaRequiredRoleIds", json!(["955029000000000199"])))).await.expect("s");
    bot_core::router::route_interaction(&t, press(g, u, "captcha:start").build()).await;
    let call = t.responses.calls()[1].clone();
    let desc = call.embed_description().unwrap_or_default().to_string();
    assert!(desc.contains("**<@&955029000000000199>** is a Linked Role: connect the account"), "{desc}");
    assert!(call.body["components"].as_array().is_none_or(Vec::is_empty), "no form yet");

    // A usable fallback form: the Applications Apply button, and the require autocomplete lists it.
    use feat_welcome::application_service as apps;
    let form = apps::create_form(&t, g, "Membership", None, Some(CH_VERIFY)).await.expect("db").expect("created");
    apps::add_question(&t, g, &form.id, apps::Question { label: "Why?".into(), style: apps::QuestionStyle::Short, required: true, placeholder: None })
        .await
        .expect("db")
        .expect("added");
    let fid = form.id.clone();
    t.settings().update_settings(g, SettingsPatch::new().set("captchaFallbackFormId", fid.clone())).await.expect("s");
    t.responses.clear();
    bot_core::router::route_interaction(&t, press(g, u, "captcha:start").build()).await;
    let call = t.responses.calls()[1].clone();
    assert!(call.embed_description().unwrap_or_default().contains("No linked account? Press **Fill out the form** below; staff will review it."), "{call:?}");
    assert_eq!(call.body["components"][0]["components"][0]["custom_id"], json!(apps::apply_custom_id(&fid)));
    assert_eq!(call.body["components"][0]["components"][0]["label"], "Fill out the form");

    t.responses.clear();
    let ac = FakeInteraction::autocomplete("captcha").guild(g).user(ADMIN).roles(&[ROLE_ADMIN]).permissions(P::MANAGE_GUILD).subcommand("require").focused("form", "");
    bot_core::router::route_interaction(&t, ac.build()).await;
    let choices = t.responses.calls()[0].body["choices"].clone();
    assert_eq!(choices[0], json!({ "name": "none (no form)", "value": "none" }));
    assert_eq!(choices[1]["value"], json!(fid));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn js_reads_the_job_rust_schedules_and_the_scheduler_runs_the_js_one() {
    let u = "955029111111100211";
    let Some(d) = Db::new("955029000000000211", &[(u, &[])]).await else { return };
    d.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", ROLE_VERIFIED).set("captchaTimeoutMinutes", 30)).await;
    let m = member_json(d.g, u, &[]);
    d.t.run_hooks(GuildMemberAdd { guild_id: gid(d.g), member: m.clone(), user: m.user.clone() }).await;
    let key = svc::timeout_job_key(d.g, u);
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["job", &key]);
    assert!(js.get("error").is_none(), "{js}");
    let ours = d.t.scheduler().get_job(&key).await.expect("job");
    assert_eq!(js["type"], "captcha.timeout");
    assert_eq!(js["dedupeKey"], json!(key));
    assert_eq!(js["payload"], json!({ "userId": u }));
    assert_eq!(js["status"], "PENDING");
    assert_eq!(js["maxAttempts"], json!(ours.max_attempts));
    assert_eq!(js["runAt"], json!(bot_db::format_js_date(&ours.run_at)));

    // The JS writes a due job: the Rust scheduler claims it and runs it through the registered handler.
    let v = "955029111111100212";
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["join", d.g, ROLE_VERIFIED, v, "30"]);
    assert!(js.get("error").is_none(), "{js}");
    let js_job = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["job", &svc::timeout_job_key(d.g, v)]);
    assert_eq!(js_job["maxAttempts"], json!(ours.max_attempts), "the same defaults on both sides");
    bot_db::sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = (now() AT TIME ZONE 'UTC') - interval '1 second' WHERE "dedupeKey" = $1"#)
        .bind(svc::timeout_job_key(d.g, v))
        .execute(d.t.db())
        .await
        .expect("due");
    let seen = seam(&d.t, KickOutcome::Kicked);
    d.rest.on(
        "GET",
        &format!("/guilds/{}/members/{v}", d.g),
        200,
        json!({ "user": { "id": v, "username": "x", "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 }),
    );
    for _ in 0..200 {
        d.t.scheduler().tick(&d.t, false).await;
        if seen.lock().map(|s| !s.is_empty()).unwrap_or(false) && d.t.scheduler().get_job(&svc::timeout_job_key(d.g, v)).await.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(seen.lock().map(|s| s.iter().map(|i| i.target_id.clone()).collect::<Vec<_>>()).unwrap_or_default(), vec![v.to_string()]);
    assert!(d.row(v).await.is_none(), "kicked: the row is gone");
    assert!(d.t.scheduler().get_job(&svc::timeout_job_key(d.g, v)).await.is_none(), "the job is done");
    d.done().await;
}

/// discord.js hands `/captcha role` and `/captcha channel` the resolved option even when the cache lacks it: an
/// uncached role is judged as the resolved role and an uncached channel is fetched over REST.
#[tokio::test]
async fn db_role_and_channel_options_missing_from_the_cache_still_work() {
    const LATE_ROLE: &str = "955029000000000109";
    const LATE_CH: &str = "955029000000000209";
    let Some(d) = Db::new("955029000000000231", &[]).await else { return };
    use bot_core::command::option_type as ot;
    let route = |f: FakeInteraction| {
        d.t.responses.clear();
        bot_core::router::route_interaction(&d.t, f.build())
    };
    let last = || d.t.responses.calls().last().cloned().expect("an answer");
    route(admin_slash(d.g, "role").option("role", ot::ROLE, json!(LATE_ROLE)).resolve_roles_by_default()).await;
    assert_eq!(last().embed_title(), Some("✅  Verified Role Set"), "{:?}", last());
    assert_eq!(d.t.settings().get_settings(d.g).await.expect("s").captcha_role_id.as_deref(), Some(LATE_ROLE));

    d.rest.on(
        "GET",
        &format!("/channels/{LATE_CH}"),
        200,
        json!({ "id": LATE_CH, "type": 0, "guild_id": d.g, "name": "late", "position": 4, "permission_overwrites": [], "nsfw": false, "parent_id": null }),
    );
    route(admin_slash(d.g, "channel").option("channel", ot::CHANNEL, json!(LATE_CH))).await;
    assert_eq!(last().embed_title(), Some("✅  Panel Moved"), "{:?}", last());
    assert_eq!(d.t.settings().get_settings(d.g).await.expect("s").captcha_channel_id.as_deref(), Some(LATE_CH));
    d.done().await;
}
