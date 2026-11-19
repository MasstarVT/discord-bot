//! The start-up against the mock database (`eval "$(node rust/mockdb/mockdb.mjs db bot)"`) and the Redis mock, with
//! Discord offline (a FakeRest): the migrations are current, the app boots with every feature, READY runs the
//! startup work, the scheduler claims a ScheduledJob row the JS bot wrote and runs it, and the shutdown is clean.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_config::MODULES;
use bot_core::{AppConfig, SettingsPatch};
use bot_core::gateway::{Before, Envelope, GatewayState, process};
use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID, connect_test_pool, database_url, redis_url, run_node};
use serde_json::json;
use serenity::all::Permissions;

const G: &str = "900000000000000731";
const OWNER: &str = "910000000000000001";
const BAN_ROLE: &str = "920000000000000731";
const CHANNEL: &str = "930000000000000731";
const BANNED: &str = "940000000000000731";
const VOICE: &str = "930000000000000732";
const TALKER_A: &str = "950000000000000731";
const TALKER_B: &str = "950000000000000732";

async fn reset(pool: &bot_db::PgPool) {
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(G).execute(pool).await.expect("cleanup runs");
}

async fn wait_for(what: &str, secs: u64, mut f: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bot_boots_runs_a_js_job_and_shuts_down() {
    let (Some(db), Some(redis)) = (database_url(), redis_url()) else {
        println!("skipped: TEST_DATABASE_URL / TEST_REDIS_URL not set");
        return;
    };
    // Every log line of this binary (spawned tasks included) goes here.
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = lines.clone();
    bot_util::logger::set_sink(Some(Box::new(move |_, l| sink.lock().unwrap_or_else(|e| e.into_inner()).push(l.to_string()))));
    let logged = |needle: &str| lines.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|l| l.contains(needle));

    let admin = connect_test_pool(&db).await;
    reset(&admin).await;

    // 1. The migrations the JS toolchain applied are the ones this build carries (checksums included): the start
    //    applies nothing.
    let report = bot::index::migrate(&db).await.expect("migrate deploy runs");
    assert!(report.applied.is_empty(), "applied {:?}", report.applied);
    assert!(report.missing_locally.is_empty(), "unknown migrations {:?}", report.missing_locally);

    // 2. The JS bot schedules a temp-ban expiry (moderationService's mod.unban job), already due.
    let run_at = chrono::Utc::now().timestamp_millis() - 60_000;
    let dedupe = format!("mod.unban:{G}:{BANNED}");
    let opts = json!({
        "type": "mod.unban",
        "payload": { "userId": BANNED, "infractionId": null },
        "dedupeKey": dedupe,
        "maxAttempts": 20,
        "onConflict": "replace",
        "runAtMs": run_at,
    });
    let js = run_node("rust/fixtures/gen/bot-core-js.mjs", &["schedule", G, &opts.to_string()]);
    assert!(js.get("error").is_none() && js["id"].is_string(), "the JS scheduleJob failed: {js}");

    // 3. The app with the production wiring, Discord offline (REST goes to a fake), every feature installed.
    let pool = bot_db::connect_lazy(&db).expect("TEST_DATABASE_URL parses");
    let config = AppConfig { client_id: Some("100000000000000001".into()), ..Default::default() };
    let app = bot::bot::build_app(pool, Some(&redis), config).expect("REDIS url parses");
    assert!(app.redis().expect("redis backend").wait_ready(5_000).await, "the Redis mock is not reachable");
    let rest = FakeRest::new();
    let guild = FakeGuild::new(G, OWNER)
        .role(BAN_ROLE, 1, Permissions::BAN_MEMBERS | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .bot_member(TEST_BOT_ID, &[BAN_ROLE])
        .channel(CHANNEL, 0, "general", &[])
        .channel(VOICE, 2, "Voice", &[])
        .member(TALKER_A, &[])
        .member(TALKER_B, &[])
        .voice_state(TALKER_A, VOICE)
        .voice_state(TALKER_B, VOICE);
    app.set_discord(rest.discord(&[guild.json()]));

    // Every module on (the guildReady hooks are gated), voice XP on, a birthday channel: the startup work of the
    // levels and birthdays plugins then seeds their recurring jobs.
    app.settings().ensure_guild_settings(G).await.expect("settings row");
    let mut patch = SettingsPatch::new().set("voiceXpEnabled", true).set("birthdayChannelId", CHANNEL);
    for m in MODULES.iter() {
        patch = patch.set(m.flag, true);
    }
    app.settings().update_settings(G, patch).await.expect("settings update");

    let boot = bot::registry::build_registry().install(&app);
    assert!(boot.plugins.errors.is_empty(), "plugin install errors: {:?}", boot.plugins.errors);
    assert!(boot.command_errors.is_empty(), "command errors: {:?}", boot.command_errors);
    assert_eq!((boot.plugins.plugins, boot.commands), (43, 79));
    assert!(!logged("Boot step"), "a boot step failed: {:?}", lines.lock().unwrap_or_else(|e| e.into_inner()));
    // The boot steps ran (feat-games' numguess listener is one).
    assert!(app.hooks().list(bot_config::HookPoint::MessageCreate).iter().any(|h| h.id == "games-fixes:numguess"));

    // 4. READY: the presence, the scheduler, the ready hooks, then guildReady for the cached guild.
    let gateway = GatewayState::new(1);
    let ready = json!({
        "v": 10,
        "user": { "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true, "verified": true, "mfa_enabled": false, "flags": 0 },
        "guilds": [],
        "session_id": "offline",
        "resume_gateway_url": "wss://offline.invalid",
        "shard": [0, 1],
        "application": { "id": "100000000000000001", "flags": 0 }
    });
    let ready: serenity::all::ReadyEvent = serde_json::from_value(ready).expect("READY deserializes");
    tokio::spawn(bot::bot::sync_levels_ready_at(app.clone()));
    process(&app, &gateway, Envelope { shard: 0, event: serenity::all::Event::Ready(ready), before: Before::default() }).await;
    wait_for("the startup work", 30, async || gateway.shard_ready(0) && logged("guildReady completed for 1/1 guild(s).")).await;
    assert!(app.scheduler().is_started());
    assert!(logged("| 1 guild(s) | Shard 0"));
    let settings_row: Option<(String,)> =
        sqlx::query_as(r#"SELECT "guildId" FROM "GuildSettings" WHERE "guildId" = $1"#).bind(G).fetch_optional(&admin).await.expect("query runs");
    assert!(settings_row.is_some(), "runGuildReady ensures the settings row");
    // Evidence the gated guildReady hooks ran: levels (gated on leveling) and birthdays seeded their jobs.
    wait_for("the recurring jobs the startup work seeds", 20, async || {
        let types: Vec<(String,)> = sqlx::query_as(r#"SELECT "type" FROM "ScheduledJob" WHERE "guildId" = $1"#)
            .bind(G)
            .fetch_all(&admin)
            .await
            .expect("query runs");
        let types: Vec<&str> = types.iter().map(|t| t.0.as_str()).collect();
        types.contains(&"levels.voiceSweep") && types.contains(&"birthday.tick")
    })
    .await;
    let ready_at = app.ready_at_ms().expect("ready time recorded");
    wait_for("the levels ready time", 5, async || feat_levels::xp_service::ready_at(&app, 0) == ready_at).await;

    // 5. The scheduler claims the JS job and runs the Rust handler: the unban, the UNBAN case, the row deleted.
    wait_for("the JS-written job to run", 40, async || {
        let left: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "ScheduledJob" WHERE "dedupeKey" = $1"#)
            .bind(&dedupe)
            .fetch_optional(&admin)
            .await
            .expect("query runs");
        left.is_none()
    })
    .await;
    let unbans = rest.calls_to("DELETE", "/guilds/*/bans/*");
    assert_eq!(unbans.len(), 1, "one unban: {:?}", rest.calls());
    assert_eq!(unbans[0].path, format!("/guilds/{G}/bans/{BANNED}"));
    assert_eq!(unbans[0].reason.as_deref(), Some("Temporary ban expired"));
    let case: Option<(String, String)> =
        sqlx::query_as(r#"SELECT "type"::text, "reason" FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2"#)
            .bind(G)
            .bind(BANNED)
            .fetch_optional(&admin)
            .await
            .expect("query runs");
    assert_eq!(case, Some(("UNBAN".to_string(), "Temporary ban expired".to_string())));

    // 6. The shutdown of bot.js: scheduler (5 s), flushes, then the pool and Redis.
    let started = std::time::Instant::now();
    bot::bot::shutdown(&app, None).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!app.scheduler().is_started());
    assert_eq!(app.scheduler().inflight(), 0);
    assert!(app.db().is_closed());
    // The hook runner's and the scheduler's failure lines (bot-core hooks/mod.rs, scheduler/mod.rs).
    let all = lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let failures: Vec<&String> = all
        .iter()
        .filter(|l| {
            (l.contains("Hook ") && (l.contains(" failed") || l.contains(" timed out and was abandoned")))
                || (l.contains("Job ") && (l.contains("failed outside its handler") || l.contains("abandoned after")))
                || l.contains("No handler registered for job type")
                || l.contains("Scheduler tick failed")
                || l.contains("Could not complete job")
                || l.contains("Could not ensure the settings row")
        })
        .collect();
    assert!(failures.is_empty(), "hook or job failures: {failures:#?}");
    for l in all.iter().filter(|l| l.contains("[ERROR]") || l.contains("[WARN")) {
        println!("logged: {l}");
    }

    reset(&admin).await;
    bot_util::logger::set_sink(None);
}
