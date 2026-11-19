//! src/dashboard/tasks.js ported to bins/dashboard `tasks`: the "dashboard side" cases of test/dashboard-tasks.test.js
//! against the real ScheduledJob table and Redis, and the protocol with the bot in both languages:
//! the Rust dashboard queues and the JS bot's handler runs it; the JS dashboard queues and the Rust bot
//! (feat-dashboard-tasks) runs it, and both dashboards read the same outcome.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use dash_collections::{TaskOpts, TaskRunner, TaskStatus};
use dashboard::tasks::{OFFLINE_TEXT, PENDING_TEXT, TaskClient, result_json};
use feat_dashboard_tasks::shared::{TASK_JOB_TYPE, task_result_key};
use serde_json::{Value, json};

/// The row the runner queued for `guild` (the one PENDING dashboard.task).
async fn queued(app: &bot_core::App, guild: &str) -> Option<(String, String, Value, i32, String)> {
    sqlx::query_as::<_, (String, String, Value, i32, String)>(
        r#"SELECT "id", "type", "payload", "maxAttempts", "dedupeKey" FROM "ScheduledJob" WHERE "guildId" = $1 AND "type" = 'dashboard.task' ORDER BY "createdAt" DESC LIMIT 1"#,
    )
    .bind(guild)
    .fetch_optional(app.db())
    .await
    .unwrap()
}

/// A runner whose `sleep` runs `script` (once, on the first poll) against the database and Redis: the fakes of the JS
/// test, played on the real tables.
fn scripted(app: &bot_core::App, wait_ms: u64, script: impl Fn(bot_core::App, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync + 'static) -> (TaskClient, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let (a, s2) = (app.clone(), seen.clone());
    let script = Arc::new(script);
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client = TaskClient::new(app.clone()).wait_ms(wait_ms).poll_ms(1).sleep(Arc::new(move |_ms| {
        let (a, s2, script, ran) = (a.clone(), s2.clone(), script.clone(), ran.clone());
        Box::pin(async move {
            if ran.swap(true, std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
                return;
            }
            let row: (String, String, Value, i32, String) = sqlx::query_as(
                r#"SELECT "id", "type", "payload", "maxAttempts", "dedupeKey" FROM "ScheduledJob" WHERE "type" = 'dashboard.task' AND "guildId" = ANY($1) ORDER BY "createdAt" DESC LIMIT 1"#,
            )
            .bind(vec!["940000000000000001".to_string(), "940000000000000002".to_string(), "940000000000000003".to_string(), "940000000000000004".to_string(), "940000000000000005".to_string()])
            .fetch_one(a.db())
            .await
            .unwrap();
            s2.lock().unwrap().push(json!({ "id": row.0, "type": row.1, "payload": row.2, "maxAttempts": row.3, "dedupeKey": row.4 }));
            script(a, row.0).await;
        })
    }));
    (client, seen)
}

async fn set_status(app: &bot_core::App, id: &str, status: &str, last_error: Option<&str>) {
    sqlx::query(sqlx::AssertSqlSafe(format!(r#"UPDATE "ScheduledJob" SET "status" = '{status}', "lastError" = $2 WHERE "id" = $1"#)))
        .bind(id)
        .bind(last_error)
        .execute(app.db())
        .await
        .unwrap();
}

async fn row_status(app: &bot_core::App, id: &str) -> Option<String> {
    sqlx::query_scalar(r#"SELECT "status"::text FROM "ScheduledJob" WHERE "id" = $1"#).bind(id).fetch_optional(app.db()).await.unwrap()
}

#[tokio::test]
async fn the_dashboard_side_cases_of_the_js_tests() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let g = "940000000000000001";
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
    app.settings().invalidate_settings(g).await;

    // queues one attempt for the guild with the actor, and reports the bot's message
    let (client, seen) = scripted(&app, 5_000, |a, id| {
        Box::pin(async move {
            sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "id" = $1"#).bind(&id).execute(a.db()).await.unwrap();
            a.cache().set(&task_result_key(&id), &json!({ "ok": true, "message": "Banned alice (case #4).", "data": { "caseId": 4 } }), 900).await;
        })
    });
    let r = client.run(g, "mod.action", json!({ "action": "BAN" }), TaskOpts { actor_id: ALICE.into(), wait_ms: None }).await.unwrap();
    assert_eq!(result_json(&r), json!({ "status": "done", "message": "Banned alice (case #4).", "data": { "caseId": 4 } }));
    let q = seen.lock().unwrap()[0].clone();
    assert_eq!(q["type"], json!(TASK_JOB_TYPE));
    assert_eq!(q["maxAttempts"], json!(1));
    let key = q["dedupeKey"].as_str().unwrap();
    assert!(key.len() == "dashboard.task:".len() + 36 && key.starts_with("dashboard.task:"), "{key}");
    assert!(key["dashboard.task:".len()..].chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{key}");
    // Key order as the JS writes it: kind, actorId, payload.
    assert_eq!(serde_json::to_string(&q["payload"]).unwrap(), r#"{"kind":"mod.action","actorId":"900000000000000003","payload":{"action":"BAN"}}"#);

    // a finished task without a stored message (Redis down) still reads as done
    let (client, _) = scripted(&app, 5_000, |a, id| {
        Box::pin(async move {
            sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "id" = $1"#).bind(&id).execute(a.db()).await.unwrap();
        })
    });
    let r = client.run_task(g, "xp.adjust", json!({}), ALICE, None).await.unwrap();
    assert_eq!(result_json(&r), json!({ "status": "done", "message": "Done.", "data": null }));

    // a refusal comes back with the bot's reason (and an empty reason reads as the default)
    let (client, _) = scripted(&app, 5_000, |a, id| Box::pin(async move { set_status(&a, &id, "FAILED", Some("That member is above the bot.")).await }));
    let r = client.run_task(g, "mod.action", json!({}), ALICE, None).await.unwrap();
    assert_eq!(result_json(&r), json!({ "status": "failed", "message": "That member is above the bot.", "data": null }));
    let (client, _) = scripted(&app, 5_000, |a, id| Box::pin(async move { set_status(&a, &id, "CANCELLED", Some("")).await }));
    let r = client.run_task(g, "mod.action", json!({}), ALICE, None).await.unwrap();
    assert_eq!(result_json(&r), json!({ "status": "failed", "message": "The bot could not do that.", "data": null }));

    // a task nobody picks up is cancelled and reported as offline
    let (client, seen) = scripted(&app, 50, |_, _| Box::pin(async {}));
    let (r, lines) = bot_util::logger::capture(client.run_task(g, "mod.action", json!({}), ALICE, None)).await;
    let r = r.unwrap();
    assert_eq!(r.status, TaskStatus::Offline);
    assert_eq!(r.message, OFFLINE_TEXT);
    let id = seen.lock().unwrap()[0]["id"].as_str().unwrap().to_string();
    assert_eq!(row_status(&app, &id).await, None, "the queued job was removed");
    assert!(lines.iter().any(|(_, l)| l.contains(&format!("Dashboard: task mod.action in guild {g} was not picked up in 0 s; cancelled"))), "{lines:?}");

    // a task still running when the wait ends is left alone and reported as pending
    let (client, seen) = scripted(&app, 50, |a, id| Box::pin(async move { set_status(&a, &id, "RUNNING", None).await }));
    let r = client.run_task(g, "giveaway.start", json!({}), ALICE, None).await.unwrap();
    assert_eq!(r.status, TaskStatus::Pending);
    assert_eq!(r.message, PENDING_TEXT);
    let id = seen.lock().unwrap()[0]["id"].as_str().unwrap().to_string();
    assert_eq!(row_status(&app, &id).await.as_deref(), Some("RUNNING"));

    // a malformed kind is refused before anything is queued
    let before = queued(&app, g).await.map(|r| r.0);
    let err = client.run_task(g, "DROP TABLE", json!({}), ALICE, None).await.unwrap_err();
    assert_eq!(err.to_string(), "invalid dashboard task kind DROP TABLE");
    assert_eq!(queued(&app, g).await.map(|r| r.0), before);

    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
}

#[tokio::test]
async fn a_task_the_rust_dashboard_queues_is_run_by_the_js_bot_and_read_back() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let g = "940000000000000002";
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();

    let client = TaskClient::new(app.clone()).wait_ms(30_000).poll_ms(50);
    let payload = json!({ "checks": [{ "channelId": CH_TEXT }] });
    let run = tokio::spawn({
        let (client, payload) = (client.clone(), payload.clone());
        async move { client.run_task(g, "access.channels", payload, OWNER, None).await }
    });
    let mut row = None;
    for _ in 0..200 {
        row = queued(&app, g).await;
        if row.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (id, ..) = row.expect("the Rust dashboard queued a job");
    // The JS bot reads the Rust-queued row and runs it (runDashboardTask), storing its result in Redis.
    let js = node_async(TASKS_JS, vec!["execute".into(), id.clone(), OWNER.into()]).await;
    assert_eq!(js["error"], Value::Null, "{js}");
    assert_eq!(js["row"]["type"], json!(TASK_JOB_TYPE));
    assert_eq!(js["row"]["maxAttempts"], json!(1));
    assert_eq!(js["row"]["payload"], json!({ "kind": "access.channels", "actorId": OWNER, "payload": payload }));
    let want = json!({ "ok": true, "message": "The bot owner is not checked.", "data": { "problems": [] } });
    assert_eq!(js["stored"], want);
    // The JS scheduler deletes a job that succeeded; the waiting Rust dashboard then reads the JS bot's result.
    sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "id" = $1"#).bind(&id).execute(app.db()).await.unwrap();
    let r = run.await.unwrap().unwrap();
    assert_eq!(result_json(&r), json!({ "status": "done", "message": "The bot owner is not checked.", "data": { "problems": [] } }));
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
}

#[tokio::test]
async fn a_task_the_js_dashboard_queues_is_run_by_the_rust_bot_and_both_dashboards_read_it() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let g = "940000000000000003";
    let bot = Bot::new(&db, &redis, &[g]).await;
    bot.reset(&[g]).await;
    let app = dashboard_app(&db, &redis).await;
    let client = TaskClient::new(app.clone()).wait_ms(0);

    // Success with data, a refusal, and an unknown kind.
    let cases = [
        ("xp.adjust", BOB, json!({ "op": "give", "userId": ALICE, "amount": 150 })),
        ("xp.adjust", ALICE, json!({ "op": "give", "userId": ALICE, "amount": 1 })),
        ("nope.nope", ALICE, json!({})),
    ];
    for (kind, actor, payload) in cases {
        let q = node(TASKS_JS, &["queue", g, kind, actor, &payload.to_string()]);
        let id = q["id"].as_str().unwrap().to_string();
        assert_eq!(q["row"]["payload"], json!({ "kind": kind, "actorId": actor, "payload": payload }));
        assert_eq!(q["row"]["type"], json!(TASK_JOB_TYPE));
        assert_eq!(q["row"]["maxAttempts"], json!(1));
        assert_eq!(bot.drain().await, 1, "{kind}");
        let rust = client.outcome(&id).await.unwrap().expect("finished");
        let js = node(TASKS_JS, &["outcome", g, &id, kind, actor]);
        assert_eq!(result_json(&rust), js, "{kind}");
    }
    let rust = client.outcome(node(TASKS_JS, &["queue", g, "xp.adjust", BOB, "{}"])["id"].as_str().unwrap()).await.unwrap();
    assert!(rust.is_none(), "a queued job is not finished yet");
    bot.reset(&[g]).await;
}

#[tokio::test]
async fn the_whole_rust_runner_against_the_rust_bot() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let g = "940000000000000004";
    let bot = Bot::new(&db, &redis, &[g]).await;
    bot.reset(&[g]).await;
    let app = dashboard_app(&db, &redis).await;
    let looping = bot.spawn_loop();
    let client = TaskClient::new(app.clone()).wait_ms(10_000).poll_ms(50);
    let r = client.run_task(g, "xp.adjust", json!({ "op": "give", "userId": ALICE, "amount": 150 }), BOB, None).await.unwrap();
    looping.abort();
    assert_eq!(r.status, TaskStatus::Done, "{r:?}");
    assert!(r.message.starts_with(&format!("Gave 150 XP to user{ALICE}. Total: 150 XP")), "{}", r.message);
    assert_eq!(r.data, Some(json!({ "userId": ALICE, "xp": 150, "level": feat_levels::xp_service::level_from_xp(150.0) })));
    bot.reset(&[g]).await;
}

