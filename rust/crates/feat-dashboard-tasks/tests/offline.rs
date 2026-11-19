//! Offline tests of feat-dashboard-tasks: the golden checks, the JS-produced fixtures
//! (rust/fixtures/gen/feat-dashboard-tasks-golden.mjs), the ported bot-side cases of test/dashboard-tasks.test.js, and
//! the gates and payload checks of every task that need no database (test/dashboard-moderation.test.js,
//! test/dashboard-giveaways-polls.test.js, test/dashboard-panels.test.js).

use std::sync::{Arc, Mutex};

use bot_config::JobType;
use bot_core::AppConfig;
use bot_core::scheduler::{Job, ScheduleJob};
use bot_core::settings::SettingsPatch;
use bot_core::testing::{FakeGuild, TEST_BOT_ID, TestApp, load_golden};
use bot_core::{PermanentJobError, Registry};
use bot_db::JobStatus;
use feat_dashboard_tasks::index::{self, DashboardTask, TaskCtx, TaskMap, TaskOutcome, execute_task, resolve_actor};
use feat_dashboard_tasks::members::{self, change_text, read_xp_payload};
use feat_dashboard_tasks::moderation::{ActionResult, action_message, read_action_payload};
use feat_dashboard_tasks::shared::{self, TaskError, task_error};
use feat_dashboard_tasks::{channel_access, util};
use feat_levels::xp_service::{RoleSync, XpChange};
use feat_moderation::moderation_service::EscalationOutcome;
use serde_json::{Value, json};
use serenity::all::Permissions;

const G: &str = "900000000000000001";
const BOB: &str = "900000000000000002";
const ALICE: &str = "900000000000000003";
const CAROL: &str = "900000000000000004";
const OWNER: &str = "900000000000000099";
const ROLE_ADMIN: &str = "910000000000000001";
const ROLE_MOD: &str = "910000000000000002";
const ROLE_BOT: &str = "910000000000000003";
const CH_TEXT: &str = "920000000000000001";
const CH_PRIVATE: &str = "920000000000000002";
const CH_CATEGORY: &str = "920000000000000003";

fn golden() -> Value {
    load_golden("feat-dashboard-tasks/golden.json")
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_dashboard_tasks::register(&mut reg);
    reg
}

/// The guild of the gate tests: BOB is an admin (Manage Server), ALICE a moderator (Kick Members), CAROL a plain member;
/// the bot can post in the text channel and manage the category; nobody but admins sees the private channel.
fn guild() -> FakeGuild {
    FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .role(ROLE_ADMIN, 3, Permissions::MANAGE_GUILD)
        .role(ROLE_MOD, 2, Permissions::KICK_MEMBERS)
        .role(ROLE_BOT, 5, Permissions::EMBED_LINKS | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES | Permissions::READ_MESSAGE_HISTORY)
        .member(TEST_BOT_ID, &[ROLE_BOT])
        .member(BOB, &[ROLE_ADMIN])
        .member(ALICE, &[ROLE_MOD])
        .member(CAROL, &[])
        .channel(CH_TEXT, 0, "general", &[])
        .channel(CH_PRIVATE, 0, "staff", &[(G, false, Permissions::empty(), Permissions::VIEW_CHANNEL), (ROLE_BOT, false, Permissions::VIEW_CHANNEL, Permissions::empty())])
        .channel(CH_CATEGORY, 4, "Tickets", &[])
}

fn app_with(config: AppConfig) -> TestApp {
    let t = TestApp::with_config(config).with_guilds([G]).with_fake_guilds(&[guild()]);
    t.install(registry());
    t
}

fn app() -> TestApp {
    app_with(AppConfig::default())
}

async fn ctx(t: &TestApp, actor: &str, payload: Value) -> TaskCtx {
    let actor = resolve_actor(t, G, Some(&json!(actor))).await;
    TaskCtx { app: t.app.clone(), guild_id: G.into(), actor, payload, task_id: "job1".into() }
}

async fn run(t: &TestApp, kind: &str, actor: &str, payload: Value) -> Result<TaskOutcome, String> {
    let task = index::tasks().get(kind).unwrap_or_else(|| panic!("{kind} is registered")).clone();
    let c = ctx(t, actor, payload).await;
    (task.run)(c).await.map_err(|e| match TaskError::of(&e) {
        Some(te) => te.0.clone(),
        None => format!("NOT A TASK ERROR: {e}"),
    })
}

fn refusal(r: Result<TaskOutcome, String>) -> String {
    match r {
        Ok(o) => panic!("expected a refusal, got {o:?}"),
        Err(e) => e,
    }
}

async fn set_flags(t: &TestApp, flags: &[&str], on: bool) {
    let mut patch = SettingsPatch::new();
    for f in flags {
        patch = patch.set(f, on);
    }
    t.settings().update_settings(G, patch).await.expect("settings saved");
}

// ── Golden checks ──────────────────────────────────────────────────────────────

#[test]
fn plugin_matches_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &[]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "dashboard-tasks");
    assert!(bot_core::testing::persistent_handler_problems_for(&reg, &["dashboard-tasks"]).is_empty());
}

#[test]
fn every_kind_of_the_js_is_registered_in_order() {
    let g = golden();
    let js: Vec<&str> = g["kinds"].as_array().unwrap().iter().map(|k| k.as_str().unwrap()).collect();
    assert_eq!(index::tasks().kinds(), js);
    for kind in js {
        assert!(shared::is_task_kind(kind), "{kind}");
    }
}

#[test]
fn a_kind_declared_twice_is_refused() {
    let t = || DashboardTask::new(|_c: TaskCtx| async { Ok(TaskOutcome::message("x")) });
    let err = TaskMap::from_groups(vec![vec![("a.b", t())], vec![("a.b", t())]]).err();
    assert_eq!(err.as_deref(), Some("dashboard task a.b is declared twice"));
}

#[test]
fn constants_and_keys_match_the_js() {
    let g = golden();
    let c = &g["constants"];
    assert_eq!(c["TASK_JOB_TYPE"], json!(shared::TASK_JOB_TYPE));
    assert_eq!(JobType::DashboardTask.as_str(), shared::TASK_JOB_TYPE);
    // JOB_TYPES['dashboard.task'] and INTERACTIVE_JOB_TYPES (test/dashboard-tasks.test.js).
    let info = JobType::DashboardTask.info();
    assert_eq!((info.owner, info.recurring, info.dedupe, info.interactive), ("dashboard-tasks", false, "dashboard.task:{taskId}", true));
    assert_eq!(bot_config::INTERACTIVE_JOB_TYPES.to_vec(), vec![JobType::DashboardTask]);
    assert_eq!(c["TASK_RESULT_TTL_SECONDS"], json!(shared::TASK_RESULT_TTL_SECONDS));
    assert_eq!(c["TASK_MESSAGE_MAX"], json!(shared::TASK_MESSAGE_MAX));
    assert_eq!(c["resultKey"], json!(shared::task_result_key("cm0abcdef0123456789abcdef")));
    assert_eq!(c["dedupeKey"], json!(shared::task_dedupe_key("0b6f1e9e-58c5-4a51-9d55-a8c1a3f7e2b4")));
    assert_eq!(c["XP_STEP_MAX"], json!(members::XP_STEP_MAX));
    assert_eq!(c["XP_OPS"], json!(members::XP_OPS));
    assert_eq!(c["DASHBOARD_MOD_ACTIONS"], json!(feat_dashboard_tasks::moderation::DASHBOARD_MOD_ACTIONS));
    assert_eq!(c["COMMAND_REASON_MAX"], json!(feat_dashboard_tasks::moderation::COMMAND_REASON_MAX));
    assert_eq!(c["ACCESS_NEEDS"], json!(channel_access::ACCESS_NEEDS));
    assert_eq!(c["ACCESS_CHECKS_MAX"], json!(channel_access::ACCESS_CHECKS_MAX));
}

#[test]
fn the_kind_pattern_matches_the_js() {
    for case in golden()["kindSweep"].as_array().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        assert_eq!(shared::is_task_kind(kind), case["ok"].as_bool().unwrap(), "{kind:?}");
    }
}

/// A number the way JSON.stringify writes it (an integral f64 as an integer).
fn num(v: Option<f64>) -> Value {
    match v {
        None => Value::Null,
        Some(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => json!(n as i64),
        Some(n) => json!(n),
    }
}

#[test]
fn read_action_payload_matches_the_js() {
    for case in golden()["actionPayloads"].as_array().unwrap() {
        let got = match read_action_payload(&case["input"]) {
            Ok(p) => json!({ "ok": {
                "action": p.action, "userId": p.user_id, "reason": p.reason, "durationMs": num(p.duration_ms),
                "deleteMessageSeconds": p.delete_message_seconds, "proofUrl": p.proof_url,
            }}),
            Err(e) => json!({ "error": e.0 }),
        };
        let want = case.as_object().unwrap().iter().filter(|(k, _)| *k != "input").map(|(k, v)| (k.clone(), v.clone())).collect::<serde_json::Map<_, _>>();
        assert_eq!(got, Value::Object(want), "input {}", case["input"]);
    }
}

#[test]
fn read_xp_payload_matches_the_js() {
    for case in golden()["xpPayloads"].as_array().unwrap() {
        let got = match read_xp_payload(&case["input"]) {
            Ok(p) => json!({ "ok": { "op": p.op, "userId": p.user_id, "amount": p.amount } }),
            Err(e) => json!({ "error": e.0 }),
        };
        let want = case.as_object().unwrap().iter().filter(|(k, _)| *k != "input").map(|(k, v)| (k.clone(), v.clone())).collect::<serde_json::Map<_, _>>();
        assert_eq!(got, Value::Object(want), "input {}", case["input"]);
    }
}

#[test]
fn read_access_payload_matches_the_js() {
    for case in golden()["accessPayloads"].as_array().unwrap() {
        let got = match channel_access::read_access_payload(&case["input"]) {
            Ok(list) => json!({ "ok": list.iter().map(|c| {
                let mut o = json!({ "channelId": c.channel_id });
                if let Some(n) = &c.needs { o["needs"] = json!(n); }
                o["optional"] = json!(c.optional);
                o
            }).collect::<Vec<_>>() }),
            Err(e) => json!({ "error": e.0 }),
        };
        let want = case.as_object().unwrap().iter().filter(|(k, _)| *k != "input").map(|(k, v)| (k.clone(), v.clone())).collect::<serde_json::Map<_, _>>();
        assert_eq!(got, Value::Object(want), "input {}", case["input"]);
    }
}

fn action_result(r: &Value) -> ActionResult {
    let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect::<Vec<_>>());
    ActionResult {
        case_id: r["infraction"]["caseId"].as_i64().unwrap() as i32,
        infraction_type: r["infraction"]["type"].as_str().unwrap().into(),
        action: r["action"].as_str().map(String::from),
        dm: r["dm"].as_str().map(String::from),
        warnings: strings(&r["warnings"]).unwrap_or_default(),
        note: r["note"].as_str().map(String::from),
        removed: strings(&r["removed"]),
        cleared: r["cleared"].as_u64(),
        escalation: r["escalation"].as_object().map(|e| EscalationOutcome {
            tier_id: e["tierId"].as_i64().unwrap(),
            action: e["action"].as_str().unwrap().into(),
            ok: e["ok"].as_bool().unwrap(),
            case_id: e.get("caseId").and_then(Value::as_i64).map(|n| n as i32),
            error: e.get("error").and_then(Value::as_str).map(String::from),
        }),
    }
}

#[test]
fn mod_action_messages_match_the_js() {
    let mut checked = 0;
    for case in golden()["modMessages"].as_array().unwrap() {
        let Some(message) = case["message"].as_str() else { continue };
        let input = read_action_payload(&case["payload"]).expect("valid payload");
        let res = action_result(&case["result"]);
        let warns = case["activeWarns"].as_i64();
        assert_eq!(action_message(&input, &res, case["name"].as_str().unwrap(), warns), message, "{}", case["payload"]);
        let data = json!({ "caseId": res.case_id, "action": res.action.clone().unwrap_or(input.action.clone()), "userId": input.user_id });
        assert_eq!(data, case["data"]);
        checked += 1;
    }
    assert!(checked >= 12);
}

#[test]
fn xp_messages_match_the_js() {
    const ROLE: &str = "900000000000000011";
    let role_name = |id: &str| if id == ROLE { "@Regular".to_string() } else { id.to_string() };
    let change = |v: &Value| -> XpChange {
        XpChange {
            xp: v["xp"].as_i64().unwrap(),
            old_level: v["oldLevel"].as_i64().unwrap(),
            new_level: v["newLevel"].as_i64().unwrap(),
            created: v["created"].as_bool().unwrap_or(false),
            old_xp: v["oldXp"].as_i64(),
        }
    };
    let roles = |v: &Value| -> Option<RoleSync> {
        let r = v.get("roles")?.as_object()?;
        let ids = |k: &str| r[k].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>();
        let failed = r["failed"].as_array().unwrap().iter().map(|f| (f["roleId"].as_str().unwrap().to_string(), f["reason"].as_str().unwrap().to_string())).collect();
        Some(RoleSync { added: ids("added"), removed: ids("removed"), failed })
    };
    let mut checked = 0;
    for case in golden()["xpMessages"].as_array().unwrap() {
        let Some(message) = case["message"].as_str() else { continue };
        let p = read_xp_payload(&case["payload"]).unwrap();
        let name = case["name"].as_str().unwrap();
        let lc = roles(&case["handleLevelChange"]);
        let got = match p.op.as_str() {
            "give" => {
                let r = change(&case["applyXpDelta"]);
                members::give_message(p.amount.unwrap(), name, &r, &change_text(r.old_level, r.new_level, lc.as_ref(), &role_name))
            }
            "take" => {
                let r = change(&case["subtractXp"]);
                members::take_message(name, &r, &change_text(r.old_level, r.new_level, lc.as_ref(), &role_name))
            }
            "set" => {
                let r = change(&case["setXp"]);
                members::set_message(name, &r, &change_text(r.old_level, r.new_level, lc.as_ref(), &role_name))
            }
            _ => {
                let removed: Vec<String> = case["syncLevelRoles"]["removed"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
                members::reset_message(name, case["resetXp"]["oldLevel"].as_i64().unwrap(), Some(&removed), &role_name)
            }
        };
        assert_eq!(got, message);
        checked += 1;
    }
    assert!(checked >= 8);
}

// ── executeTask (test/dashboard-tasks.test.js, bot side) ────────────────────────

fn job(kind: Value, payload: Value) -> Job {
    let mut outer = json!({ "actorId": BOB, "payload": payload });
    if !kind.is_null() {
        outer["kind"] = kind;
    }
    Job {
        id: "job1".into(),
        guild_id: G.into(),
        job_type: "dashboard.task".into(),
        payload: outer,
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 1,
        dedupe_key: Some("dashboard.task:x".into()),
        version: 0,
    }
}

fn doubles(seen: Arc<Mutex<Option<TaskCtx>>>) -> TaskMap {
    let s = seen.clone();
    TaskMap::from_groups(vec![vec![
        (
            "test.echo",
            DashboardTask::new(move |c: TaskCtx| {
                let s = s.clone();
                async move {
                    *s.lock().unwrap() = Some(c);
                    Ok(TaskOutcome::new("x".repeat(900), json!({ "id": 7 })))
                }
            }),
        ),
        ("test.empty", DashboardTask::new(|_c: TaskCtx| async { Ok(TaskOutcome { message: String::new(), data: None }) })),
        ("test.refuse", DashboardTask::new(|_c: TaskCtx| async { Err(task_error("r".repeat(600))) })),
        ("test.crash", DashboardTask::new(|_c: TaskCtx| async { Err(anyhow::anyhow!("secret internals")) })),
    ]])
    .unwrap()
}

#[tokio::test]
async fn a_task_gets_the_guild_the_actor_and_its_payload_and_its_message_is_kept_short() {
    let t = app();
    let seen = Arc::new(Mutex::new(None));
    let tasks = doubles(seen.clone());
    execute_task(&tasks, &job(json!("test.echo"), json!({ "a": 1 })), &t, G).await.unwrap();
    let c = seen.lock().unwrap().clone().unwrap();
    assert_eq!(c.guild_id, G);
    assert_eq!(c.payload, json!({ "a": 1 }));
    assert_eq!(c.actor.id, BOB);
    assert_eq!(c.actor.member.as_ref().map(|m| m.user.id.to_string()), Some(BOB.to_string()));
    assert!(!c.actor.is_bot_owner);
    assert_eq!(c.task_id, "job1");
    // The result the dashboard reads: `{ ok: true, message, data }`, 900 s.
    let key = shared::task_result_key("job1");
    let raw = t.cache_backend.raw(&key).expect("stored");
    assert_eq!(raw, json!({ "ok": true, "message": "x".repeat(500), "data": { "id": 7 } }).to_string());
    let ttl = t.cache_backend.pttl(&key);
    assert!(ttl > 899_000 && ttl <= 900_000, "{ttl}");

    execute_task(&tasks, &job(json!("test.empty"), json!({})), &t, G).await.unwrap();
    assert_eq!(t.cache_backend.raw(&key).unwrap(), r#"{"ok":true,"message":"Done.","data":null}"#);
}

#[tokio::test]
async fn an_actor_who_left_the_server_reaches_the_task_as_member_none_and_the_payload_defaults_to_empty() {
    let t = app();
    let seen = Arc::new(Mutex::new(None));
    let tasks = doubles(seen.clone());
    let mut j = job(json!("test.echo"), json!(null));
    j.payload["actorId"] = json!("900000000000000077");
    j.payload.as_object_mut().unwrap().remove("payload");
    execute_task(&tasks, &j, &t, G).await.unwrap();
    let c = seen.lock().unwrap().clone().unwrap();
    assert!(c.actor.member.is_none());
    assert_eq!(c.payload, json!({}), "a missing payload is {{}}");

    let mut j = job(json!("test.echo"), json!(null));
    j.payload["actorId"] = json!(5);
    execute_task(&tasks, &j, &t, G).await.unwrap();
    let c = seen.lock().unwrap().clone().unwrap();
    assert_eq!(c.payload, Value::Null, "an explicit null stays null");
    assert_eq!(c.actor.id, "5");
    assert!(c.actor.member.is_none());
}

#[tokio::test]
async fn the_bot_owner_is_recognised_only_by_an_exact_id() {
    let t = app_with(AppConfig { bot_owner_id: Some(CAROL.into()), ..Default::default() });
    assert!(resolve_actor(&t, G, Some(&json!(CAROL))).await.is_bot_owner);
    assert!(!resolve_actor(&t, G, Some(&json!(BOB))).await.is_bot_owner);
    assert!(!resolve_actor(&t, G, None).await.is_bot_owner);
    let t = app();
    assert!(!resolve_actor(&t, G, Some(&json!(CAROL))).await.is_bot_owner, "no BOT_OWNER_ID: nobody");
}

#[tokio::test]
async fn failures_are_permanent_with_the_messages_of_the_js() {
    let t = app();
    let tasks = doubles(Arc::new(Mutex::new(None)));
    for case in golden()["execute"].as_array().unwrap() {
        let kind = match &case["kind"] {
            Value::String(s) if s == "__undefined__" => None,
            k => Some(k.clone()),
        };
        let mut j = job(json!(null), json!({}));
        match kind {
            Some(k) => {
                j.payload["kind"] = k;
            }
            None => {
                j.payload.as_object_mut().unwrap().remove("kind");
            }
        }
        let err = execute_task(&tasks, &j, &t, G).await.expect_err("fails");
        assert!(PermanentJobError::is(&err), "{err}");
        assert_eq!(err.to_string(), case["error"].as_str().unwrap(), "kind {}", case["kind"]);
    }
}

#[tokio::test]
async fn the_job_runs_through_the_scheduler_once_and_stores_its_result_or_its_reason() {
    let t = app();
    let queue = |kind: &str, payload: Value, key: &str| {
        ScheduleJob::new(G, JobType::DashboardTask, t.clock().now())
            .payload(json!({ "kind": kind, "actorId": BOB, "payload": payload }))
            .dedupe_key(shared::task_dedupe_key(key))
            .max_attempts(1)
    };
    // access.channels: BOB (an admin) may use the text channel.
    let ok = t.scheduler().schedule_job(queue("access.channels", json!({ "checks": [{ "channelId": CH_TEXT }] }), "a")).await.unwrap();
    let refused = t.scheduler().schedule_job(queue("access.channels", json!({ "checks": [] }), "b")).await.unwrap();
    let unknown = t.scheduler().schedule_job(queue("nope.nope", json!({}), "c")).await.unwrap();
    // The interactive loop runs one job per guild at a time.
    let mut claimed = 0;
    for _ in 0..5 {
        claimed += t.scheduler().tick(&t, true).await;
    }
    assert_eq!(claimed, 3);
    assert!(t.job_store.by_id(&ok.id).is_none(), "a finished task's row is deleted");
    let stored: Value = serde_json::from_str(&t.cache_backend.raw(&shared::task_result_key(&ok.id)).unwrap()).unwrap();
    assert_eq!(stored, json!({ "ok": true, "message": "You can use those channels.", "data": { "problems": [] } }));
    let row = t.job_store.by_id(&refused.id).unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert_eq!(row.last_error.as_deref(), Some("Those channel checks were not valid. Reload the page and try again."));
    let row = t.job_store.by_id(&unknown.id).unwrap();
    assert_eq!(row.status, JobStatus::Failed);
    assert_eq!(row.last_error.as_deref(), Some("This bot version cannot do \"nope.nope\" yet. Update the bot."));
}

// ── access.channels ──────────────────────────────────────────────────────────

#[tokio::test]
async fn access_channels_runs_every_check_with_discords_overwrites() {
    let t = app();
    let payload = json!({ "checks": [
        { "channelId": CH_TEXT },
        { "channelId": CH_PRIVATE, "needs": ["ViewChannel"] },
        { "channelId": "920000000000000099", "optional": true },
        { "channelId": "920000000000000098" },
    ]});
    let r = run(&t, "access.channels", CAROL, payload.clone()).await.unwrap();
    assert_eq!(r.message, "You need View Channel in #staff to do that.");
    assert_eq!(
        r.data,
        Some(json!({ "problems": [
            { "index": 1, "text": "You need View Channel in #staff to do that." },
            { "index": 3, "text": "The bot cannot find that channel, so your access to it cannot be checked." },
        ]}))
    );
    // Only the guild owner and Administrators are exempt (Manage Server is not); the bot owner is not checked; a member
    // who left is refused.
    let r = run(&t, "access.channels", BOB, json!({ "checks": [{ "channelId": CH_PRIVATE }] })).await.unwrap();
    assert_eq!(r.message, "You need View Channel in #staff to do that.");
    let r = run(&t, "access.channels", BOB, json!({ "checks": [{ "channelId": CH_TEXT, "needs": ["ViewChannel", "SendMessages"] }] })).await.unwrap();
    assert_eq!(r.message, "You can use those channels.");
    let t2 = app_with(AppConfig { bot_owner_id: Some("900000000000000055".into()), ..Default::default() });
    let r = run(&t2, "access.channels", "900000000000000055", payload).await.unwrap();
    assert_eq!((r.message.as_str(), r.data), ("The bot owner is not checked.", Some(json!({ "problems": [] }))));
    let e = refusal(run(&t, "access.channels", "900000000000000077", json!({ "checks": [{ "channelId": CH_TEXT }] })).await);
    assert_eq!(e, "You need to be a member of this server to do that.");
}

// ── Gates and payloads (no database) ───────────────────────────────────────────

#[tokio::test]
async fn mod_tasks_check_who_may_act_and_the_module() {
    let t = app_with(AppConfig { bot_owner_id: Some("900000000000000055".into()), ..Default::default() });
    let kick = json!({ "action": "KICK", "userId": ALICE });
    // The payload is read first.
    assert_eq!(refusal(run(&t, "mod.action", CAROL, json!(null)).await), "The request was malformed.");
    assert_eq!(refusal(run(&t, "mod.action", "900000000000000077", kick.clone()).await), "You are no longer a member of this server.");
    assert!(refusal(run(&t, "mod.action", "900000000000000055", kick.clone()).await).starts_with("Join this server to moderate it"));
    set_flags(&t, &["modEnabled"], false).await;
    assert_eq!(refusal(run(&t, "mod.action", BOB, kick.clone()).await), "The Moderation module is off. Turn it on to do that.");
    set_flags(&t, &["modEnabled"], true).await;
    assert_eq!(
        refusal(run(&t, "mod.action", CAROL, kick.clone()).await),
        "You need moderator rights in this server (Kick, Ban or Timeout Members, or a moderator role) to do that."
    );
    assert_eq!(refusal(run(&t, "mod.clearWarnings", ALICE, json!({ "userId": BOB })).await), "Only administrators (Manage Server) can do that.");
    // /case refusals before anything is written.
    assert_eq!(refusal(run(&t, "mod.editCase", ALICE, json!({ "caseId": 0, "reason": "x" })).await), "That case does not exist.");
    assert_eq!(refusal(run(&t, "mod.editCase", ALICE, json!({ "caseId": 1, "reason": "  " })).await), "The reason can't be empty.");
    assert_eq!(refusal(run(&t, "mod.editCase", ALICE, json!({ "caseId": 1, "reason": null })).await), "The reason can't be empty.");
    assert_eq!(
        refusal(run(&t, "mod.editCase", ALICE, json!({ "caseId": 1, "proofUrl": "javascript:alert(1)" })).await),
        "Proof must be an http:// or https:// link (max 1000 characters)."
    );
    assert_eq!(refusal(run(&t, "mod.editCase", ALICE, json!({ "caseId": 1 })).await), "Nothing to change.");
    t.settings_store.set_available(false);
    t.settings().invalidate_settings(G).await;
    assert_eq!(refusal(run(&t, "mod.action", BOB, kick).await), "The bot couldn't load this server's settings. Try again in a moment.");
}

#[tokio::test]
async fn xp_adjust_is_for_administrators_and_the_bot_owner() {
    let t = app_with(AppConfig { bot_owner_id: Some("900000000000000055".into()), ..Default::default() });
    let give = json!({ "op": "give", "userId": ALICE, "amount": 1 });
    assert_eq!(refusal(run(&t, "xp.adjust", ALICE, give.clone()).await), "Only administrators (Manage Server) can change XP.");
    assert_eq!(refusal(run(&t, "xp.adjust", "900000000000000077", give.clone()).await), "You are no longer a member of this server.");
    assert_eq!(refusal(run(&t, "xp.adjust", BOB, json!({ "op": "give", "userId": ALICE, "amount": 0 })).await), "The amount must be between 1 and 1,000,000 XP.");
    assert_eq!(refusal(run(&t, "xp.adjust", BOB, json!({ "op": "double", "userId": ALICE })).await), "Unknown XP change.");
    assert_eq!(refusal(run(&t, "xp.adjust", BOB, json!({ "op": "reset", "userId": "12" })).await), "Pick a member, or paste a user ID.");
    // Bots and users who are not members cannot be given XP (checked before any write).
    assert_eq!(refusal(run(&t, "xp.adjust", BOB, json!({ "op": "give", "userId": TEST_BOT_ID, "amount": 5 })).await), "Bots don't earn XP.");
    assert_eq!(
        refusal(run(&t, "xp.adjust", "900000000000000055", json!({ "op": "give", "userId": "900000000000000077", "amount": 5 })).await),
        "That user isn't in this server."
    );
    assert_eq!(
        refusal(run(&t, "xp.adjust", BOB, json!({ "op": "set", "userId": "900000000000000077", "amount": 5 })).await),
        "That user isn't in this server."
    );
    t.settings_store.set_available(false);
    t.settings().invalidate_settings(G).await;
    assert_eq!(
        refusal(run(&t, "xp.adjust", BOB, json!({ "op": "reset", "userId": ALICE })).await),
        "The bot couldn't load this server's settings. Try again in a moment."
    );
}

#[tokio::test]
async fn giveaway_and_poll_tasks_have_the_routers_gate_and_the_commands_level() {
    let t = app();
    let start = json!({ "prize": "Nitro", "channelId": CH_TEXT, "duration": "1h" });
    set_flags(&t, &["giveawaysEnabled", "pollsEnabled"], false).await;
    for (kind, label) in [("giveaway.start", "🎉 Giveaways"), ("poll.start", "📊 Polls")] {
        assert_eq!(
            refusal(run(&t, kind, BOB, start.clone()).await),
            format!("The {label} module is turned off on this server. Turn it on first.")
        );
    }
    set_flags(&t, &["giveawaysEnabled", "pollsEnabled"], true).await;
    assert_eq!(refusal(run(&t, "giveaway.start", CAROL, start.clone()).await), "You need to be a moderator of this server to manage giveaways.");
    assert_eq!(refusal(run(&t, "poll.end", CAROL, json!({ "pollId": "x" })).await), "You need to be a moderator of this server to manage polls.");
    assert_eq!(refusal(run(&t, "giveaway.end", "900000000000000077", json!({})).await), "You are no longer a member of this server.");
    // Ids are checked before the database is asked.
    assert_eq!(refusal(run(&t, "giveaway.end", ALICE, json!({ "giveawayId": "nope" })).await), "That giveaway no longer exists.");
    assert_eq!(refusal(run(&t, "poll.delete", ALICE, json!({ "pollId": 5 })).await), "That poll no longer exists.");
    // giveaway.start re-reads the form before anything else is looked up.
    let invalid = "Those giveaway details were not valid. Reload the page and try again.";
    let with = |patch: Value| {
        let mut p = start.clone();
        for (k, v) in patch.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, json!({ "channelId": CH_TEXT })).await), invalid);
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "prize": "  " }))).await), "The prize cannot be empty.");
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "prize": "p".repeat(257) }))).await), "The prize can be at most 256 characters.");
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "channelId": "x" }))).await), invalid);
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "winnerCount": 0 }))).await), "Pick between 1 and 90 winners.");
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "winnerCount": null }))).await), "Pick between 1 and 90 winners.");
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "requiredRoleIds": ["x"] }))).await), invalid);
    let w = json!({ "roleId": ROLE_MOD, "weight": 3 });
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "roleWeights": [w, w] }))).await), "Each role can have bonus odds only once.");
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "roleWeights": [{ "roleId": ROLE_MOD, "weight": 1 }] }))).await), invalid);
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "xpPrize": 0 }))).await), invalid);
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "embedColor": 16777216 }))).await), invalid);
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "endsAt": 5 }))).await), invalid);
    // Channel checks, for the bot and for the dashboard user.
    assert_eq!(
        refusal(run(&t, "giveaway.start", ALICE, with(json!({ "channelId": CH_CATEGORY }))).await),
        "Giveaways can be posted in text or announcement channels only."
    );
    assert_eq!(refusal(run(&t, "giveaway.start", ALICE, with(json!({ "channelId": CH_PRIVATE }))).await), "You can't send messages in #staff.");
    // Then the length.
    assert_eq!(
        refusal(run(&t, "giveaway.start", BOB, with(json!({ "duration": "1h", "endsAt": "2030-01-01T00:00:00Z" }))).await),
        "Use either the length or the end time, not both."
    );
    assert_eq!(refusal(run(&t, "giveaway.start", BOB, with(json!({ "duration": "soon" }))).await), "Use a length like 30m, 2h, 3d or 1w.");
    assert_eq!(refusal(run(&t, "giveaway.start", BOB, with(json!({ "duration": null }))).await), "Set how long the giveaway runs, or when it ends.");
    assert_eq!(refusal(run(&t, "giveaway.start", BOB, with(json!({ "duration": "10s" }))).await), "A giveaway must run for at least 1 minute.");
    assert_eq!(refusal(run(&t, "giveaway.start", BOB, with(json!({ "duration": "100d" }))).await), "A giveaway can run for at most 90 days.");
    assert_eq!(
        refusal(run(&t, "giveaway.start", BOB, with(json!({ "duration": null, "endsAt": "2001-01-01T00:00:00Z" }))).await),
        "That end time is in the past (server timezone: UTC)."
    );
    // Roles must exist and must not be @everyone.
    assert_eq!(
        refusal(run(&t, "giveaway.start", BOB, with(json!({ "requiredRoleIds": [G] }))).await),
        "Choose a specific role for the required roles, not @everyone."
    );
    assert_eq!(
        refusal(run(&t, "giveaway.start", BOB, with(json!({ "deniedRoleIds": ["910000000000000099"] }))).await),
        "A role chosen for the blocked roles no longer exists. Pick another one."
    );
    assert_eq!(
        refusal(run(&t, "giveaway.start", BOB, with(json!({ "requiredRoleIds": [ROLE_MOD], "deniedRoleIds": [ROLE_MOD] }))).await),
        "The same role cannot be both required and blocked."
    );

    // poll.start re-reads the form.
    let poll = json!({ "question": "Pizza?", "options": ["Yes", "No"], "channelId": CH_TEXT });
    let pwith = |patch: Value| {
        let mut p = poll.clone();
        for (k, v) in patch.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    let pinvalid = "Those poll details were not valid. Reload the page and try again.";
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "options": ["a\nb", "c"] }))).await), pinvalid);
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "maxChoices": 0 }))).await), "Pick between 1 and 20 choices per member.");
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "hideResults": "yes" }))).await), pinvalid);
    assert_eq!(
        refusal(run(&t, "poll.start", ALICE, pwith(json!({ "question": " ", "options": ["one"] }))).await),
        "That poll is not valid. The question is empty. Add at least 2 options (one per line)."
    );
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "duration": "1s" }))).await), "Polls must run for at least 1 minute.");
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "duration": "99d" }))).await), "Polls can run for at most 30 days.");
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "duration": "later" }))).await), "Use a length like 30m, 2h, 1d or 1w.");
    assert_eq!(refusal(run(&t, "poll.start", ALICE, pwith(json!({ "deniedRoleIds": [G] }))).await), "You can't block @everyone, or nobody could vote.");
    assert_eq!(
        refusal(run(&t, "poll.start", ALICE, pwith(json!({ "allowedRoleIds": [ROLE_MOD], "deniedRoleIds": [ROLE_MOD] }))).await),
        "The allowed roles and the blocked roles must be different."
    );
    assert_eq!(
        refusal(run(&t, "poll.start", ALICE, pwith(json!({ "allowedRoleIds": ["910000000000000099"] }))).await),
        "A role you picked no longer exists. Pick another one."
    );
    assert_eq!(
        refusal(run(&t, "poll.start", ALICE, pwith(json!({ "channelId": "920000000000000099" }))).await),
        "That channel no longer exists. Pick another one."
    );
    assert_eq!(
        refusal(run(&t, "poll.start", ALICE, pwith(json!({ "channelId": CH_PRIVATE }))).await),
        "You need permission to view and send messages in #staff."
    );
}

#[tokio::test]
async fn panel_tasks_need_the_admin_level_and_a_valid_id() {
    let t = app();
    for (kind, text) in [
        ("rr.publish", "You need the Admin level (Manage Server) to change reaction role panels."),
        ("rr.repost", "You need the Admin level (Manage Server) to change reaction role panels."),
        ("rr.delete", "You need the Admin level (Manage Server) to change reaction role panels."),
        ("rr.roleTemplate", "You need the Admin level (Manage Server) to change reaction role panels."),
        ("ticket.check", "You need the Admin level (Manage Server) to change ticket panels."),
        ("ticket.publish", "You need the Admin level (Manage Server) to change ticket panels."),
        ("ticket.refresh", "You need the Admin level (Manage Server) to change ticket panels."),
        ("ticket.delete", "You need the Admin level (Manage Server) to change ticket panels."),
    ] {
        assert_eq!(refusal(run(&t, kind, ALICE, json!({})).await), text, "{kind}");
        assert_eq!(refusal(run(&t, kind, "900000000000000077", json!({})).await), "You are no longer a member of this server.", "{kind}");
    }
    assert_eq!(refusal(run(&t, "rr.publish", BOB, json!({ "groupId": "g1", "mode": "other" })).await), "That request is not valid.");
    assert_eq!(refusal(run(&t, "rr.publish", BOB, json!({ "groupId": "bad id!", "mode": "post" })).await), "That panel is not valid.");
    assert_eq!(refusal(run(&t, "rr.delete", BOB, json!({ "groupId": 5 })).await), "That panel is not valid.");
    assert_eq!(refusal(run(&t, "rr.roleTemplate", BOB, json!({ "groupId": "g1", "template": "nope" })).await), "That template is not valid.");
    assert_eq!(refusal(run(&t, "ticket.publish", BOB, json!({ "panelId": "x".repeat(41) })).await), "That panel is not valid.");
}

#[tokio::test]
async fn ticket_check_reports_the_bots_permissions_in_categories_and_the_transcript_channel() {
    let t = app();
    let r = run(&t, "ticket.check", BOB, json!({ "categoryIds": [CH_CATEGORY, CH_CATEGORY] })).await.unwrap();
    assert_eq!((r.message.as_str(), r.data), ("The bot can use those channels.", None));
    assert_eq!(refusal(run(&t, "ticket.check", BOB, json!({ "categoryIds": [CH_TEXT] })).await), "#general is not a category.");
    assert_eq!(refusal(run(&t, "ticket.check", BOB, json!({ "categoryIds": ["x"] })).await), "A category is not valid.");
    assert_eq!(
        refusal(run(&t, "ticket.check", BOB, json!({ "categoryIds": ["920000000000000099"] })).await),
        "That channel does not exist in this server any more. Pick another one."
    );
    assert_eq!(
        refusal(run(&t, "ticket.check", BOB, json!({ "transcriptChannelId": CH_CATEGORY })).await),
        "Transcripts can only be posted in text or announcement channels."
    );
    assert_eq!(refusal(run(&t, "ticket.check", BOB, json!({ "transcriptChannelId": CH_TEXT })).await), "I need Attach Files in #general.");
    assert_eq!(refusal(run(&t, "ticket.check", BOB, json!({ "transcriptChannelId": 5 })).await), "The transcript channel is not valid.");
    // No list: nothing to check. Only the first 28 ids are looked at.
    assert_eq!(run(&t, "ticket.check", BOB, json!({})).await.unwrap().message, "The bot can use those channels.");
    let mut ids: Vec<Value> = vec![json!(CH_CATEGORY); 28];
    ids.push(json!("not checked"));
    assert!(run(&t, "ticket.check", BOB, json!({ "categoryIds": ids })).await.is_ok());

    // A category without Manage Roles for the bot.
    let g = guild().channel("920000000000000005", 4, "Locked", &[(ROLE_BOT, false, Permissions::empty(), Permissions::MANAGE_ROLES)]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[g]);
    assert_eq!(
        refusal(run(&t, "ticket.check", BOB, json!({ "categoryIds": ["920000000000000005"] })).await),
        "I need Manage Roles in the \"Locked\" category."
    );
}

#[test]
fn small_helpers_follow_the_js() {
    assert!(util::is_snowflake("12345678901234567") && !util::is_snowflake("1234567890123456") && !util::is_snowflake("123456789012345678901"));
    assert_eq!(util::js_integer(Some(&json!(5.0))), Some(5.0));
    assert_eq!(util::js_integer(Some(&json!("5"))), None);
    assert_eq!(util::replace_channel_mentions("a <#123> b <#x> <#45>", |id| format!("#{id}")), "a #123 b <#x> #45");
    assert!(feat_dashboard_tasks::giveaways::is_cuid("cm0abcdefghijklmnopqrstu"));
    assert!(!feat_dashboard_tasks::giveaways::is_cuid("cm0ABCDEFGHIJKLMNOPQRSTU"));
    assert_eq!(feat_dashboard_tasks::giveaways::plain("**I** can't see <#1>.", Some("news")), "I can't see #news.");
    assert_eq!(index::clamp_message(&"é".repeat(600)).chars().count(), 500);
    let at = chrono::DateTime::parse_from_rfc3339("2026-05-02T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
    assert_eq!(util::utc(at), "2026-05-02 12:00 UTC");
    let end = feat_dashboard_tasks::giveaways::end_time(None, Some("2026-05-03T12:00:00.000Z"), "UTC", at).unwrap();
    assert_eq!(end.timestamp_millis() - at.timestamp_millis(), 86_400_000);
    assert_eq!(
        feat_dashboard_tasks::giveaways::end_time(None, Some("not a date"), "Europe/Paris", at).unwrap_err().0,
        "I couldn't understand that. Use a length like `30m`, `2h` or `3d`, or an end time like `2026-12-24 18:00` (server timezone: Europe/Paris)."
    );
}

// ── Giveaway and poll texts against the JS (stubbed service results) ─────────────

fn giveaway_error(r: &Value) -> feat_giveaways_polls::giveaway_service::GiveawayError {
    use feat_giveaways_polls::discord_ops::DiscordError;
    use feat_giveaways_polls::giveaway_service::GiveawayError as E;
    let strings = |v: &Value| v.as_array().map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect()).unwrap_or_default();
    match r["code"].as_str().unwrap() {
        "LIMIT" => E::Limit,
        "SEND_FAILED" => E::SendFailed(DiscordError::code(50013)),
        "DB_ERROR" => E::DbError("x".into()),
        "NOT_FOUND" => E::NotFound,
        "NOT_ACTIVE" => E::NotActive,
        "RESOLVE_FAILED" => E::ResolveFailed,
        "DRAFTED" => E::Drafted,
        "NO_ACCESS" => E::NoAccess { missing: strings(&r["missing"]), reason: r["reason"].as_str().map(String::from) },
        "NOT_ENDED" => E::NotEnded,
        "NO_ENTRANTS" => E::NoEntrants,
        "CONFLICT" => E::Conflict,
        "NOT_DRAFT" => E::NotDraft,
        other => panic!("unknown code {other}"),
    }
}

fn outcome_of(case: &Value) -> Result<String, String> {
    match case["message"].as_str() {
        Some(m) => Ok(m.to_string()),
        None => Err(case["error"].as_str().unwrap().to_string()),
    }
}

#[test]
fn giveaway_texts_match_the_js() {
    use feat_dashboard_tasks::giveaways::{end_text, name_list_with, reroll_text, restart_failure_text, start_failure_text};
    use feat_giveaways_polls::giveaway_service::{Ended, Rerolled};
    let g = golden();
    let names_map = g["names"].clone();
    let names = |ids: &[String]| name_list_with(ids, &|id: &str| names_map[id].as_str().map(String::from));
    let strings = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect()).unwrap_or_default() };
    let texts = &g["giveawayTexts"];
    for case in texts["end"].as_array().unwrap() {
        let r = &case["result"];
        let res = if r["ok"] == json!(true) {
            Ok(Ended { winner_ids: strings(&r["winnerIds"]), entry_count: 0, announced: r["announced"] != json!(false), pay_warnings: strings(&r["payWarnings"]) })
        } else {
            Err(giveaway_error(r))
        };
        assert_eq!(end_text(case["prize"].as_str().unwrap(), &res, &names), outcome_of(case), "{r}");
    }
    for case in texts["reroll"].as_array().unwrap() {
        let r = &case["result"];
        let res = if r["ok"] == json!(true) {
            Ok(Rerolled { new_winner_ids: strings(&r["newWinnerIds"]), announced: r["announced"] != json!(false), pay_warnings: strings(&r["payWarnings"]) })
        } else {
            Err(giveaway_error(r))
        };
        assert_eq!(reroll_text(case["prize"].as_str().unwrap(), &res, &names), outcome_of(case), "{r}");
    }
    let now = chrono::DateTime::parse_from_rfc3339(g["now"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
    for (key, f) in [("start", start_failure_text as fn(&_, &str) -> String), ("restart", restart_failure_text)] {
        for case in texts[key].as_array().unwrap() {
            let r = &case["result"];
            if r["ok"] == json!(true) {
                let prize = bot_util::text::clamp_text(case["prize"].as_str().unwrap(), 80);
                let want = if key == "start" {
                    format!("Started the giveaway for \"{prize}\" in #general. It ends {}.", util::utc(now + chrono::Duration::hours(1)))
                } else {
                    format!("\"{prize}\" is live again in #general and ends {}. Existing entries were kept.", util::utc(now + chrono::Duration::hours(2)))
                };
                assert_eq!(case["message"].as_str().unwrap(), want);
                continue;
            }
            assert_eq!(f(&giveaway_error(r), "general"), case["error"].as_str().unwrap(), "{key} {r}");
        }
    }
}

#[test]
fn poll_texts_match_the_js() {
    use feat_dashboard_tasks::polls::{end_text, post_failure_text, start_message};
    use feat_giveaways_polls::discord_ops::DiscordError;
    use feat_giveaways_polls::poll_service::{EndPollError, PostError};
    let g = golden();
    let texts = &g["pollTexts"];
    for case in texts["start"].as_array().unwrap() {
        let r = &case["result"];
        let question = case["question"].as_str().unwrap();
        let requested = case["payload"]["maxChoices"].as_i64().unwrap();
        let got = if r["ok"] == json!(true) {
            let ends_at = r["poll"]["endsAt"].as_str().map(|s| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc));
            Ok(start_message(question, "general", ends_at, requested.min(3), requested, r["timerFailed"] == json!(true)))
        } else {
            let e = match r["code"].as_str().unwrap() {
                "LIMIT" => PostError::Limit,
                "SEND" => PostError::Send(match r["error"]["code"].as_u64() {
                    Some(c) => DiscordError::code(c),
                    None => DiscordError::new(None, "x"),
                }),
                _ => PostError::Save("x".into()),
            };
            Err(post_failure_text(&e, "general"))
        };
        assert_eq!(got, outcome_of(case), "{r}");
    }
    for case in texts["end"].as_array().unwrap() {
        let r = &case["result"];
        let res = match r["code"].as_str() {
            None => Ok(r["posted"] == json!(true)),
            Some("ALREADY_ENDED") => Err(EndPollError::AlreadyEnded),
            Some(_) => Err(EndPollError::NotFound),
        };
        assert_eq!(end_text(case["question"].as_str().unwrap(), res, "general"), outcome_of(case), "{r}");
    }
}

#[tokio::test]
async fn access_channels_fails_closed_when_the_guild_cannot_be_read() {
    // The channel and the member come over REST, but the guild is not cached: nothing can be resolved, so every need
    // counts as missing (the JS permissionsFor failure path) instead of passing.
    let rest = bot_core::testing::FakeRest::new();
    rest.on(
        "GET",
        &format!("/channels/{CH_TEXT}"),
        200,
        json!({ "id": CH_TEXT, "type": 0, "guild_id": G, "name": "general", "position": 0, "permission_overwrites": [], "nsfw": false }),
    );
    rest.on(
        "GET",
        &format!("/guilds/{G}/members/{CAROL}"),
        200,
        json!({
            "user": { "id": CAROL, "username": "carol", "discriminator": "0", "avatar": null }, "roles": [],
            "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
        }),
    );
    let t = TestApp::new().with_guilds([G]).with_rest(&rest);
    let checks = json!({ "checks": [{ "channelId": CH_TEXT, "needs": ["SendMessages", "CreatePublicThreads"] }] });
    let r = run(&t, "access.channels", CAROL, checks).await.unwrap();
    assert_eq!(r.message, "You need Send Messages, Create Public Threads in #the channel to do that.");
}
