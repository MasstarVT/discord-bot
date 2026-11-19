//! Database tests of feat-dashboard-tasks (TEST_DATABASE_URL and TEST_REDIS_URL; they return early without them):
//! the protocol with the JS dashboard in both directions (src/dashboard/tasks.js queues, the Rust bot runs, the JS
//! dashboard reads; Rust queues, the JS bot runs, Rust reads), and every task area against the real services, with rows
//! written by the JS services and read back by Prisma (rust/fixtures/gen/feat-dashboard-tasks-js.mjs). Discord is the
//! cached fake guild plus bot-core's REST double.

use std::time::Duration;

use bot_config::JobType;
use bot_core::Registry;
use bot_core::scheduler::{CancelJobs, ScheduleJob};
use bot_core::testing::{DbTestApp, FakeGuild, FakeRest, RestCall, TEST_BOT_ID, database_url, redis_url, run_node};
use feat_dashboard_tasks::index::{self, TaskCtx, TaskOutcome, resolve_actor};
use feat_dashboard_tasks::shared::{self, TaskError, TaskResultValue};
use serde_json::{Value, json};
use serenity::all::Permissions;

const JS: &str = "rust/fixtures/gen/feat-dashboard-tasks-js.mjs";
const BOB: &str = "900000000000000002";
const ALICE: &str = "900000000000000003";
const CAROL: &str = "900000000000000004";
const OWNER: &str = "900000000000000099";
const ROLE_ADMIN: &str = "910000000000000001";
const ROLE_MOD: &str = "910000000000000002";
const ROLE_BOT: &str = "910000000000000003";
const CH_TEXT: &str = "920000000000000001";

/// The DB tests share the job table: a tick of one test must not claim another test's job, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn guild(g: &str) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .role(ROLE_ADMIN, 3, Permissions::MANAGE_GUILD)
        .role(ROLE_MOD, 2, Permissions::KICK_MEMBERS)
        .role(ROLE_BOT, 5, Permissions::ADMINISTRATOR)
        .member(TEST_BOT_ID, &[ROLE_BOT])
        .member(BOB, &[ROLE_ADMIN])
        .member(ALICE, &[ROLE_MOD])
        .member(CAROL, &[])
        .channel(CH_TEXT, 0, "general", &[])
}

fn message_json(id: &str, channel: &str) -> Value {
    json!({
        "id": id, "channel_id": channel, "content": "", "timestamp": "2026-05-02T12:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
        "pinned": false, "type": 0, "components": [],
        "author": { "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true },
    })
}

struct Env {
    t: DbTestApp,
    rest: FakeRest,
    g: &'static str,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl Env {
    async fn reset(&self) {
        self.t.reset_guilds(&[self.g]).await;
        self.t.app.scheduler().cancel_jobs(CancelJobs { guild_id: Some(self.g.to_string()), ..Default::default() }).await;
    }
}

async fn env(g: &'static str) -> Option<Env> {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else {
        eprintln!("skipped: TEST_DATABASE_URL or TEST_REDIS_URL not set");
        return None;
    };
    let lock = SERIAL.lock().await;
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([g]);
    let rest = FakeRest::new();
    // Discord answers a create with the full message; the double's echo copies a reply's partial message_reference,
    // which serenity cannot read, so creates are answered here.
    let next = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    rest.on_with("POST", "/channels/*/messages", move |call: &RestCall| {
        let seg: Vec<&str> = call.path.trim_matches('/').split('/').collect();
        let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut m = message_json(&format!("{}", 1_300_000_000_000_000_000u64 + n), seg[1]);
        if let Some(c) = call.body.get("content") {
            m["content"] = c.clone();
        }
        (200, m)
    });
    rest.on_with("GET", "/channels/*/messages/*", |call: &RestCall| {
        let seg: Vec<&str> = call.path.trim_matches('/').split('/').collect();
        (200, message_json(seg[3], seg[1]))
    });
    t.app.set_discord(rest.discord(&[guild(g).json()]));
    let mut reg = Registry::new();
    feat_dashboard_tasks::register(&mut reg);
    reg.install(&t.app);
    let e = Env { t, rest, g, _lock: lock };
    e.reset().await;
    e.t.app.settings().ensure_guild_settings(g).await.expect("settings row");
    Some(e)
}

async fn run(e: &Env, kind: &str, actor: &str, payload: Value) -> Result<TaskOutcome, String> {
    let task = index::tasks().get(kind).unwrap().clone();
    let actor = resolve_actor(&e.t.app, e.g, Some(&json!(actor))).await;
    let ctx = TaskCtx { app: e.t.app.clone(), guild_id: e.g.into(), actor, payload, task_id: "job1".into() };
    (task.run)(ctx).await.map_err(|err| match TaskError::of(&err) {
        Some(te) => te.0.clone(),
        None => format!("NOT A TASK ERROR: {err:#}"),
    })
}

fn node(args: &[&str]) -> Value {
    let v = run_node(JS, args);
    assert!(!v.get("error").is_some_and(Value::is_string), "node {args:?}: {v}");
    v
}

/// Ticks the interactive loop until nothing is left to claim (one job per guild per claim).
async fn drain(e: &Env) -> usize {
    let mut n = 0;
    loop {
        let k = e.t.app.scheduler().tick(&e.t.app, true).await;
        if k == 0 {
            return n;
        }
        n += k;
    }
}

// ── The protocol ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_js_dashboard_queues_the_rust_bot_runs_and_the_js_dashboard_reads_the_outcome() {
    let Some(e) = env("980000000000000001").await else { return };
    let g = e.g;

    // The whole JS runner (queue, poll, read) while the Rust bot's interactive loop runs the job.
    let payload = json!({ "checks": [{ "channelId": CH_TEXT }] }).to_string();
    let args: Vec<String> = ["runner", g, "access.channels", BOB, &payload, "10000"].iter().map(|s| s.to_string()).collect();
    let js = tokio::task::spawn_blocking(move || {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        run_node(JS, &a)
    });
    let mut claimed = 0;
    for _ in 0..100 {
        claimed += e.t.app.scheduler().tick(&e.t.app, true).await;
        if js.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let out = js.await.unwrap();
    assert!(claimed >= 1);
    assert_eq!(out, json!({ "status": "done", "message": "You can use those channels.", "data": { "problems": [] } }));

    // A refusal and an unknown kind come back as the FAILED row's lastError.
    for (kind, payload, message) in [
        ("access.channels", json!({ "checks": [] }), "Those channel checks were not valid. Reload the page and try again."),
        ("nope.nope", json!({}), "This bot version cannot do \"nope.nope\" yet. Update the bot."),
        ("xp.adjust", json!({ "op": "give", "userId": ALICE, "amount": 1 }), "Only administrators (Manage Server) can change XP."),
    ] {
        let q = node(&["queue", g, kind, ALICE, &payload.to_string()]);
        let id = q["id"].as_str().unwrap().to_string();
        assert_eq!(q["row"]["payload"], json!({ "kind": kind, "actorId": ALICE, "payload": payload }));
        assert_eq!(drain(&e).await, 1);
        let out = node(&["outcome", g, &id, kind, ALICE]);
        assert_eq!(out, json!({ "status": "failed", "message": message, "data": null }), "{kind}");
    }

    // A success that carries data, read back from Redis by the JS dashboard.
    let q = node(&["queue", g, "xp.adjust", BOB, &json!({ "op": "give", "userId": ALICE, "amount": 150 }).to_string()]);
    let id = q["id"].as_str().unwrap().to_string();
    assert_eq!(drain(&e).await, 1);
    let out = node(&["outcome", g, &id, "xp.adjust", BOB]);
    assert_eq!(out["status"], json!("done"));
    assert_eq!(out["data"], json!({ "userId": ALICE, "xp": 150, "level": feat_levels::xp_service::level_from_xp(150.0) }));
    assert!(out["message"].as_str().unwrap().starts_with(&format!("Gave 150 XP to user{ALICE}. Total: 150 XP")), "{out}");
    let stored: TaskResultValue = e.t.app.cache().get_as(&shared::task_result_key(&id)).await.expect("stored");
    assert_eq!(stored.message, out["message"].as_str().unwrap());
    e.reset().await;
}

#[tokio::test]
async fn a_task_queued_with_the_shared_constants_runs_under_the_js_bot_and_rust_reads_its_result() {
    let Some(e) = env("980000000000000002").await else { return };
    let g = e.g;
    let key = shared::task_dedupe_key("6f2a3c84-1f0e-4f9b-8d1e-6a1c0e7b9a55");
    let payload = json!({ "kind": "access.channels", "actorId": OWNER, "payload": { "checks": [{ "channelId": CH_TEXT }] } });
    let job = e
        .t
        .app
        .scheduler()
        .schedule_job(ScheduleJob::new(g, JobType::DashboardTask, chrono::Utc::now()).payload(payload.clone()).dedupe_key(key.clone()).max_attempts(1))
        .await
        .unwrap();
    let js = node(&["execute", &job.id, OWNER]);
    assert_eq!(js["row"]["type"], json!(shared::TASK_JOB_TYPE));
    assert_eq!(js["row"]["dedupeKey"], json!(key));
    assert_eq!(js["row"]["payload"], payload);
    assert_eq!(js["row"]["maxAttempts"], json!(1));
    assert_eq!(js["error"], Value::Null);
    let want = json!({ "ok": true, "message": "The bot owner is not checked.", "data": { "problems": [] } });
    assert_eq!(js["stored"], want);
    // What the JS bot stored is what the Rust side reads.
    let stored: TaskResultValue = e.t.app.cache().get_as(&shared::task_result_key(&job.id)).await.expect("readable");
    assert_eq!(serde_json::to_value(&stored).unwrap(), want);
    e.reset().await;
}

// ── Moderation ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn moderation_tasks_edit_and_pardon_cases_the_js_recorded() {
    let Some(e) = env("980000000000000003").await else { return };
    let g = e.g;
    node(&["warn", g, ALICE, BOB, "spam"]);
    node(&["warn", g, ALICE, BOB, "more spam"]);

    let r = run(&e, "mod.editCase", ALICE, json!({ "caseId": 1, "reason": " new\nreason ", "proofUrl": "https://x.test/p" })).await.unwrap();
    assert_eq!((r.message.as_str(), r.data), ("Case #1 updated.", Some(json!({ "caseId": 1 }))));
    assert_eq!(refusal(run(&e, "mod.editCase", ALICE, json!({ "caseId": 99, "reason": "x" })).await), "Case #99 doesn't exist in this server.");

    let r = run(&e, "mod.action", ALICE, json!({ "action": "WARN", "userId": CAROL, "reason": "rude" })).await.unwrap();
    assert!(r.message.starts_with(&format!("Warned user{CAROL} (case #3). Active warnings: 1.")), "{}", r.message);
    assert_eq!(r.data, Some(json!({ "caseId": 3, "action": "WARN", "userId": CAROL })));

    assert_eq!(refusal(run(&e, "mod.clearWarnings", ALICE, json!({ "userId": ALICE })).await), "Only administrators (Manage Server) can do that.");
    let r = run(&e, "mod.clearWarnings", BOB, json!({ "userId": ALICE, "reason": "fresh start" })).await.unwrap();
    assert_eq!(r.message, format!("Cleared 2 warning(s) for user{ALICE} (case #4)."));
    assert_eq!(r.data, Some(json!({ "cleared": 2, "caseId": 4, "userId": ALICE })));
    assert_eq!(refusal(run(&e, "mod.clearWarnings", BOB, json!({ "userId": ALICE })).await), format!("user{ALICE} has no active warnings."));

    let cases = node(&["cases", g, ALICE]);
    assert_eq!(
        cases,
        json!([
            { "caseId": 1, "type": "WARN", "reason": "new\nreason", "proofUrl": "https://x.test/p", "active": false, "moderatorId": BOB },
            { "caseId": 2, "type": "WARN", "reason": "more spam", "proofUrl": null, "active": false, "moderatorId": BOB },
            { "caseId": 4, "type": "NOTE", "reason": "[Pardon] Cleared 2 warning(s): fresh start", "proofUrl": null, "active": false, "moderatorId": BOB },
        ])
    );
    let carol = node(&["cases", g, CAROL]);
    assert_eq!(carol[0]["type"], json!("WARN"));
    assert_eq!(carol[0]["moderatorId"], json!(ALICE));
    e.reset().await;
}

fn refusal(r: Result<TaskOutcome, String>) -> String {
    match r {
        Ok(o) => panic!("expected a refusal, got {o:?}"),
        Err(e) => e,
    }
}

// ── XP ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn xp_adjust_changes_the_rows_the_js_reads() {
    let Some(e) = env("980000000000000004").await else { return };
    let g = e.g;
    node(&["xpset", g, ALICE, "500"]);
    let level = |xp: f64| feat_levels::xp_service::level_from_xp(xp);

    let r = run(&e, "xp.adjust", BOB, json!({ "op": "take", "userId": ALICE, "amount": 100 })).await.unwrap();
    // setXp does not touch the stored level (handleLevelChange owns it), so the change starts from the stored 0.
    let expected_change = format!(" Level 0 → {}.", level(400.0));
    assert_eq!(r.message, format!("Removed 100 XP from user{ALICE}. Total: 400 XP (level {}).{expected_change}", level(400.0)));
    assert_eq!(node(&["xp", g, ALICE])["xp"], json!(400));

    let r = run(&e, "xp.adjust", BOB, json!({ "op": "set", "userId": ALICE, "amount": 0 })).await.unwrap();
    assert!(r.message.starts_with(&format!("user{ALICE} now has 0 XP (level 0).")), "{}", r.message);
    let r = run(&e, "xp.adjust", BOB, json!({ "op": "reset", "userId": ALICE })).await.unwrap();
    assert_eq!(r.message, format!("Wiped XP and level for user{ALICE} (was level 0)."));
    assert_eq!(node(&["xp", g, ALICE]), Value::Null);
    assert_eq!(refusal(run(&e, "xp.adjust", BOB, json!({ "op": "reset", "userId": ALICE })).await), format!("user{ALICE} has no XP in this server."));
    assert_eq!(refusal(run(&e, "xp.adjust", BOB, json!({ "op": "take", "userId": CAROL, "amount": 5 })).await), format!("user{CAROL} has no XP in this server."));
    assert_eq!(refusal(run(&e, "xp.adjust", BOB, json!({ "op": "set", "userId": CAROL, "amount": 0 })).await), format!("user{CAROL} already has 0 XP."));

    let r = run(&e, "xp.adjust", BOB, json!({ "op": "give", "userId": CAROL, "amount": 1_000_000 })).await.unwrap();
    assert!(r.message.starts_with(&format!("Gave 1,000,000 XP to user{CAROL}. Total: 1,000,000 XP (level {}).", level(1_000_000.0))), "{}", r.message);
    assert_eq!(node(&["xp", g, CAROL])["xp"], json!(1_000_000));
    e.reset().await;
}

// ── Giveaways ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn giveaway_tasks_run_the_giveaway_service() {
    let Some(e) = env("980000000000000005").await else { return };
    let g = e.g;
    let r = run(&e, "giveaway.start", ALICE, json!({ "prize": "  Nitro  ", "channelId": CH_TEXT, "duration": "2h", "winnerCount": 2 })).await.unwrap();
    assert!(r.message.starts_with("Started the giveaway for \"Nitro\" in #general. It ends "), "{}", r.message);
    assert!(r.message.ends_with(" UTC."));
    let id = r.data.unwrap()["giveawayId"].as_str().unwrap().to_string();
    assert_eq!(e.rest.calls_to("POST", &format!("/channels/{CH_TEXT}/messages")).len(), 1);
    let js = node(&["giveaway", &id]);
    assert_eq!(js["giveaway"]["status"], json!("ACTIVE"));
    assert_eq!(js["giveaway"]["prize"], json!("Nitro"));
    assert_eq!(js["giveaway"]["winnerCount"], json!(2));
    assert_eq!(js["giveaway"]["hostId"], json!(ALICE));
    assert_eq!(js["job"]["type"], json!("giveaway.end"));

    // Bonus odds: the complete list, applied with the command's calls.
    let r = run(&e, "giveaway.odds", ALICE, json!({ "giveawayId": id, "roleWeights": [{ "roleId": ROLE_MOD, "weight": 3 }] })).await.unwrap();
    assert_eq!(r.message, "Updated the bonus odds of \"Nitro\".");
    let r = run(&e, "giveaway.odds", ALICE, json!({ "giveawayId": id, "roleWeights": [{ "roleId": ROLE_MOD, "weight": 3 }] })).await.unwrap();
    assert_eq!(r.message, "The bonus odds are already like that.");
    assert_eq!(node(&["giveaway", &id])["giveaway"]["roleWeights"], json!([{ "roleId": ROLE_MOD, "weight": 3 }]));
    assert_eq!(
        refusal(run(&e, "giveaway.restart", ALICE, json!({ "giveawayId": id })).await),
        "Only draft giveaways can be restarted. A giveaway becomes a draft when its message is deleted."
    );
    assert_eq!(refusal(run(&e, "giveaway.reroll", ALICE, json!({ "giveawayId": id })).await), "That giveaway is still running. End it first.");

    let r = run(&e, "giveaway.end", ALICE, json!({ "giveawayId": id })).await.unwrap();
    assert_eq!(r.message, "Ended \"Nitro\" with no valid entrants.");
    assert_eq!(node(&["giveaway", &id])["giveaway"]["status"], json!("ENDED"));
    assert_eq!(refusal(run(&e, "giveaway.end", ALICE, json!({ "giveawayId": id })).await), "That giveaway is already ended.");
    assert_eq!(refusal(run(&e, "giveaway.reroll", ALICE, json!({ "giveawayId": id, "winners": 0 })).await), "Pick between 1 and 90 winners.");
    assert_eq!(refusal(run(&e, "giveaway.reroll", ALICE, json!({ "giveawayId": id })).await), "There are no other eligible entrants to draw from.");
    assert_eq!(refusal(run(&e, "giveaway.cancel", ALICE, json!({ "giveawayId": id })).await), "That giveaway is already ended.");

    // A giveaway the JS bot started is cancelled by the Rust task.
    let js = node(&["gstart", g, CH_TEXT, "Steam key"]);
    let jid = js["id"].as_str().unwrap().to_string();
    let r = run(&e, "giveaway.cancel", ALICE, json!({ "giveawayId": jid })).await.unwrap();
    assert_eq!((r.message.as_str(), r.data), ("Cancelled the giveaway for \"Steam key\".", Some(json!({ "giveawayId": jid }))));
    let back = node(&["giveaway", &jid]);
    assert_eq!(back["giveaway"]["status"], json!("CANCELLED"));
    assert_eq!(back["job"], Value::Null, "the end job is gone");
    assert_eq!(refusal(run(&e, "giveaway.cancel", ALICE, json!({ "giveawayId": "cmissingmissingmissingmis" })).await), "That giveaway no longer exists.");
    e.reset().await;
}

// ── Polls ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn poll_tasks_run_the_poll_service() {
    let Some(e) = env("980000000000000006").await else { return };
    let g = e.g;
    let r = run(&e, "poll.start", ALICE, json!({ "question": "Pizza?", "options": ["- Yes", "2) No"], "channelId": CH_TEXT, "maxChoices": 5, "duration": "1d" }))
        .await
        .unwrap();
    assert!(r.message.starts_with("Posted the poll \"Pizza?\" in #general. It ends "), "{}", r.message);
    assert!(r.message.ends_with(" UTC. Choices per member was lowered to 2 (the number of options)."), "{}", r.message);
    let id = r.data.unwrap()["pollId"].as_str().unwrap().to_string();
    let row = node(&["poll", &id]);
    assert_eq!(row["options"], json!(["Yes", "No"]));
    assert_eq!(row["maxChoices"], json!(2));
    assert_eq!(row["authorId"], json!(ALICE));
    assert_eq!(row["status"], json!("ACTIVE"));

    let r = run(&e, "poll.end", ALICE, json!({ "pollId": id })).await.unwrap();
    assert_eq!(r.message, "Ended \"Pizza?\". The results were posted in #general.");
    assert_eq!(refusal(run(&e, "poll.end", ALICE, json!({ "pollId": id })).await), "That poll has already ended.");
    let r = run(&e, "poll.delete", ALICE, json!({ "pollId": id })).await.unwrap();
    assert_eq!(r.message, "Deleted the poll \"Pizza?\" and its votes.");
    assert_eq!(node(&["poll", &id]), Value::Null);

    // A poll the JS bot posted: deleted by the Rust task, message included.
    let js = node(&["pstart", g, CH_TEXT, "Tea?"]);
    let (pid, mid) = (js["id"].as_str().unwrap().to_string(), js["messageId"].as_str().unwrap().to_string());
    e.rest.on("DELETE", &format!("/channels/{CH_TEXT}/messages/{mid}"), 403, json!({ "code": 50013, "message": "Missing Permissions" }));
    let r = run(&e, "poll.delete", BOB, json!({ "pollId": pid })).await.unwrap();
    assert_eq!(
        r.message,
        "Deleted the poll \"Tea?\" and its votes. I could not delete its message (I am missing permissions there), so delete that by hand."
    );
    assert_eq!(node(&["poll", &pid]), Value::Null);
    e.reset().await;
}

/// polls.js poll.delete fetches the poll's channel (`guild.channels.fetch`) before it deletes the message: a channel
/// that is gone means nothing to delete, and a channel of another server (discord.js GuildChannelUnowned) is never
/// touched. Before the fix the Rust task sent the DELETE to whatever channel id the row held.
#[tokio::test]
async fn poll_delete_finds_the_channel_in_this_server_before_deleting() {
    let Some(e) = env("980000000000000031").await else { return };
    let start = |q: &str| json!({ "question": q, "options": ["Yes", "No"], "channelId": CH_TEXT });
    let move_to = |ch: &'static str| format!(r#"UPDATE "Poll" SET "channelId" = '{ch}' WHERE "id" = $1"#);
    let deletes = |e: &Env| e.rest.calls_to("DELETE", "/channels/*/messages/*").len();

    // The channel is gone (Unknown Channel, the double's default answer): nothing to delete, no note.
    let id = run(&e, "poll.start", ALICE, start("Gone?")).await.unwrap().data.unwrap()["pollId"].as_str().unwrap().to_string();
    sqlx::query(sqlx::AssertSqlSafe(move_to("920000000000000077"))).bind(id.clone()).execute(e.t.app.db()).await.unwrap();
    e.rest.clear_calls();
    let r = run(&e, "poll.delete", ALICE, json!({ "pollId": id })).await.unwrap();
    assert_eq!(r.message, "Deleted the poll \"Gone?\" and its votes.");
    assert_eq!(deletes(&e), 0, "no DELETE for a channel that does not exist");
    assert_eq!(node(&["poll", &id]), Value::Null);

    // A channel of another server: the message is left alone and the admin is told to delete it by hand.
    let id = run(&e, "poll.start", ALICE, start("Elsewhere?")).await.unwrap().data.unwrap()["pollId"].as_str().unwrap().to_string();
    sqlx::query(sqlx::AssertSqlSafe(move_to("920000000000000078"))).bind(id.clone()).execute(e.t.app.db()).await.unwrap();
    e.rest.on(
        "GET",
        "/channels/920000000000000078",
        200,
        json!({ "id": "920000000000000078", "type": 0, "guild_id": "980000000000000099", "name": "other", "position": 0,
                "permission_overwrites": [], "nsfw": false, "parent_id": null, "flags": 0 }),
    );
    e.rest.clear_calls();
    let r = run(&e, "poll.delete", ALICE, json!({ "pollId": id })).await.unwrap();
    assert_eq!(r.message, "Deleted the poll \"Elsewhere?\" and its votes. I could not delete its message, so delete that by hand.");
    assert_eq!(deletes(&e), 0, "no DELETE in another server's channel");
    assert_eq!(node(&["poll", &id]), Value::Null);
    e.reset().await;
}

// ── Reaction roles ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reaction_role_tasks_post_and_delete_panels() {
    use feat_reactionroles::reaction_role_service::{self as rr, NewItem, PanelDraft};
    let Some(e) = env("980000000000000007").await else { return };
    let g = e.g;
    let draft = PanelDraft {
        id: "panel1".into(),
        guild_id: g.into(),
        channel_id: CH_TEXT.into(),
        message_id: "0".into(),
        title: "Pick roles".into(),
        description: None,
        kind: "NORMAL".into(),
        layout: "BUTTONS".into(),
        max_roles: None,
        plain_text: false,
        message_content: None,
        attached: false,
        placeholder: None,
        items: vec![rr::draft_item("panel1", &NewItem { role_id: ROLE_MOD.into(), label: "Mods".into(), ..Default::default() }, 0).unwrap()],
    };
    rr::create_panel(e.t.app.db(), &draft).await.unwrap();

    let r = run(&e, "rr.publish", BOB, json!({ "groupId": "panel1", "mode": "sync" })).await.unwrap();
    assert_eq!(r.message, "Posted \"Pick roles\" in #general.");
    let data = r.data.unwrap();
    let message_id = data["messageId"].as_str().unwrap().to_string();
    assert_eq!(data["url"], json!(format!("https://discord.com/channels/{g}/{CH_TEXT}/{message_id}")));
    let row = node(&["rr", g, "panel1"]);
    assert_eq!(row["messageId"], json!(message_id));
    assert_eq!(row["channelId"], json!(CH_TEXT));

    let r = run(&e, "rr.publish", BOB, json!({ "groupId": "panel1", "mode": "sync" })).await.unwrap();
    assert_eq!(r.message, "Updated the panel message of \"Pick roles\".");
    assert_eq!(e.rest.calls_to("PATCH", &format!("/channels/{CH_TEXT}/messages/{message_id}")).len(), 1);

    let r = run(&e, "rr.repost", BOB, json!({ "groupId": "panel1" })).await.unwrap();
    assert_eq!(r.message, "\"Pick roles\" is now in #general.");
    let moved = r.data.unwrap()["messageId"].as_str().unwrap().to_string();
    assert_ne!(moved, message_id);
    assert_eq!(e.rest.calls_to("DELETE", &format!("/channels/{CH_TEXT}/messages/{message_id}")).len(), 1, "the old message is removed");

    let r = run(&e, "rr.delete", BOB, json!({ "groupId": "panel1" })).await.unwrap();
    assert_eq!(r.message, "Deleted \"Pick roles\", along with its message.");
    assert_eq!(node(&["rr", g, "panel1"]), Value::Null);
    assert_eq!(refusal(run(&e, "rr.delete", BOB, json!({ "groupId": "panel1" })).await), "That panel no longer exists.");
    e.reset().await;
}

// ── Tickets ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ticket_tasks_publish_refresh_and_delete_panels() {
    use feat_tickets::ticket_service::{self as ts, NewPanel};
    let Some(e) = env("980000000000000008").await else { return };
    let g = e.g;
    let panel = ts::create_panel(&e.t.app, NewPanel::new(g, "Support")).await.unwrap().unwrap();
    let pid = panel.panel.id.clone();

    assert_eq!(
        refusal(run(&e, "ticket.refresh", BOB, json!({ "panelId": pid })).await),
        "This panel is not published, so there is no message to update. Use \"Publish\" first."
    );
    let r = run(&e, "ticket.publish", BOB, json!({ "panelId": pid, "channelId": CH_TEXT })).await.unwrap();
    assert_eq!(
        r.message,
        "Posted \"Support\" in #general. This panel has no ticket manager roles, so only moderators and admins can see its tickets."
    );
    let data = r.data.unwrap();
    assert_eq!(data["mode"], json!("posted"));
    assert_eq!(node(&["ticketPanel", g, &pid])["messageId"], data["messageId"]);

    // The JS dashboard queued a manager sync for another panel; the Rust task merges into the same job.
    node(&["managersync", g, "otherpanel", &json!([ROLE_ADMIN]).to_string()]);
    ts::update_panel_fields(&e.t.app, &pid, g, &[("managerRoleIds", json!([ROLE_MOD]))]).await.unwrap().unwrap();
    let r = run(&e, "ticket.refresh", BOB, json!({ "panelId": pid, "managersAdded": [ROLE_MOD, ROLE_MOD], "managersRemoved": [ROLE_MOD] })).await.unwrap();
    assert_eq!(r.message, "Updated the published panel message. Existing tickets are being updated in the background (the 50 newest).");
    let job = node(&["job", &format!("ticket.managersync:{g}")]);
    assert_eq!(job["type"], json!("ticket.managersync"));
    assert_eq!(job["maxAttempts"], json!(3));
    let expected = ts::merge_manager_sync(&ts::manager_sync_payload("otherpanel", &[ROLE_ADMIN.to_string()]), &ts::manager_sync_payload(&pid, &[ROLE_MOD.to_string()]));
    assert_eq!(job["payload"], expected);
    assert!(job["payload"]["panels"]["otherpanel"].is_object() && job["payload"]["panels"][&pid].is_object(), "{}", job["payload"]);

    // Nothing to sync and no message to refresh.
    let r = run(&e, "ticket.refresh", BOB, json!({ "panelId": pid, "message": false })).await.unwrap();
    assert_eq!(r.message, "Done.");

    let r = run(&e, "ticket.delete", BOB, json!({ "panelId": pid })).await.unwrap();
    assert_eq!(r.message, "Deleted \"Support\" and its published message.");
    assert_eq!(node(&["ticketPanel", g, &pid]), Value::Null);
    assert_eq!(refusal(run(&e, "ticket.delete", BOB, json!({ "panelId": pid })).await), "That ticket panel no longer exists.");
    e.reset().await;
}

// ── Failures of Discord and of the guild's setup ──────────────────────────────

#[tokio::test]
async fn send_failures_reward_and_ping_refusals_become_the_js_messages() {
    let Some(e) = env("980000000000000009").await else { return };
    // No XP provider is installed in this app (the Levels plugin is not registered).
    let m = bot_config::ModuleKey::Leveling.info();
    assert_eq!(
        refusal(run(&e, "giveaway.start", ALICE, json!({ "prize": "Nitro", "channelId": CH_TEXT, "duration": "1h", "xpPrize": 50 })).await),
        format!("XP prizes are not available on this bot (the {} {} module is not installed).", m.emoji, m.label)
    );
    // The bot (Administrator) may ping the role; the moderator may not.
    assert_eq!(
        refusal(run(&e, "giveaway.start", ALICE, json!({ "prize": "Nitro", "channelId": CH_TEXT, "duration": "1h", "pingRoleId": ROLE_ADMIN })).await),
        format!("You don't have permission to ping @role{ROLE_ADMIN} in #general.")
    );
    e.rest.fail("POST", "/channels/*/messages", 403, 50013, "Missing Permissions");
    assert_eq!(
        refusal(run(&e, "giveaway.start", ALICE, json!({ "prize": "Nitro", "channelId": CH_TEXT, "duration": "1h" })).await),
        "I couldn't post the giveaway in #general. Check my permissions there."
    );
    assert_eq!(
        refusal(run(&e, "poll.start", ALICE, json!({ "question": "Tea?", "options": ["Yes", "No"], "channelId": CH_TEXT })).await),
        "I couldn't post the poll in #general: I'm missing permissions there."
    );
    e.reset().await;
}

#[tokio::test]
async fn a_draft_giveaway_goes_live_again() {
    let Some(e) = env("980000000000000010").await else { return };
    let r = run(&e, "giveaway.start", ALICE, json!({ "prize": "Nitro", "channelId": CH_TEXT, "duration": "2h" })).await.unwrap();
    let id = r.data.unwrap()["giveawayId"].as_str().unwrap().to_string();
    let to_draft = |ends: &str| format!(r#"UPDATE "Giveaway" SET "status" = 'DRAFT', "messageId" = NULL, "endsAt" = {ends} WHERE "id" = $1"#);
    sqlx::query(sqlx::AssertSqlSafe(to_draft(r#""endsAt""#))).bind(id.clone()).execute(e.t.app.db()).await.unwrap();
    assert_eq!(
        refusal(run(&e, "giveaway.end", ALICE, json!({ "giveawayId": id })).await),
        "That giveaway is a draft (its message was deleted). Restart it first."
    );
    let r = run(&e, "giveaway.restart", ALICE, json!({ "giveawayId": id })).await.unwrap();
    assert!(r.message.starts_with("\"Nitro\" is live again in #general and ends "), "{}", r.message);
    assert!(r.message.ends_with(" UTC. Existing entries were kept."));
    let row = node(&["giveaway", &id]);
    assert_eq!(row["giveaway"]["status"], json!("ACTIVE"));
    assert!(row["giveaway"]["messageId"].is_string());

    let past = to_draft(r#"((now() AT TIME ZONE 'UTC') - interval '1 hour')"#);
    sqlx::query(sqlx::AssertSqlSafe(past)).bind(id.clone()).execute(e.t.app.db()).await.unwrap();
    assert_eq!(
        refusal(run(&e, "giveaway.restart", ALICE, json!({ "giveawayId": id })).await),
        "This giveaway's end time has passed. Give it a new length or end time."
    );
    let r = run(&e, "giveaway.restart", ALICE, json!({ "giveawayId": id, "duration": "30m", "channelId": CH_TEXT })).await.unwrap();
    assert!(r.message.contains("is live again in #general"), "{}", r.message);
    e.reset().await;
}

fn panel_draft(g: &str, id: &str, title: &str, layout: &str, message_id: &str) -> feat_reactionroles::reaction_role_service::PanelDraft {
    feat_reactionroles::reaction_role_service::PanelDraft {
        id: id.into(),
        guild_id: g.into(),
        channel_id: CH_TEXT.into(),
        message_id: message_id.into(),
        title: title.into(),
        description: None,
        kind: "NORMAL".into(),
        layout: layout.into(),
        max_roles: None,
        plain_text: false,
        message_content: None,
        attached: false,
        placeholder: None,
        items: Vec::new(),
    }
}

#[tokio::test]
async fn a_role_template_fills_an_empty_panel_and_posts_it() {
    use feat_reactionroles::reaction_role_service as rr;
    use feat_reactionroles::role_template_setup::describe_setup;
    use feat_reactionroles::role_templates::get_role_template;
    let Some(e) = env("980000000000000011").await else { return };
    let g = e.g;
    let next = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    e.rest.on_with("POST", "/guilds/*/roles", move |call: &RestCall| {
        let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let role = json!({
            "id": format!("{}", 1_400_000_000_000_000_000u64 + n), "name": call.body["name"], "color": 0, "hoist": false, "position": 1,
            "permissions": "0", "managed": false, "mentionable": false, "flags": 0,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        });
        (200, role)
    });
    rr::create_panel(e.t.app.db(), &panel_draft(g, "tpl1", "Pings", "BUTTONS", "0")).await.unwrap();
    rr::create_panel(e.t.app.db(), &panel_draft(g, "tpl2", "Pings", "REACTIONS", "0")).await.unwrap();
    assert_eq!(
        refusal(run(&e, "rr.roleTemplate", BOB, json!({ "groupId": "tpl2", "template": "pings" })).await),
        "Template panels use a dropdown or buttons."
    );

    let template = get_role_template("pings").unwrap();
    let r = run(&e, "rr.roleTemplate", BOB, json!({ "groupId": "tpl1", "template": "pings" })).await.unwrap();
    let note = template.note.unwrap().replace("**", "").replace('`', "");
    assert_eq!(r.message, format!("{} Posted \"Pings\" in #general. {note}", describe_setup(template.roles.len(), 0)));
    let row = node(&["rr", g, "tpl1"]);
    assert_eq!(row["items"].as_array().unwrap().len(), template.roles.len());
    assert_eq!(row["items"][0]["style"], json!(2));
    assert_eq!(row["messageId"], r.data.unwrap()["messageId"]);
    assert_eq!(refusal(run(&e, "rr.roleTemplate", BOB, json!({ "groupId": "tpl1", "template": "pings" })).await), "That panel already has roles.");
    e.reset().await;
}

#[tokio::test]
async fn a_reaction_panel_syncs_its_reactions() {
    use feat_reactionroles::reaction_role_service::{self as rr, NewItem};
    let Some(e) = env("980000000000000012").await else { return };
    let g = e.g;
    let mut draft = panel_draft(g, "react1", "React", "REACTIONS", "1300000000000000999");
    let item = NewItem { role_id: ROLE_MOD.into(), label: "Mods".into(), emoji: Some("👍".into()), ..Default::default() };
    draft.items = vec![rr::draft_item("react1", &item, 0).unwrap()];
    rr::create_panel(e.t.app.db(), &draft).await.unwrap();
    let r = run(&e, "rr.publish", BOB, json!({ "groupId": "react1", "mode": "sync", "removedEmojis": ["🎉", "👍", 5] })).await.unwrap();
    assert_eq!(r.message, "Updated the panel message of \"React\".");
    assert_eq!(e.rest.calls_to("PATCH", &format!("/channels/{CH_TEXT}/messages/1300000000000000999")).len(), 1);
    let reacted = e.rest.calls_to("PUT", &format!("/channels/{CH_TEXT}/messages/1300000000000000999/reactions/**"));
    assert_eq!(reacted.len(), 1, "{:?}", e.rest.calls());
    e.reset().await;
}

#[tokio::test]
async fn a_published_ticket_panel_is_updated_and_an_unchanged_save_queues_nothing() {
    use feat_tickets::ticket_service::{self as ts, NewPanel};
    let Some(e) = env("980000000000000013").await else { return };
    let g = e.g;
    let panel = ts::create_panel(&e.t.app, NewPanel::new(g, "Help")).await.unwrap().unwrap();
    let pid = panel.panel.id.clone();
    let first = run(&e, "ticket.publish", BOB, json!({ "panelId": pid, "channelId": CH_TEXT })).await.unwrap();
    let second = run(&e, "ticket.publish", BOB, json!({ "panelId": pid, "channelId": CH_TEXT })).await.unwrap();
    assert!(second.message.starts_with("Updated \"Help\" in #general."), "{}", second.message);
    assert_eq!(second.data.as_ref().unwrap()["mode"], json!("edited"));
    assert_eq!(second.data.unwrap()["messageId"], first.data.unwrap()["messageId"]);

    node(&["managersync", g, "otherpanel", &json!([ROLE_ADMIN]).to_string()]);
    let before = node(&["job", &format!("ticket.managersync:{g}")]);
    // Roles that are not managers cannot be "added"; roles that still are cannot be "removed".
    let r = run(&e, "ticket.refresh", BOB, json!({ "panelId": pid, "managersAdded": [ROLE_MOD], "managersRemoved": [] })).await.unwrap();
    assert_eq!(r.message, "Updated the published panel message.");
    assert_eq!(node(&["job", &format!("ticket.managersync:{g}")]), before);
    assert_eq!(refusal(run(&e, "ticket.refresh", BOB, json!({ "panelId": pid, "managersAdded": "x" })).await), "That request is not valid.");
    assert_eq!(refusal(run(&e, "ticket.refresh", BOB, json!({ "panelId": pid, "managersAdded": ["x"] })).await), "A role is not valid.");
    e.reset().await;
}
