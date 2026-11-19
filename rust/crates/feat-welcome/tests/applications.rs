//! Port of `test/applications.test.js`, plus the golden checks for both commands and plugins.
//!
//! Offline: the pure rules and builders against `rust/fixtures/golden/feat-welcome/golden.json` (produced by the JS
//! service), the command helpers and the hook filters. With `TEST_DATABASE_URL`: the form and question writes, the
//! draft / pending / cooldown rules, the whole apply -> review flow through the router with a recording Discord, the
//! cleanup hooks, the raw statements, and both cross-language directions.

mod common;

use bot_core::prelude::*;
use bot_core::testing::{FakeGuild, TEST_BOT_ID, TestApp};
use common::*;
use feat_welcome::application_service::{self as svc, Decision, Outcome, Question, Refusal, application as A};
use serde_json::{Value, json};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_welcome::register(&mut reg);
    reg
}

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["welcome", "application"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "welcome");
    bot_core::testing::assert_plugin_matches_golden(&reg, "applications");
}

fn form_of(v: &Value) -> bot_db::ApplicationForm {
    let mut row = json!({
        "id": "F", "guildId": GID, "name": "Staff", "description": null, "questions": [], "reviewChannelId": null, "acceptRoleIds": [],
        "requiredRoleIds": [], "cooldownHours": 24, "acceptMessage": null, "denyMessage": null, "panelChannelId": null, "panelMessageId": null,
        "enabled": true, "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z",
    });
    for (k, val) in v.as_object().into_iter().flatten() {
        row[k] = val.clone();
    }
    serde_json::from_value(row).unwrap_or_else(|e| panic!("form: {e}"))
}

fn ms(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(|e| panic!("{s}: {e}"))
}

#[test]
fn pure_rules_match_the_js() {
    let g = golden();
    let p = &g["applications"];
    for c in p["parseQuestions"].as_array().into_iter().flatten() {
        same_ordered(&svc::questions_json(&svc::parse_questions(&c["input"])), &c["out"], "parseQuestions");
    }
    for c in p["pageCount"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::page_count(c["n"].as_u64().unwrap_or(0) as usize)), c["out"]);
    }
    for c in p["answerLimit"].as_array().into_iter().flatten() {
        let style = if c["style"] == "PARAGRAPH" { svc::QuestionStyle::Paragraph } else { svc::QuestionStyle::Short };
        assert_eq!(json!(svc::answer_limit(style, c["total"].as_u64().unwrap_or(0) as usize)), c["out"], "{c}");
    }
    for c in p["clear"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::is_clear_word(c["v"].as_str().unwrap_or(""))), c["out"], "{c}");
    }
    for c in p["normalize"].as_array().into_iter().flatten() {
        let qs = svc::parse_questions(&c["qs"]);
        assert_eq!(json!(svc::normalize_answers(&qs, &c["stored"])), c["out"], "{c}");
        let values: Vec<String> = ["x", "y", "z"].iter().take(qs.len()).map(|s| s.to_string()).collect();
        same_ordered(&svc::merge_answers(&qs, &c["stored"], 0, &values), &c["merged"], "merge");
    }
    let six: Vec<Question> = (0..6).map(|n| Question { label: format!("Q{n}"), style: svc::QuestionStyle::Short, required: n != 5, placeholder: None }).collect();
    let answers: Vec<String> = ["a", "", " ", "d", "e", ""].iter().map(|s| s.to_string()).collect();
    assert_eq!(svc::missing_required(&six, &answers), vec![1, 2]);
    assert_eq!(svc::clean_answer(Some("  hello  "), 3), "hel");
    assert_eq!(svc::clean_answer(None, 10), "");
    assert_eq!(bot_config::limits::APPLICATION_QUESTIONS, 3 * A::PAGE_SIZE);
    for c in p["judge"].as_array().into_iter().flatten() {
        let form = form_of(&c["form"]);
        let roles: Vec<String> = c["facts"]["memberRoleIds"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
        let facts = svc::ApplyFacts {
            form: &form,
            member_role_ids: &roles,
            review_channel_problem: c["facts"]["reviewChannelProblem"].as_str().map(String::from),
            pending: c["facts"]["pending"].as_bool().unwrap_or(false),
            last_denied_at: c["facts"]["lastDenied"]["reviewedAt"].as_str().map(ms),
            now: ms(c["now"].as_str().unwrap_or("")),
        };
        let got = match svc::judge_apply(&facts) {
            Ok(()) => json!({ "ok": true }),
            Err(Refusal::ReviewChannel { reason }) => json!({ "ok": false, "code": "REVIEW_CHANNEL", "data": { "reason": reason } }),
            Err(Refusal::MissingRole { role_ids }) => json!({ "ok": false, "code": "MISSING_ROLE", "data": { "roleIds": role_ids } }),
            Err(Refusal::Cooldown { until }) => json!({ "ok": false, "code": "COOLDOWN", "data": { "until": bot_db::format_js_date(&until) } }),
            Err(r) => json!({ "ok": false, "code": r.code() }),
        };
        same(&got, &c["out"], &format!("judge {c}"));
    }
    let k = &p["constants"];
    assert_eq!(json!(A::NAME_MAX), k["NAME_MAX"]);
    assert_eq!(json!(A::ANSWER_BUDGET), k["ANSWER_BUDGET"]);
    assert_eq!(json!(A::NO_ANSWER), k["NO_ANSWER"]);
    assert_eq!(json!(svc::DEFAULT_ACCEPT_MESSAGE), k["DEFAULT_ACCEPT_MESSAGE"]);
    assert_eq!(json!(svc::DEFAULT_DENY_MESSAGE), k["DEFAULT_DENY_MESSAGE"]);
}

/// discord.js writes a unicode button emoji as `{ name, animated: false }`, serenity as `{ name }`; Discord reads
/// both the same. Everything else must be identical.
fn strip_animated(mut v: Value) -> Value {
    if let Some(rows) = v.get_mut("components").and_then(Value::as_array_mut) {
        for c in rows.iter_mut() {
            if c.get("disabled") == Some(&json!(false))
                && let Some(o) = c.as_object_mut()
            {
                o.shift_remove("disabled"); // serenity always writes Discord's default
            }
            if let Some(e) = c.get_mut("emoji").and_then(Value::as_object_mut) {
                e.shift_remove("animated");
                e.shift_remove("id");
            }
        }
    }
    v
}

#[test]
fn builders_match_the_js() {
    let g = golden();
    let p = &g["applications"];
    let qs = svc::parse_questions(&p["questions15"]);
    for c in p["applyModals"].as_array().into_iter().flatten() {
        let questions = if c["two"].as_bool() == Some(true) { qs[..2].to_vec() } else { qs.clone() };
        let got = svc::apply_modal_json(c["name"].as_str().unwrap_or(""), &questions, "SUB1", &p["stored"], c["page"].as_u64().unwrap_or(0) as usize);
        same(&got, &c["out"], &format!("apply modal page {}", c["page"]));
    }
    same(&svc::deny_modal_json("SUB1"), &p["denyModal"], "deny modal");
    for c in p["continueRows"].as_array().into_iter().flatten() {
        let row = svc::continue_row("SUB1", c["page"].as_u64().unwrap_or(0) as usize, c["total"].as_u64().unwrap_or(0) as usize, c["label"].as_str());
        same(&strip_animated(serde_json::to_value(&row).unwrap_or(Value::Null)), &strip_animated(c["out"].clone()), "continue row");
    }
    same(&strip_animated(serde_json::to_value(svc::review_row("SUB1")).unwrap_or(Value::Null)), &strip_animated(p["reviewRow"].clone()), "review row");
    assert_eq!(svc::apply_custom_id("FORM1"), p["applyCustomId"].as_str().unwrap_or(""));
    let button = json!({ "components": [serde_json::to_value(svc::build_apply_button("FORM1", Some("Fill out the form"))).unwrap_or(Value::Null)] });
    same(&strip_animated(button), &strip_animated(json!({ "components": [p["applyButton"].clone()] })), "apply button");

    let guild = test_guild("Test Guild");
    let ctx = bot_util::placeholder_parser::RenderContext { guild: Some(bot_core::adapters::placeholder_guild(&guild)), ..Default::default() };
    for c in p["panels"].as_array().into_iter().flatten() {
        let f = &c["form"];
        let form = form_of(&json!({ "id": f["id"], "name": f["name"], "description": f["description"], "questions": f["questions"] }));
        let got = payload_json(&svc::build_panel_payload(&form, &ctx));
        same_clean(&got["embeds"], &c["out"]["embeds"], "panel embed");
        let rows: Vec<Value> = got["components"].as_array().into_iter().flatten().map(|r| strip_animated(r.clone())).collect();
        let js: Vec<Value> = c["out"]["components"].as_array().into_iter().flatten().map(|r| strip_animated(r.clone())).collect();
        same(&json!(rows), &json!(js), "panel row");
        assert_eq!(got["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    }

    // Review and outcome embeds (the JS fake's user: id UID, `user\`one`, the default avatar).
    let who = svc::Applicant {
        id: UID.into(),
        username: "user`one".into(),
        created_ms: bot_core::adapters::snowflake_ms(UID.parse().unwrap_or(1)),
        joined_ms: Some(1_690_000_000_000.0),
        avatar_url: Some("https://cdn.discordapp.com/embed/avatars/1.png".into()),
    };
    let answers = json!((0..15).map(|n| json!({ "question": format!("Q{} {}", n + 1, "q".repeat(40)), "answer": "a".repeat(1000) })).collect::<Vec<_>>());
    let at = ms(p["review"]["submittedAt"].as_str().unwrap_or(""));
    let review = svc::build_review_embed("Staff", &answers, &who, "SUB1", at);
    let review_json = serde_json::to_value(&review).unwrap_or(Value::Null);
    let ts = review_json["timestamp"].as_str().map(ms).expect("timestamp");
    assert_eq!(ts, ms(p["review"]["timestamp"].as_str().unwrap_or("")), "the review timestamp is the submit time");
    same_clean(&embed_json(&review), &p["review"]["out"], "review embed");
    let empty = svc::build_review_embed("Staff", &json!([{ "question": "Q", "answer": "  " }]), &svc::Applicant { joined_ms: None, ..who.clone() }, "S", at);
    same_clean(&embed_json(&empty), &p["reviewEmpty"], "empty review");

    let mut source_json = review_json.clone();
    source_json["type"] = json!("rich");
    let source: serenity::all::Embed = serde_json::from_value(source_json).expect("embed");
    for c in p["outcomes"].as_array().into_iter().flatten() {
        let o = Outcome {
            form_name: Some("Staff"),
            decision: if c["status"] == "ACCEPTED" { Decision::Accepted } else { Decision::Denied },
            reviewer_id: c["reviewerId"].as_str().unwrap_or(""),
            note: c["note"].as_str(),
            extra: c["extra"].as_str(),
            submission_id: "SUB1",
            at: ms(c["at"].as_str().unwrap_or("")),
        };
        let e = svc::build_outcome_embed(if c["source"].as_bool() == Some(true) { Some(&source) } else { None }, &o);
        same_clean(&embed_json(&e), &c["out"], &format!("outcome {c}"));
    }
    for c in p["refusals"].as_array().into_iter().flatten() {
        let d = &c["data"];
        let r = match c["code"].as_str().unwrap_or("") {
            "MODULE_OFF" => Refusal::ModuleOff,
            "FORM_NOT_FOUND" => Refusal::FormNotFound,
            "FORM_DISABLED" => Refusal::FormDisabled,
            "NOT_SET_UP" => Refusal::NotSetUp,
            "REVIEW_CHANNEL" => Refusal::ReviewChannel { reason: d["reason"].as_str().unwrap_or("").into() },
            "MISSING_ROLE" => Refusal::MissingRole { role_ids: d["roleIds"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect() },
            "PENDING" => Refusal::Pending,
            "COOLDOWN" => Refusal::Cooldown { until: ms(d["until"].as_str().unwrap_or("")) },
            "SLOW" => Refusal::Slow,
            "SERVER_ONLY" => Refusal::ServerOnly,
            _ => Refusal::Error,
        };
        same_clean(&embed_json(&svc::refusal_embed(&r)), &c["out"], &format!("refusal {}", c["code"]));
    }
}

#[test]
fn command_helpers() {
    use feat_welcome::commands::application::{js_parse_int, question_choices, question_lines};
    assert_eq!(js_parse_int("2"), Some(2));
    assert_eq!(js_parse_int(" 12abc"), Some(12));
    assert_eq!(js_parse_int("abc"), None);
    assert_eq!(js_parse_int("-1"), Some(-1));
    let qs = svc::parse_questions(&json!([{ "label": "Why us?", "style": "PARAGRAPH" }, { "label": "Age", "required": false }]));
    assert_eq!(question_lines(&qs), "**1.** Why us? ¶\n**2.** Age *(optional)*");
    assert_eq!(question_lines(&[]), "*No questions yet. Add one with `/application question add`.*");
    let c = question_choices(&qs, "2");
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].name.as_deref(), Some("2. Age"));
    assert_eq!(c[0].value, json!("2"));
}

#[tokio::test]
async fn hook_filters_bot_and_uncached_messages_only() {
    use feat_welcome::plugins::applications::is_own_or_unknown;
    let t = TestApp::new().with_guilds([GID]).with_fake_guilds(&[FakeGuild::new(GID, OWNER).member(TEST_BOT_ID, &[])]);
    let del = |author: Option<&str>| bot_core::hooks::events::MessageDelete {
        guild_id: gid(),
        message_id: serenity::all::MessageId::new(5),
        channel_id: serenity::all::ChannelId::new(6),
        message: author.map(|a| bot_core::testing::fake_message("5", GID, "6", a, "x")),
        partial: author.is_none(),
        channel: None,
    };
    assert!(is_own_or_unknown(&t, &del(None)));
    assert!(is_own_or_unknown(&t, &del(Some(TEST_BOT_ID))));
    assert!(!is_own_or_unknown(&t, &del(Some(UID))), "member chat never costs a query");
}

// ═════════════════════════════════════════════════════════════════════════════
// DB cases (TEST_DATABASE_URL and TEST_REDIS_URL)
// ═════════════════════════════════════════════════════════════════════════════

use bot_core::testing::{DbTestApp, FakeInteraction, database_url, offline_discord_as, redis_url, run_node};
use serenity::all::Permissions as P;

const RC: &str = "955008000000000611";
const PNL: &str = "955008000000000612";
const R1: &str = "955008000000000621";
const HIGH: &str = "955008000000000622";
const VIP: &str = "955008000000000623";
const MOD: &str = "955008000000000624";
const ADMIN_R: &str = "955008000000000625";
const A1: &str = "955008000000000631";
const A2: &str = "955008000000000632";
const A3: &str = "955008000000000633";
const M1: &str = "955008000000000641";
const M2: &str = "955008000000000642";
const ADMIN: &str = "955008000000000651";
const X1: &str = "955008000000000661";

fn app_guild(id: &str) -> FakeGuild {
    FakeGuild::new(id, OWNER)
        .role(BOT_ROLE, 10, P::MANAGE_ROLES | P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role(R1, 3, P::empty())
        .role(HIGH, 50, P::empty())
        .role(VIP, 3, P::empty())
        .role(MOD, 4, P::empty())
        .role(ADMIN_R, 9, P::MANAGE_GUILD)
        .member(A1, &[])
        .member(A2, &[])
        .member(A3, &[VIP])
        .member(M1, &[MOD])
        .member(M2, &[MOD])
        .member(ADMIN, &[ADMIN_R])
        .member(X1, &[])
        .channel(RC, 0, "review", &[])
        .channel(PNL, 0, "panel", &[])
}

struct Apps {
    t: DbTestApp,
    io: std::sync::Arc<FakeIo>,
    g: &'static str,
}

impl Apps {
    async fn new(g: &'static str, enabled: bool) -> Option<Apps> {
        let db = database_url()?;
        let t = match redis_url() {
            Some(r) => DbTestApp::with_redis(&db, &r).await,
            None => DbTestApp::new(&db).await,
        }
        .with_guilds([g]);
        t.set_discord(offline_discord_as(&[app_guild(g).json()]));
        t.reset_guilds(&[g]).await;
        t.settings().update_settings(g, bot_core::SettingsPatch::new().set("applicationsEnabled", enabled).set("modRoleIds", json!([MOD]))).await.expect("settings");
        t.install(registry());
        let io = FakeIo::install(&t);
        for m in [A1, A2, A3, M1, M2, X1] {
            io.set_member(member_of_in(g, m, if m == A3 { &[VIP] } else { &[] }));
        }
        Some(Apps { t, io, g })
    }

    /// A form with `count` questions and a review channel.
    async fn form(&self, count: usize, extra: Value) -> bot_db::ApplicationForm {
        let form = svc::create_form(&self.t, self.g, "Staff", None, Some(RC)).await.expect("db").expect("created");
        for n in 1..=count {
            let q = Question { label: format!("Question {n}"), style: if n % 2 == 1 { svc::QuestionStyle::Short } else { svc::QuestionStyle::Paragraph }, required: true, placeholder: None };
            svc::add_question(&self.t, self.g, &form.id, q).await.expect("db").expect("added");
        }
        if let Some(o) = extra.as_object().filter(|o| !o.is_empty()) {
            svc::update_form(&self.t, self.g, &form.id, o.clone()).await.expect("db").expect("updated");
        }
        svc::find_form(self.t.db(), self.g, &form.id).await.expect("db").expect("form")
    }

    async fn sub_of(&self, form: &str, user: &str) -> Option<bot_db::ApplicationSubmission> {
        bot_db::sqlx::query_as(r#"SELECT * FROM "ApplicationSubmission" WHERE "formId" = $1 AND "userId" = $2 ORDER BY "createdAt" DESC LIMIT 1"#)
            .bind(form)
            .bind(user)
            .fetch_optional(self.t.db())
            .await
            .expect("db")
    }

    async fn route(&self, i: serenity::all::Interaction) -> Vec<bot_core::interaction::RecordedCall> {
        self.t.responses.clear();
        bot_core::router::route_interaction(&self.t, i).await;
        self.t.responses.calls()
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn member_of_in(g: &str, id: &str, roles: &[&str]) -> serenity::all::Member {
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": false },
        "roles": roles, "joined_at": "2023-07-22T04:26:40.000000+00:00", "deaf": false, "mute": false, "flags": 0, "pending": false, "guild_id": g,
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

fn button(g: &str, user: &str, roles: &[&str], custom_id: &str) -> FakeInteraction {
    FakeInteraction::button(custom_id).guild(g).user(user).roles(roles)
}

/// A modal submit; `from_message` attaches the message it was opened from.
fn modal_submit(g: &str, user: &str, roles: &[&str], custom_id: &str, fields: &[(&str, &str)], from_message: Option<Value>) -> serenity::all::Interaction {
    let mut f = FakeInteraction::modal(custom_id, fields).guild(g).user(user).roles(roles);
    if let Some(m) = from_message {
        f.json_mut()["message"] = m;
    }
    f.build()
}

fn message_with(embed: Option<Value>) -> Value {
    let mut b = FakeInteraction::button("noop");
    let mut m = b.json_mut()["message"].clone();
    if let Some(e) = embed {
        m["embeds"] = json!([e]);
    }
    m
}

/// The fields of the modal a showModal call opened, filled with `answers`.
fn fill(modal: &Value, answers: &[&str]) -> Vec<(String, String)> {
    modal["components"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(j, row)| (row["components"][0]["custom_id"].as_str().unwrap_or("").to_string(), answers.get(j).copied().unwrap_or("").to_string()))
        .collect()
}

fn modal_of(calls: &[bot_core::interaction::RecordedCall]) -> Value {
    calls.iter().find(|c| c.method == "showModal").map(|c| c.body.clone()).unwrap_or(Value::Null)
}

fn row_ids(body: &Value) -> Vec<String> {
    body["components"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["components"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["custom_id"].as_str().map(String::from))
        .collect()
}

fn refs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
}

#[tokio::test]
async fn db_create_form_names_are_unique_ignoring_case_and_capped() {
    let Some(d) = Apps::new("955008000000000601", true).await else { return };
    assert!(svc::create_form(&d.t, d.g, "Staff", None, None).await.expect("db").is_ok());
    assert_eq!(svc::create_form(&d.t, d.g, "STAFF", None, None).await.expect("db").err(), Some(Refusal::NameTaken));
    for n in 1..bot_config::limits::APPLICATION_FORMS {
        assert!(svc::create_form(&d.t, d.g, &format!("Form {n}"), None, None).await.expect("db").is_ok());
    }
    assert_eq!(svc::create_form(&d.t, d.g, "One too many", None, None).await.expect("db").err(), Some(Refusal::FormLimit));
    // find, update, questions, roles.
    let forms = svc::list_forms(d.t.db(), d.g).await.expect("list");
    let staff = &forms[0].form;
    assert_eq!(svc::find_form(d.t.db(), d.g, "sTaFf").await.expect("db").map(|f| f.id), Some(staff.id.clone()));
    assert_eq!(svc::find_form(d.t.db(), "955008000000000699", &staff.id).await.expect("db"), None, "never another guild");
    assert_eq!(svc::find_form(d.t.db(), d.g, "").await.expect("db"), None);
    let other = &forms[1].form;
    assert_eq!(svc::update_form(&d.t, d.g, &other.id, json!({ "name": "staff" }).as_object().cloned().unwrap_or_default()).await.expect("db").err(), Some(Refusal::NameTaken));
    let renamed = svc::update_form(&d.t, d.g, &staff.id, json!({ "name": "STAFF", "cooldownHours": 0 }).as_object().cloned().unwrap_or_default()).await.expect("db").expect("ok");
    assert_eq!((renamed.name.as_str(), renamed.cooldown_hours), ("STAFF", 0));
    assert_eq!(svc::update_form(&d.t, "955008000000000699", &staff.id, json!({ "cooldownHours": 1 }).as_object().cloned().unwrap_or_default()).await.expect("db").err(), Some(Refusal::FormNotFound));
    for n in 1..=bot_config::limits::APPLICATION_QUESTIONS {
        let q = Question { label: format!("Q{n}"), style: svc::QuestionStyle::Short, required: true, placeholder: None };
        assert!(svc::add_question(&d.t, d.g, &staff.id, q).await.expect("db").is_ok());
    }
    let more = Question { label: "More".into(), style: svc::QuestionStyle::Short, required: true, placeholder: None };
    assert_eq!(svc::add_question(&d.t, d.g, &staff.id, more).await.expect("db").err(), Some(Refusal::QuestionLimit));
    assert_eq!(svc::remove_question(&d.t, d.g, &staff.id, Some(16)).await.expect("db").err(), Some(Refusal::QuestionNotFound));
    assert_eq!(svc::remove_question(&d.t, d.g, &staff.id, Some(0)).await.expect("db").err(), Some(Refusal::QuestionNotFound));
    let (form, removed) = svc::remove_question(&d.t, d.g, &staff.id, Some(2)).await.expect("db").expect("removed");
    assert_eq!(removed.label, "Q2");
    assert_eq!(svc::parse_questions(&form.questions)[1].label, "Q3");
    let dup = Question { label: "q3".into(), style: svc::QuestionStyle::Short, required: true, placeholder: None };
    assert_eq!(svc::add_question(&d.t, d.g, &staff.id, dup.clone()).await.expect("db").err(), Some(Refusal::QuestionDup));
    assert_eq!(svc::add_question(&d.t, "955008000000000699", &staff.id, dup).await.expect("db").err(), Some(Refusal::FormNotFound));
    let oid = other.id.clone();
    let ch = |list, role: String, remove| {
        let (t, g, id) = (&d.t, d.g, oid.clone());
        async move { svc::change_role(t, g, &id, list, &role, remove).await }
    };
    assert!(ch(svc::RoleList::Accept, "A".into(), false).await.expect("db").expect("ok").1);
    assert!(!ch(svc::RoleList::Accept, "A".into(), false).await.expect("db").expect("ok").1);
    assert!(!ch(svc::RoleList::Required, "A".into(), true).await.expect("db").expect("ok").1);
    assert!(ch(svc::RoleList::Accept, "A".into(), true).await.expect("db").expect("ok").0.accept_role_ids.is_empty());
    for n in 0..bot_config::limits::APPLICATION_ROLES {
        ch(svc::RoleList::Required, format!("R{n}"), false).await.expect("db").expect("ok");
    }
    assert_eq!(ch(svc::RoleList::Required, "EXTRA".into(), false).await.expect("db").err(), Some(Refusal::RoleLimit));
    d.done().await;
}

async fn insert_sub(d: &Apps, form: &str, user: &str, status: &str, review: Option<&str>, reviewed_ago_ms: Option<i64>) -> String {
    let reviewed = reviewed_ago_ms.map(|a| (chrono::Utc::now() - chrono::Duration::milliseconds(a)).naive_utc());
    let id = bot_db::new_cuid();
    bot_db::sqlx::query(
        r#"INSERT INTO "ApplicationSubmission" ("id","guildId","formId","userId","status","reviewMessageId","reviewedAt","updatedAt") VALUES ($1,$2,$3,$4,$5::"ApplicationStatus",$6,$7,(now() AT TIME ZONE 'UTC'))"#,
    )
    .bind(&id)
    .bind(d.g)
    .bind(form)
    .bind(user)
    .bind(status)
    .bind(review)
    .bind(reviewed)
    .execute(d.t.db())
    .await
    .expect("insert");
    id
}

#[tokio::test]
async fn db_drafts_pending_orphans_and_the_cooldown() {
    let Some(d) = Apps::new("955008000000000602", true).await else { return };
    let form = d.form(2, json!({})).await;
    let now = chrono::Utc::now();
    let (a, b) = tokio::join!(svc::start_draft(&d.t, &form, "U1", &[], None, now), svc::start_draft(&d.t, &form, "U1", &[], None, now));
    let (a, b) = (a.expect("db").expect("ok"), b.expect("db").expect("ok"));
    assert_eq!(a.id, b.id, "one draft per member and form, even for simultaneous clicks");
    let other = svc::start_draft(&d.t, &form, "U2", &[], None, now).await.expect("db").expect("ok");
    assert_ne!(other.id, a.id);

    let pending = insert_sub(&d, &form.id, "U3", "PENDING", Some("MSG"), None).await;
    assert_eq!(svc::start_draft(&d.t, &form, "U3", &[], None, now).await.expect("db").err(), Some(Refusal::Pending));
    bot_db::sqlx::query(r#"UPDATE "ApplicationSubmission" SET "reviewMessageId" = NULL WHERE "id" = $1"#).bind(&pending).execute(d.t.db()).await.expect("orphan");
    assert!(svc::start_draft(&d.t, &form, "U3", &[], None, now).await.expect("db").is_ok(), "an orphaned pending row does not lock the member out");

    insert_sub(&d, &form.id, "U4", "DENIED", None, Some(2 * 3_600_000)).await;
    match svc::start_draft(&d.t, &form, "U4", &[], None, now).await.expect("db") {
        Err(Refusal::Cooldown { until }) => assert!(until > now),
        other => panic!("expected a cooldown, got {other:?}"),
    }
    let mut short = form.clone();
    short.cooldown_hours = 1;
    assert!(svc::start_draft(&d.t, &short, "U4", &[], None, now).await.expect("db").is_ok());

    let before: i64 = bot_db::sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ApplicationSubmission" WHERE "formId" = $1"#).bind(&form.id).fetch_one(d.t.db()).await.expect("n");
    let mut closed = form.clone();
    closed.enabled = false;
    assert_eq!(svc::start_draft(&d.t, &closed, "U9", &[], None, now).await.expect("db").err(), Some(Refusal::FormDisabled));
    let mut gated = form.clone();
    gated.required_role_ids = vec!["VIP".into()];
    assert!(matches!(svc::start_draft(&d.t, &gated, "U9", &["x".into()], None, now).await.expect("db"), Err(Refusal::MissingRole { .. })));
    let after: i64 = bot_db::sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ApplicationSubmission" WHERE "formId" = $1"#).bind(&form.id).fetch_one(d.t.db()).await.expect("n");
    assert_eq!(before, after, "no draft rows for refused members");

    // listForms and pendingCount count only pending submissions with a review post.
    for s in ["PENDING", "PENDING"] {
        insert_sub(&d, &form.id, &bot_db::new_cuid(), s, Some(&bot_db::new_cuid()), None).await;
    }
    insert_sub(&d, &form.id, "U7", "PENDING", None, None).await;
    let list = svc::list_forms(d.t.db(), d.g).await.expect("list");
    assert_eq!(list[0].pending, 2);
    assert_eq!(svc::pending_count(d.t.db(), &form.id).await.expect("count"), 2);
    d.done().await;
}

#[tokio::test]
async fn db_apply_is_refused_without_a_modal_when_the_module_is_off_the_form_is_unknown_or_the_member_ineligible() {
    let Some(d) = Apps::new("955008000000000603", false).await else { return };
    let form = d.form(7, json!({ "requiredRoleIds": [VIP] })).await;
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    // The component gate answers first while the module is off (the JS foundation wrapper does the same).
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].embed_title().unwrap_or_default().contains("Module Disabled") || calls[0].content().unwrap_or_default().contains("turned off"), "{calls:?}");
    assert!(calls[0].ephemeral());
    // /application apply is gated = false: the service refuses itself.
    let apply = FakeInteraction::slash("application").guild(d.g).user(A1).subcommand("apply").option("form", 3, json!(form.id)).build();
    let calls = d.route(apply).await;
    assert_eq!(calls[0].method, "reply");
    assert_eq!(calls[0].embed_title(), Some("⚠️  Module Disabled"));
    assert_eq!(calls[0].embed_description(), Some("The **Applications** module is turned off on this server. An admin can enable it with `/module enable applications`."));

    d.t.settings().update_settings(d.g, bot_core::SettingsPatch::new().set("applicationsEnabled", true)).await.expect("on");
    let calls = d.route(button(d.g, A1, &[], "application:apply:nope").build()).await;
    assert_eq!(calls[0].embed_title(), Some("❌  Application Not Found"));
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    assert!(calls[0].embed_description().unwrap_or_default().contains(&format!("<@&{VIP}>")));
    let apply = FakeInteraction::slash("application").guild(d.g).user(A3).roles(&[VIP]).subcommand("apply").option("form", 3, json!("staff")).build();
    let calls = d.route(apply).await;
    assert_eq!(calls[0].method, "showModal", "a member with the role gets the form, found by name");
    d.done().await;
}

#[tokio::test]
async fn db_apply_then_review_two_pages_then_accept_gives_roles_and_messages_the_member() {
    let Some(d) = Apps::new("955008000000000604", true).await else { return };
    let form = d.form(7, json!({})).await;
    svc::change_role(&d.t, d.g, &form.id, svc::RoleList::Accept, R1, false).await.expect("db").expect("ok");
    svc::change_role(&d.t, d.g, &form.id, svc::RoleList::Accept, HIGH, false).await.expect("db").expect("ok");

    // Page 1 (opened from the public panel) is answered with a new ephemeral reply.
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    let m1 = modal_of(&calls);
    let draft = d.sub_of(&form.id, A1).await.expect("draft");
    assert_eq!(m1["custom_id"], format!("application-modal:page:{}:0", draft.id));
    assert_eq!(m1["components"].as_array().map(Vec::len), Some(5));
    let f1 = fill(&m1, &["answer 1", "answer 2", "answer 3", "answer 4", "answer 5"]);
    let calls = d.route(modal_submit(d.g, A1, &[], m1["custom_id"].as_str().unwrap_or(""), &refs(&f1), Some(message_with(None)))).await;
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"], "page 1 never updates the public panel");
    assert!(calls[0].ephemeral());
    assert_eq!(row_ids(&calls[1].body), vec![format!("application:next:{}:1", draft.id)]);
    let saved = d.sub_of(&form.id, A1).await.expect("draft");
    assert_eq!(saved.answers.as_array().map(Vec::len), Some(7), "answers are kept in question order");

    let calls = d.route(button(d.g, A1, &[], &format!("application:next:{}:1", draft.id)).build()).await;
    let m2 = modal_of(&calls);
    assert_eq!(m2["components"].as_array().map(Vec::len), Some(2));
    assert!(m2["title"].as_str().unwrap_or("").ends_with("(2/2)"));
    let calls = d.route(button(d.g, A2, &[], &format!("application:next:{}:1", draft.id)).build()).await;
    assert_eq!(calls[0].method, "reply", "nobody can open someone else's draft");

    let f2 = fill(&m2, &["six", "seven"]);
    let calls = d.route(modal_submit(d.g, A1, &[], m2["custom_id"].as_str().unwrap_or(""), &refs(&f2), Some(message_with(None)))).await;
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferUpdate", "editReply"], "later pages replace the Continue message");
    assert_eq!(calls[1].embed_title(), Some("✅  Application Submitted"));
    assert_eq!(calls[1].body["components"], json!([]));
    let sent = d.io.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, RC);
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    let ids: Vec<String> = sent[0].1["components"][0]["components"].as_array().into_iter().flatten().filter_map(|c| c["custom_id"].as_str().map(String::from)).collect();
    assert_eq!(ids, vec![format!("application:accept:{}", draft.id), format!("application:deny:{}", draft.id)]);
    let post = sent[0].1["embeds"][0].clone();
    assert_eq!(post["fields"].as_array().map(Vec::len), Some(7));
    assert_eq!(post["fields"][6]["value"], "seven");
    let pending = d.sub_of(&form.id, A1).await.expect("pending");
    assert_eq!(pending.status.as_str(), "PENDING");
    assert!(pending.review_message_id.is_some());

    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    assert!(calls[0].embed_description().unwrap_or_default().contains("waiting for review"));
    // A double submit cannot post twice.
    let calls = d.route(modal_submit(d.g, A1, &[], m2["custom_id"].as_str().unwrap_or(""), &refs(&f2), Some(message_with(None)))).await;
    assert!(calls.last().and_then(|c| c.embed_description()).unwrap_or_default().contains("no longer open or was already submitted"));
    assert_eq!(d.io.sent().len(), 1);

    // Review: members are refused, moderators decide.
    let mut post_embed = post.clone();
    post_embed["timestamp"] = json!("2026-10-20T12:00:00.000Z");
    let on_post = |user: &str, roles: &[&str], action: &str| {
        let mut b = button(d.g, user, roles, &format!("application:{action}:{}", draft.id));
        b.json_mut()["message"]["embeds"] = json!([post_embed.clone()]);
        b.build()
    };
    let calls = d.route(on_post(X1, &[], "accept")).await;
    assert_eq!(calls[0].embed_title(), Some("❌  Insufficient Permissions"));
    assert_eq!(d.sub_of(&form.id, A1).await.map(|s| s.status.as_str().to_string()).as_deref(), Some("PENDING"));

    let calls = d.route(on_post(M1, &[MOD], "accept")).await;
    assert_eq!(calls[0].method, "deferUpdate");
    assert_eq!(d.io.roles().iter().map(|r| r.1.as_str()).collect::<Vec<_>>(), vec![R1], "only the assignable role was given");
    let fin = d.sub_of(&form.id, A1).await.expect("accepted");
    assert_eq!((fin.status.as_str(), fin.reviewer_id.as_deref()), ("ACCEPTED", Some(M1)));
    assert!(fin.reviewed_at.is_some());
    let edit = calls.iter().find(|c| c.method == "editReply").expect("edited");
    assert_eq!(edit.body["components"], json!([]));
    let outcome = edit.body["embeds"][0]["fields"].as_array().and_then(|f| f.last()).cloned().unwrap_or(Value::Null);
    let value = outcome["value"].as_str().unwrap_or("");
    assert!(value.contains(&format!("Accepted** by <@{M1}>")), "{value}");
    assert!(value.contains(&format!("Roles given: <@&{R1}>")), "{value}");
    assert!(value.contains(&format!("Could not give <@&{HIGH}>: That role is above my highest role")), "{value}");
    let dms = d.io.dms();
    assert_eq!(dms.len(), 1);
    let dm = dms[0].1["embeds"][0]["description"].as_str().unwrap_or("");
    assert!(dm.contains("Your application for **Staff** in **Test Guild** was accepted"), "{dm}");
    let follow = calls.iter().find(|c| c.method == "followUp").expect("problems");
    assert!(follow.ephemeral());
    assert!(follow.embed_description().unwrap_or_default().contains(&format!("Could not give <@&{HIGH}>")));

    let calls = d.route(on_post(M2, &[MOD], "accept")).await;
    assert!(calls.iter().any(|c| c.embed_description().unwrap_or_default().contains(&format!("already accepted by <@{M1}>"))));
    assert_eq!(d.io.roles().len(), 1, "no second role grant");
    assert_eq!(d.io.dms().len(), 1, "no second DM");
    d.done().await;
}

#[tokio::test]
async fn db_deny_asks_for_a_reason_dms_it_and_starts_the_cooldown() {
    let Some(d) = Apps::new("955008000000000605", true).await else { return };
    let form = d.form(1, json!({ "denyMessage": "Sorry {user.name}, not this time." })).await;
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    let m = modal_of(&calls);
    let calls = d.route(modal_submit(d.g, A1, &[], m["custom_id"].as_str().unwrap_or(""), &refs(&fill(&m, &["my answer"])), None)).await;
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferReply", "editReply"], "a one-page form is acknowledged with a reply");
    let id = d.sub_of(&form.id, A1).await.expect("sub").id;
    let mut post = d.io.sent()[0].1["embeds"][0].clone();
    post["timestamp"] = json!("2026-10-20T12:00:00.000Z");

    let mut click = button(d.g, M1, &[MOD], &format!("application:deny:{id}"));
    click.json_mut()["message"]["embeds"] = json!([post.clone()]);
    let calls = d.route(click.build()).await;
    assert_eq!(calls[0].method, "showModal");
    let dm = &calls[0].body;
    assert_eq!(dm["custom_id"], format!("application-modal:deny:{id}"));
    assert_eq!(dm["components"][0]["components"][0]["custom_id"], "application-note");
    assert_eq!(dm["components"][0]["components"][0]["required"], false);

    let mut msg = message_with(Some(post));
    msg["embeds"][0]["type"] = json!("rich");
    let calls = d.route(modal_submit(d.g, X1, &[], &format!("application-modal:deny:{id}"), &[("application-note", "x")], Some(msg.clone()))).await;
    assert_eq!(calls[0].embed_title(), Some("❌  Insufficient Permissions"), "modal submits skip the router's level check");
    let calls = d.route(modal_submit(d.g, M1, &[MOD], &format!("application-modal:deny:{id}"), &[("application-note", "  Too new to the server  ")], Some(msg))).await;
    let sub = d.sub_of(&form.id, A1).await.expect("sub");
    assert_eq!((sub.status.as_str(), sub.review_note.as_deref()), ("DENIED", Some("Too new to the server")));
    let text = d.io.dms()[0].1["embeds"][0]["description"].as_str().unwrap_or("").to_string();
    assert!(text.contains(&format!("Sorry user{A1}, not this time.")), "{text}");
    assert!(text.contains("**Reason:** Too new to the server"), "{text}");
    let edit = calls.iter().find(|c| c.method == "editReply").expect("edited");
    let value = edit.body["embeds"][0]["fields"].as_array().and_then(|f| f.last()).map(|f| f["value"].as_str().unwrap_or("").to_string()).unwrap_or_default();
    assert!(value.contains(&format!("Denied** by <@{M1}>")) && value.contains("**Reason:** Too new to the server"), "{value}");

    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    let desc = calls[0].embed_description().unwrap_or_default().to_string();
    assert!(desc.starts_with("Your last application was denied. You can apply again <t:") && desc.ends_with(":R>."), "{desc}");
    svc::update_form(&d.t, d.g, &form.id, json!({ "cooldownHours": 0 }).as_object().cloned().unwrap_or_default()).await.expect("db").expect("ok");
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    assert_eq!(calls[0].method, "showModal", "with no cooldown the member can apply again");
    d.done().await;
}

async fn apply_submit(d: &Apps, form: &str, user: &str, roles: &[&str]) -> String {
    let calls = d.route(button(d.g, user, roles, &format!("application:apply:{form}")).build()).await;
    let m = modal_of(&calls);
    d.route(modal_submit(d.g, user, roles, m["custom_id"].as_str().unwrap_or(""), &refs(&fill(&m, &["hi"])), None)).await;
    d.sub_of(form, user).await.expect("sub").id
}

#[tokio::test]
async fn db_nobody_reviews_their_own_application_and_missing_applicants_and_closed_dms_are_reported() {
    let Some(d) = Apps::new("955008000000000606", true).await else { return };
    let form = d.form(1, json!({})).await;
    svc::change_role(&d.t, d.g, &form.id, svc::RoleList::Accept, R1, false).await.expect("db").expect("ok");
    let own = apply_submit(&d, &form.id, M1, &[MOD]).await;
    let calls = d.route(button(d.g, M1, &[MOD], &format!("application:accept:{own}")).build()).await;
    assert!(calls.iter().any(|c| c.embed_description().unwrap_or_default().contains("cannot review your own application")));
    assert_eq!(d.sub_of(&form.id, M1).await.map(|s| s.status.as_str().to_string()).as_deref(), Some("PENDING"));

    let shy = apply_submit(&d, &form.id, A2, &[]).await;
    d.io.fail_dms(&[Some(50007)]);
    let calls = d.route(button(d.g, M2, &[MOD], &format!("application:accept:{shy}")).build()).await;
    assert_eq!(d.sub_of(&form.id, A2).await.map(|s| s.status.as_str().to_string()).as_deref(), Some("ACCEPTED"));
    assert_eq!(d.io.roles().last().map(|r| (r.0.clone(), r.1.clone())), Some((A2.to_string(), R1.to_string())));
    assert!(calls.iter().any(|c| c.embed_description().unwrap_or_default().contains("not messaged: their DMs are closed")));

    let gone = apply_submit(&d, &form.id, A1, &[]).await;
    d.io.remove_member(A1);
    let calls = d.route(button(d.g, M2, &[MOD], &format!("application:accept:{gone}")).build()).await;
    assert_eq!(d.sub_of(&form.id, A1).await.map(|s| s.status.as_str().to_string()).as_deref(), Some("ACCEPTED"));
    assert!(calls.iter().any(|c| c.embed_description().unwrap_or_default().contains("no longer in the server, so no roles were given")));
    d.done().await;
}

#[tokio::test]
async fn db_submit_needs_required_answers_and_reverts_when_the_review_channel_fails() {
    let Some(d) = Apps::new("955008000000000607", true).await else { return };
    let form = d.form(2, json!({})).await;
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    let m = modal_of(&calls);
    let calls = d.route(modal_submit(d.g, A1, &[], m["custom_id"].as_str().unwrap_or(""), &refs(&fill(&m, &["only the first", "   "])), None)).await;
    let refused = calls.last().expect("answer");
    assert_eq!(refused.embed_title(), Some("❌  Answer Required"));
    assert!(refused.embed_description().unwrap_or_default().contains("Question 2"));
    let retry_id = row_ids(&refused.body).first().cloned().unwrap_or_default();
    assert!(retry_id.starts_with("application:next:") && retry_id.ends_with(":0"), "{retry_id}");
    assert!(d.io.sent().is_empty());
    let calls = d.route(button(d.g, A1, &[], &retry_id).build()).await;
    let m2 = modal_of(&calls);
    assert_eq!(m2["components"][0]["components"][0]["value"], "only the first", "the first answer was saved");
    d.io.fail_sends(&[Some(50001)]);
    let calls = d.route(modal_submit(d.g, A1, &[], m2["custom_id"].as_str().unwrap_or(""), &refs(&fill(&m2, &["only the first", "second"])), Some(message_with(None)))).await;
    assert!(calls.last().and_then(|c| c.embed_description()).unwrap_or_default().contains("could not post in the review channel"));
    assert_eq!(d.sub_of(&form.id, A1).await.map(|s| s.status.as_str().to_string()).as_deref(), Some("DRAFT"), "the draft comes back so nothing is lost");
    d.done().await;
}

#[tokio::test]
async fn db_cleanup_forgets_deleted_panels_review_posts_channels_and_roles() {
    let Some(d) = Apps::new("955008000000000608", true).await else { return };
    let form = d.form(1, json!({ "panelChannelId": "PC", "panelMessageId": "PANEL" })).await;
    let sub = insert_sub(&d, &form.id, "U1", "PENDING", Some("REVIEW"), None).await;
    let pool = d.t.db();
    let form_row = |id: String| async move { svc::find_form_by_id(pool, d.g, &id).await.expect("db").expect("form") };
    let review_of = |id: String| async move {
        bot_db::sqlx::query_scalar::<_, Option<String>>(r#"SELECT "reviewMessageId" FROM "ApplicationSubmission" WHERE "id" = $1"#).bind(id).fetch_one(pool).await.expect("db")
    };
    svc::forget_message(pool, d.g, "OTHER").await;
    assert_eq!(form_row(form.id.clone()).await.panel_message_id.as_deref(), Some("PANEL"));
    svc::forget_message(pool, d.g, "PANEL").await;
    let f = form_row(form.id.clone()).await;
    assert_eq!((f.panel_message_id, f.panel_channel_id), (None, None));
    assert_eq!(review_of(sub.clone()).await.as_deref(), Some("REVIEW"));
    svc::forget_message(pool, d.g, "REVIEW").await;
    assert_eq!(review_of(sub.clone()).await, None);
    svc::forget_message(pool, "955008000000000699", "REVIEW").await; // never fails

    let second = svc::create_form(&d.t, d.g, "Mods", None, Some("OTHER-REVIEW")).await.expect("db").expect("ok");
    svc::update_form(&d.t, d.g, &form.id, json!({ "panelChannelId": "PC", "panelMessageId": "P1" }).as_object().cloned().unwrap_or_default()).await.expect("db").expect("ok");
    let mine = insert_sub(&d, &form.id, "U2", "PENDING", Some("R1"), None).await;
    let theirs = insert_sub(&d, &second.id, "U2", "PENDING", Some("R2"), None).await;
    svc::forget_channel(pool, d.g, "PC").await;
    let f = form_row(form.id.clone()).await;
    assert_eq!((f.panel_message_id, f.review_channel_id.as_deref()), (None, Some(RC)), "a panel channel is not the review channel");
    svc::forget_channel(pool, d.g, RC).await;
    assert_eq!(form_row(form.id.clone()).await.review_channel_id, None);
    assert_eq!(review_of(mine).await, None);
    assert_eq!(review_of(theirs).await.as_deref(), Some("R2"));
    assert_eq!(form_row(second.id.clone()).await.review_channel_id.as_deref(), Some("OTHER-REVIEW"));
    let closed = form_row(form.id.clone()).await;
    assert_eq!(svc::start_draft(&d.t, &closed, "U5", &[], None, chrono::Utc::now()).await.expect("db").err(), Some(Refusal::NotSetUp));
    svc::forget_channel(pool, d.g, "UNRELATED").await;

    for (list, role) in [(svc::RoleList::Accept, "A"), (svc::RoleList::Accept, "B"), (svc::RoleList::Required, "A"), (svc::RoleList::Required, "C")] {
        svc::change_role(&d.t, d.g, &form.id, list, role, false).await.expect("db").expect("ok");
    }
    svc::forget_role(pool, d.g, "A").await;
    let f = form_row(form.id.clone()).await;
    assert_eq!((f.accept_role_ids.clone(), f.required_role_ids.clone(), f.enabled), (vec!["B".to_string()], vec!["C".to_string()], true));
    svc::forget_role(pool, d.g, "C").await;
    let f = form_row(form.id.clone()).await;
    assert!(f.required_role_ids.is_empty());
    assert!(!f.enabled, "the last required role closes the form instead of opening it to all");
    let open = svc::create_form(&d.t, d.g, "Open", None, None).await.expect("db").expect("ok");
    svc::change_role(&d.t, d.g, &open.id, svc::RoleList::Accept, "B", false).await.expect("db").expect("ok");
    svc::forget_role(pool, d.g, "B").await;
    assert!(form_row(open.id.clone()).await.enabled, "a form that never needed a role stays open");
    d.done().await;
}

#[tokio::test]
async fn db_every_raw_statement_runs_in_a_rolled_back_transaction() {
    let Some(db) = database_url() else { return };
    let pool = bot_core::testing::DbTestApp::new(&db).await.db().clone();
    for (name, sql) in svc::RAW_SQL {
        let mut tx = pool.begin().await.expect("tx");
        let mut q = bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(sql.to_string()));
        let n = (1..=8).filter(|i| sql.contains(&format!("${i}"))).max().unwrap_or(0);
        for i in 1..=n {
            q = match (*name, i) {
                ("saveAnswers", 1) => q.bind(json!([])),
                ("claimSubmit", 1) | ("claimDecision", 3) => q.bind(chrono::Utc::now().naive_utc()),
                ("claimDecision", 1) => q.bind("DENIED"),
                ("formsForAutocomplete", 2) => q.bind(false),
                ("orphanReviews" | "clearReviewChannel" | "clearPanel", 1) => q.bind(vec!["x".to_string()]),
                ("setRoleLists", 1 | 2) => q.bind(vec!["x".to_string()]),
                ("setRoleLists", 3) => q.bind(Option::<bool>::None),
                ("nameClash", 3) | ("formsForAutocomplete", 3) | ("claimDecision", 4) => q.bind(Option::<String>::None),
                _ => q.bind("x"),
            };
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
        let stripped = sql.replace("(now() AT TIME ZONE 'UTC')", "").to_lowercase();
        assert!(!stripped.contains("now()") && !stripped.contains("current_timestamp"), "{name} uses a bare now()");
    }
}

// ── The command through the router ────────────────────────────────────────────

fn admin_cmd(g: &str, group: Option<&str>, sub: &str) -> FakeInteraction {
    let f = FakeInteraction::slash("application").guild(g).user(ADMIN).roles(&[ADMIN_R]).permissions(P::MANAGE_GUILD);
    match group {
        Some(gr) => f.group(gr, sub),
        None => f.subcommand(sub),
    }
}

#[tokio::test]
async fn db_application_command_setup_path() {
    use bot_core::command::option_type as ot;
    let Some(d) = Apps::new("955008000000000609", false).await else { return };
    let last = |calls: Vec<bot_core::interaction::RecordedCall>| calls.last().cloned().expect("answered");
    let c = last(d.route(admin_cmd(d.g, Some("form"), "create").option("name", ot::STRING, json!(" Staff ")).option("description", ot::STRING, json!("Join us")).option("review_channel", ot::CHANNEL, json!(RC)).build()).await);
    assert_eq!(c.embed_title(), Some("✅  Form Created"));
    assert!(c.embed_description().unwrap_or_default().contains("module is off"), "the admin is told the module is off");
    let form = svc::find_form(d.t.db(), d.g, "Staff").await.expect("db").expect("form");
    assert_eq!((form.review_channel_id.as_deref(), form.description.as_deref()), (Some(RC), Some("Join us")));
    let c = last(d.route(admin_cmd(d.g, Some("form"), "create").option("name", ot::STRING, json!("staff")).build()).await);
    assert_eq!(c.embed_title(), Some("❌  Name Taken"));
    let c = last(d.route(admin_cmd(d.g, None, "panel").option("form", ot::STRING, json!(form.id)).option("channel", ot::CHANNEL, json!(PNL)).build()).await);
    assert_eq!(c.embed_title(), Some("❌  Not Ready"), "no questions yet");
    let add = admin_cmd(d.g, Some("question"), "add")
        .option("form", ot::STRING, json!(form.id))
        .option("label", ot::STRING, json!("Why us?"))
        .option("style", ot::STRING, json!("PARAGRAPH"))
        .option("required", ot::BOOLEAN, json!(true))
        .option("placeholder", ot::STRING, json!("tell us"));
    let c = last(d.route(add.build()).await);
    assert!(c.embed_description().unwrap_or_default().contains("**1.** Why us? ¶"), "{c:?}");
    let c = last(d.route(admin_cmd(d.g, Some("question"), "add").option("form", ot::STRING, json!(form.id)).option("label", ot::STRING, json!("why us?")).build()).await);
    assert_eq!(c.embed_title(), Some("❌  Duplicate Question"));

    let role = |list: &str, r: &str| admin_cmd(d.g, Some("form"), "role").option("form", ot::STRING, json!(form.id)).option("list", ot::STRING, json!(list)).option("role", ot::ROLE, json!(r)).build();
    let c = last(d.route(role("accept", HIGH)).await);
    assert!(c.embed_description().unwrap_or_default().contains("above my highest role"), "{c:?}");
    assert!(svc::find_form(d.t.db(), d.g, &form.id).await.expect("db").expect("f").accept_role_ids.is_empty());
    assert_eq!(last(d.route(role("accept", R1)).await).embed_title(), Some("✅  Form Updated"));
    assert_eq!(last(d.route(role("required", d.g)).await).embed_title(), Some("❌  Cannot Use That Role"));
    assert_eq!(last(d.route(role("required", R1)).await).embed_title(), Some("✅  Form Updated"));

    let c = last(d.route(admin_cmd(d.g, None, "panel").option("form", ot::STRING, json!(form.id)).option("channel", ot::CHANNEL, json!(PNL)).build()).await);
    assert_eq!(c.embed_title(), Some("✅  Panel Posted"));
    let sent = d.io.sent();
    assert_eq!(sent.last().map(|s| s.0.as_str()), Some(PNL));
    let panel = svc::find_form(d.t.db(), d.g, &form.id).await.expect("db").expect("f");
    assert_eq!(panel.panel_channel_id.as_deref(), Some(PNL));
    let old = panel.panel_message_id.clone().expect("panel id");

    let edit = admin_cmd(d.g, Some("form"), "edit")
        .option("form", ot::STRING, json!(form.id))
        .option("name", ot::STRING, json!("Staff Team"))
        .option("description", ot::STRING, json!("none"))
        .option("cooldown_hours", ot::INTEGER, json!(0))
        .option("enabled", ot::BOOLEAN, json!(false));
    let c = last(d.route(edit.build()).await);
    assert_eq!(c.embed_title(), Some("✅  Form Updated"));
    let edits = d.io.edits();
    assert_eq!(edits.len(), 1, "the panel is re-rendered");
    assert_eq!(edits[0].1, old);
    assert!(edits[0].2["embeds"][0]["title"].as_str().unwrap_or("").contains("Staff Team"));
    let now = svc::find_form(d.t.db(), d.g, &form.id).await.expect("db").expect("f");
    assert_eq!((now.name.as_str(), now.description.clone(), now.cooldown_hours, now.enabled), ("Staff Team", None, 0, false));
    assert_eq!(last(d.route(admin_cmd(d.g, Some("form"), "edit").option("form", ot::STRING, json!(form.id)).build()).await).embed_title(), Some("❌  Nothing To Change"));

    let c = last(d.route(admin_cmd(d.g, None, "panel").option("form", ot::STRING, json!(form.id)).option("channel", ot::CHANNEL, json!(PNL)).build()).await);
    assert!(d.io.deletes().iter().any(|(_, m)| *m == old), "the old panel message was deleted: {c:?} {:?}", d.io.deletes());
    assert_eq!(last(d.route(admin_cmd(d.g, Some("question"), "remove").option("form", ot::STRING, json!(form.id)).option("question", ot::STRING, json!("1")).build()).await).embed_title(), Some("✅  Question Removed"));
    assert_eq!(last(d.route(admin_cmd(d.g, Some("question"), "remove").option("form", ot::STRING, json!(form.id)).option("question", ot::STRING, json!("1")).build()).await).embed_title(), Some("❌  Question Not Found"));
    let c = last(d.route(admin_cmd(d.g, Some("form"), "list").build()).await);
    let field = c.body["embeds"][0]["fields"][0]["value"].as_str().unwrap_or("").to_string();
    assert!(field.contains("Staff Team** · ⏸️ closed · ⚠️ no questions"), "{field}");

    // Members never reach the admin subcommands.
    let member = FakeInteraction::slash("application").guild(d.g).user(A1).group("form", "list").build();
    let c = last(d.route(member).await);
    assert!(c.embed_title().unwrap_or_default().contains("Permission"), "{c:?}");
    d.done().await;
}

#[tokio::test]
async fn db_autocomplete_members_see_open_forms_while_the_module_is_on_admins_see_all() {
    let Some(d) = Apps::new("955008000000000610", true).await else { return };
    let form = d.form(3, json!({})).await;
    let closed = svc::create_form(&d.t, d.g, "Closed one", None, None).await.expect("db").expect("ok");
    svc::update_form(&d.t, d.g, &closed.id, json!({ "enabled": false }).as_object().cloned().unwrap_or_default()).await.expect("db").expect("ok");
    let ask = |i: serenity::all::Interaction| async {
        let calls = d.route(i).await;
        calls.last().map(|c| c.choices()).unwrap_or_default()
    };
    let names = |c: &[Value]| c.iter().map(|x| x["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    let apply = || FakeInteraction::autocomplete("application").guild(d.g).user(A1).subcommand("apply").focused("form", "").build();
    let c = ask(apply()).await;
    assert_eq!(names(&c), vec!["Staff"], "closed forms are hidden from members");
    assert_eq!(c[0]["value"], json!(form.id));
    let admin = FakeInteraction::autocomplete("application").guild(d.g).user(ADMIN).roles(&[ADMIN_R]).permissions(P::MANAGE_GUILD).group("form", "edit").focused("form", "clo").build();
    assert_eq!(names(&ask(admin).await), vec!["⏸️ Closed one"]);
    let mut q = FakeInteraction::autocomplete("application").guild(d.g).user(ADMIN).roles(&[ADMIN_R]).permissions(P::MANAGE_GUILD).group("question", "remove");
    q = q.option("form", 3, json!(form.id)).focused("question", "2");
    assert_eq!(json!(ask(q.build()).await), json!([{ "name": "2. Question 2", "value": "2" }]));
    d.t.settings().update_settings(d.g, bot_core::SettingsPatch::new().set("applicationsEnabled", false)).await.expect("off");
    assert!(ask(apply()).await.is_empty(), "no suggestions while the module is off");
    d.done().await;
}

// ── Cross-language: the JS service writes, Rust reads; Rust writes, the JS reads ──

#[tokio::test]
async fn js_writes_forms_and_a_draft_and_rust_reads_and_continues_them() {
    let Some(d) = Apps::new("955008000000000613", true).await else { return };
    let js = run_node("rust/fixtures/gen/feat-welcome-js.mjs", &["app-write", d.g, A1, RC]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["clash"], json!({ "ok": false, "code": "NAME_TAKEN" }));
    let staff = svc::find_form(d.t.db(), d.g, "staff").await.expect("db").expect("staff");
    same(&serde_json::to_value(&staff).unwrap_or(Value::Null), &js["staff"], "the form reads the same through Prisma and Rust");
    assert_eq!(svc::parse_questions(&staff.questions).len(), 3);
    let draft = svc::start_draft(&d.t, &staff, A1, &["955008000000000302".into()], None, chrono::Utc::now()).await.expect("db").expect("ok");
    assert_eq!(draft.id, js["draft"]["id"].as_str().unwrap_or(""), "Rust resumes the draft the JS created");
    same(&serde_json::to_value(&draft).unwrap_or(Value::Null), &js["draft"], "draft");
    let list = svc::list_forms(d.t.db(), d.g).await.expect("list");
    assert_eq!(list.iter().map(|f| f.form.name.as_str()).collect::<Vec<_>>(), vec!["Staff", "a_b%"]);
    d.done().await;
}

#[tokio::test]
async fn rust_writes_forms_and_submissions_and_the_js_reads_them_the_same_way() {
    let Some(d) = Apps::new("955008000000000614", true).await else { return };
    let staff = d.form(3, json!({ "acceptMessage": "Welcome {user}!", "cooldownHours": 2 })).await;
    svc::create_form(&d.t, d.g, "a_b%", None, None).await.expect("db").expect("ok");
    svc::change_role(&d.t, d.g, &staff.id, svc::RoleList::Accept, R1, false).await.expect("db").expect("ok");
    // A submitted application through the real flow, so the row has answers, status and review message.
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", staff.id)).build()).await;
    let m = modal_of(&calls);
    d.route(modal_submit(d.g, A1, &[], m["custom_id"].as_str().unwrap_or(""), &refs(&fill(&m, &["one", "two", "three"])), None)).await;
    let refs_list = ["staff", "STAFF", "a_b%", "axb%", "a_bx", "A_B%", "nope"];
    let queries = ["", "st", "_", "%", "a_", "xyz"];
    let js = run_node(
        "rust/fixtures/gen/feat-welcome-js.mjs",
        &["app-read", d.g, A1, &serde_json::to_string(&refs_list).unwrap_or_default(), &serde_json::to_string(&queries).unwrap_or_default()],
    );
    assert!(js.get("error").is_none(), "{js}");
    let ours = svc::list_forms(d.t.db(), d.g).await.expect("list");
    let ours_json: Vec<Value> = ours
        .iter()
        .map(|f| {
            let mut v = serde_json::to_value(&f.form).unwrap_or(Value::Null);
            v["pending"] = json!(f.pending);
            v
        })
        .collect();
    same(&json!(ours_json), &js["forms"], "listForms");
    for r in refs_list {
        let id = svc::find_form(d.t.db(), d.g, r).await.expect("db").map(|f| f.id);
        assert_eq!(json!(id), js["found"][r], "findForm({r:?}) agrees with Prisma's case-insensitive equals");
    }
    for q in queries {
        let names: Vec<String> = svc::find_forms_for_autocomplete(d.t.db(), d.g, q, false).await.expect("db").into_iter().map(|f| f.name).collect();
        assert_eq!(json!(names), js["auto"][q], "findFormsForAutocomplete({q:?}) agrees with Prisma's contains");
    }
    assert_eq!(js["pending"], json!(1));
    let sub = d.sub_of(&staff.id, A1).await.expect("sub");
    same(&json!([serde_json::to_value(&sub).unwrap_or(Value::Null)]), &js["subs"], "the submission reads the same through Prisma");
    d.done().await;
}

/// `[...member.roles.cache.keys()]` includes @everyone (the guild id): a form whose required roles name the guild id
/// (possible through the dashboard) is open to every member, as in the JS.
#[tokio::test]
async fn db_apply_counts_the_everyone_role_among_the_member_roles() {
    let Some(d) = Apps::new("955008000000000615", true).await else { return };
    let form = d.form(2, json!({ "requiredRoleIds": [d.g] })).await;
    let calls = d.route(button(d.g, A1, &[], &format!("application:apply:{}", form.id)).build()).await;
    assert_eq!(calls[0].method, "showModal", "{calls:?}");
    d.done().await;
}

/// discord.js puts the resolved option role and channel into the caches when it builds the interaction, so
/// `guild.roles.cache.get(picked.id)` and `guild.channels.cache.get(picked.id)` find them even when the bot's cache
/// lacked them: an uncached role is accepted and an uncached review channel is fetched.
#[tokio::test]
async fn db_application_options_missing_from_the_cache_still_work() {
    const LATE_ROLE: &str = "955008000000000626";
    const LATE_CH: &str = "955008000000000616";
    let g = "955008000000000617";
    let Some(db) = database_url() else { return };
    let t = DbTestApp::new(&db).await.with_guilds([g]);
    let rest = bot_core::testing::FakeRest::new();
    t.set_discord(rest.discord(&[app_guild(g).json()]));
    t.reset_guilds(&[g]).await;
    t.settings().update_settings(g, bot_core::SettingsPatch::new().set("applicationsEnabled", true)).await.expect("settings");
    t.install(registry());
    let io = FakeIo::install(&t);
    let d = Apps { t, io, g };
    rest.on("GET", &format!("/channels/{LATE_CH}"), 200, json!({ "id": LATE_CH, "type": 0, "guild_id": d.g, "name": "late", "position": 7, "permission_overwrites": [], "nsfw": false, "parent_id": null }));
    use bot_core::command::option_type as ot;
    let last = |calls: Vec<bot_core::interaction::RecordedCall>| calls.last().cloned().expect("answered");
    let c = last(d.route(admin_cmd(d.g, Some("form"), "create").option("name", ot::STRING, json!("Late")).option("review_channel", ot::CHANNEL, json!(LATE_CH)).build()).await);
    assert_eq!(c.embed_title(), Some("✅  Form Created"), "{c:?}");
    let form = svc::find_form(d.t.db(), d.g, "late").await.expect("db").expect("form");
    assert_eq!(form.review_channel_id.as_deref(), Some(LATE_CH));
    let role = admin_cmd(d.g, Some("form"), "role")
        .option("form", ot::STRING, json!(form.id))
        .option("list", ot::STRING, json!("required"))
        .option("role", ot::ROLE, json!(LATE_ROLE))
        .resolve_roles_by_default()
        .build();
    assert_eq!(last(d.route(role).await).embed_title(), Some("✅  Form Updated"));
    d.done().await;
}
