//! The database cases of `test/reminders.test.js` and `test/review-community2.test.js` (TEST_DATABASE_URL).

use std::sync::Arc;

use super::*;

struct H {
    t: DbTestApp,
    g: &'static str,
    rest: Arc<RecRest>,
}

const CH: &str = "810000000000000001";

async fn harness(g: &'static str, denied: Permissions) -> Option<H> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    t.set_discord(offline_discord_as(&[fake_guild(g, CH, denied).json()]));
    let rest = RecRest::install(&t);
    t.settings().update_settings(g, SettingsPatch::new().set("timezone", "UTC").set("remindersEnabled", true)).await.expect("settings");
    Some(H { t, g, rest })
}

fn interval(minutes: i32, created_ago_ms: i64) -> Schedule {
    Schedule {
        mode: ReminderMode::Interval,
        time_of_day: None,
        weekdays: Vec::new(),
        interval_minutes: Some(minutes),
        window_start: None,
        window_end: None,
        run_at: None,
        require_activity: false,
        created_at: Some(Utc::now() - chrono::Duration::milliseconds(created_ago_ms)),
    }
}

impl H {
    async fn create(&self, name: &str, fields: Schedule, extra: impl FnOnce(&mut svc::NewReminder)) -> Reminder {
        let now = fields.created_at.unwrap_or_else(Utc::now);
        let mut n = svc::NewReminder {
            guild_id: self.g.into(),
            name: name.into(),
            channel_id: CH.into(),
            template: json!({ "content": "Hello {server}", "embed": null, "buttons": [] }),
            fields,
            ping_role_id: None,
            created_by: "u1".into(),
            now,
        };
        extra(&mut n);
        svc::create_reminder_row(&self.t, n).await.expect("created")
    }

    async fn fire(&self, r: &Reminder, attempt: i32) -> anyhow::Result<JobOutcome> {
        let job = Job {
            id: "job".into(),
            guild_id: self.g.into(),
            job_type: "reminder.fire".into(),
            payload: json!({ "reminderId": r.id }),
            run_at: Utc::now(),
            attempts: attempt,
            max_attempts: 10,
            dedupe_key: None,
            version: 0,
        };
        svc::fire_reminder(job, self.t.app.clone(), attempt).await
    }

    async fn row(&self, r: &Reminder) -> Reminder {
        svc::get_reminder(self.t.db(), self.g, &r.id).await.expect("read").expect("row")
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

/// Five minutes and ten seconds ago: a 5-minute interval anchored there has a slot ten seconds ago.
const DUE: i64 = 310_000;

#[tokio::test]
async fn create_reminder_row_normalises_names_and_enforces_the_cap_and_unique_names() {
    let Some(h) = harness("955140000000000001", Permissions::empty()).await else { return };
    let r = h.create("daily-standup", interval(60, 0), |_| {}).await;
    assert_eq!(svc::find_reminder_by_name(h.t.db(), h.g, " Daily Standup ").await.expect("q").map(|x| x.id), Some(r.id.clone()));
    assert_eq!(svc::find_reminder_by_name(h.t.db(), h.g, "!!!").await.expect("q"), None);
    let err = svc::create_reminder_row(
        &h.t,
        svc::NewReminder {
            guild_id: h.g.into(),
            name: "daily-standup".into(),
            channel_id: CH.into(),
            template: svc::default_template(),
            fields: interval(60, 0),
            ping_role_id: None,
            created_by: "u".into(),
            now: Utc::now(),
        },
    )
    .await
    .expect_err("taken");
    let u = err.downcast_ref::<svc::ReminderUserError>().expect("user error");
    assert_eq!(u.title, "Name Taken");
    assert!(u.message.contains("**daily-standup** already exists"));
    for i in 1..bot_config::limits::REMINDERS {
        h.create(&format!("r{i}"), interval(60, 0), |_| {}).await;
    }
    let err = svc::create_reminder_row(
        &h.t,
        svc::NewReminder {
            guild_id: h.g.into(),
            name: "one-too-many".into(),
            channel_id: CH.into(),
            template: svc::default_template(),
            fields: interval(60, 0),
            ping_role_id: None,
            created_by: "u".into(),
            now: Utc::now(),
        },
    )
    .await
    .expect_err("cap");
    assert_eq!(err.downcast_ref::<svc::ReminderUserError>().map(|u| u.title.as_str()), Some("Limit Reached"));
    h.done().await;
}

#[tokio::test]
async fn sync_job_replaces_and_cancels_and_pause_delete_disable_cancel_too() {
    let Some(h) = harness("955140000000000002", Permissions::empty()).await else { return };
    let settings = h.t.settings().get_settings(h.g).await.expect("settings");
    let r = h.create("sync", interval(60, 0), |_| {}).await;
    let at = svc::sync_reminder_job(&h.t, &r, Some(&settings), Utc::now()).await.expect("ok").expect("scheduled");
    let job = h.t.scheduler().get_job(&svc::job_key(&r.id)).await.expect("job");
    assert_eq!((job.r#type.as_str(), job.payload.clone(), job.max_attempts), ("reminder.fire", json!({ "reminderId": r.id }), 10));
    assert_eq!(job.run_at.timestamp_millis(), at.timestamp_millis());
    svc::pause_reminder(&h.t, &r).await.expect("paused");
    assert!(h.t.scheduler().get_job(&svc::job_key(&r.id)).await.is_none());
    assert!(!h.row(&r).await.enabled);
    let paused = h.row(&r).await;
    assert_eq!(svc::sync_reminder_job(&h.t, &paused, Some(&settings), Utc::now()).await.expect("ok"), None);

    let once = h.create("old-once", Schedule { mode: ReminderMode::OneShot, run_at: Some(Utc::now() - chrono::Duration::hours(2)), ..interval(60, 0) }, |_| {}).await;
    assert_eq!(svc::sync_reminder_job(&h.t, &once, Some(&settings), Utc::now()).await.expect("ok"), None, "a one-shot over an hour late is not scheduled");

    let d = h.create("deleted", interval(60, 0), |_| {}).await;
    svc::sync_reminder_job(&h.t, &d, Some(&settings), Utc::now()).await.expect("ok");
    svc::delete_reminder(&h.t, &d).await.expect("deleted");
    assert!(svc::get_reminder(h.t.db(), h.g, &d.id).await.expect("q").is_none());
    assert!(h.t.scheduler().get_job(&svc::job_key(&d.id)).await.is_none());

    // autoDisable pauses once; channelDelete pauses the channel's reminders and cancels their jobs.
    let a = h.create("auto", interval(60, 0), |_| {}).await;
    svc::auto_disable(&h.t, &a, "why").await.expect("ok");
    svc::auto_disable(&h.t, &a, "why").await.expect("ok");
    assert!(!h.row(&a).await.enabled);
    let c1 = h.create("in-gone", interval(60, 0), |n| n.channel_id = "810000000000000333".into()).await;
    let c2 = h.create("in-gone-2", interval(60, 0), |n| n.channel_id = "810000000000000333".into()).await;
    let keep = h.create("kept", interval(60, 0), |_| {}).await;
    for x in [&c1, &c2, &keep] {
        svc::sync_reminder_job(&h.t, x, Some(&settings), Utc::now()).await.expect("ok");
    }
    assert_eq!(svc::disable_reminders_for_channel(&h.t, h.g, "810000000000000333").await.expect("ok"), 2);
    assert!(!h.row(&c1).await.enabled && !h.row(&c2).await.enabled && h.row(&keep).await.enabled);
    assert!(h.t.scheduler().get_job(&svc::job_key(&c1.id)).await.is_none());
    assert!(h.t.scheduler().get_job(&svc::job_key(&keep.id)).await.is_some());
    assert_eq!(svc::disable_reminders_for_channel(&h.t, h.g, "810000000000000333").await.expect("ok"), 0);
    h.done().await;
}

#[tokio::test]
async fn reconcile_repairs_missing_jobs_and_pauses_finished_ones() {
    let Some(h) = harness("955140000000000003", Permissions::empty()).await else { return };
    let ok = h.create("needs-job", interval(60, 0), |_| {}).await;
    let missed = h.create("missed", Schedule { mode: ReminderMode::OneShot, run_at: Some(Utc::now() - chrono::Duration::hours(3)), ..interval(60, 0) }, |_| {}).await;
    svc::reconcile_guild_reminders(&h.t, h.g).await.expect("ok");
    assert!(h.t.scheduler().get_job(&svc::job_key(&ok.id)).await.is_some());
    assert!(!h.row(&missed).await.enabled, "a one-shot missed while offline is paused");
    h.done().await;
}

#[tokio::test]
async fn fire_sends_a_due_recurring_reminder_once() {
    let Some(h) = harness("955140000000000004", Permissions::empty()).await else { return };
    let r = h.create("due", interval(5, DUE), |n| n.ping_role_id = Some(ROLE_OK.into())).await;
    let out = h.fire(&r, 1).await.expect("ok");
    let JobOutcome::Reschedule { next_run_at, .. } = out else { panic!("rescheduled") };
    assert!(next_run_at > Utc::now());
    let sent = h.rest.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["content"], format!("<@&{ROLE_OK}> Hello Test Guild"));
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": ["users"], "roles": [ROLE_OK] }));
    assert_eq!(sent[0].1["enforce_nonce"], true);
    assert_eq!(sent[0].1["nonce"].as_str().map(str::len), Some(25));
    let row = h.row(&r).await;
    assert!(row.last_sent_at.is_some());
    assert!(row.last_message_id.is_some());
    // The same occurrence again (a lock expiry) sends nothing: the claim is idempotent.
    h.fire(&r, 1).await.expect("ok");
    assert_eq!(h.rest.count(), 1);
    h.done().await;
}

#[tokio::test]
async fn fire_not_due_deleted_paused_malformed_and_module_off() {
    let Some(h) = harness("955140000000000005", Permissions::empty()).await else { return };
    let later = h.create("later", interval(60, -600_000), |_| {}).await;
    assert!(matches!(h.fire(&later, 1).await.expect("ok"), JobOutcome::Reschedule { .. }));
    assert_eq!(h.rest.count(), 0);
    let mut gone = later.clone();
    gone.id = "missing".into();
    assert_eq!(h.fire(&gone, 1).await.expect("ok"), JobOutcome::Done);
    let job = Job { id: "j".into(), guild_id: h.g.into(), job_type: "reminder.fire".into(), payload: json!({}), run_at: Utc::now(), attempts: 1, max_attempts: 10, dedupe_key: None, version: 0 };
    assert_eq!(svc::fire_reminder(job, h.t.app.clone(), 1).await.expect("ok"), JobOutcome::Done);
    svc::pause_reminder(&h.t, &later).await.expect("ok");
    assert_eq!(h.fire(&later, 1).await.expect("ok"), JobOutcome::Done);

    h.t.settings().update_settings(h.g, SettingsPatch::new().set("remindersEnabled", false)).await.expect("settings");
    let due = h.create("due-off", interval(5, DUE), |_| {}).await;
    assert!(matches!(h.fire(&due, 1).await.expect("ok"), JobOutcome::Reschedule { .. }), "keeps rescheduling without sending");
    assert_eq!(h.rest.count(), 0);
    let once = h.create("once-off", Schedule { mode: ReminderMode::OneShot, run_at: Some(Utc::now() - chrono::Duration::seconds(30)), ..interval(60, 0) }, |_| {}).await;
    assert_eq!(h.fire(&once, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&once).await.enabled, "a one-off is never delayed until the module is back on");
    h.done().await;
}

#[tokio::test]
async fn fire_the_activity_gate() {
    let Some(h) = harness("955140000000000006", Permissions::empty()).await else { return };
    let r = h.create("gated", Schedule { require_activity: true, ..interval(5, DUE) }, |_| {}).await;
    h.fire(&r, 1).await.expect("ok");
    assert_eq!(h.rest.count(), 1, "never sent here yet: sends");
    // Next occurrence: only bots spoke since.
    bot_db::sqlx::query(r#"UPDATE "Reminder" SET "lastSentAt" = "lastSentAt" - interval '10 minutes', "createdAt" = "createdAt" - interval '5 minutes' WHERE "id" = $1"#).bind(&r.id).execute(h.t.db()).await.expect("sql");
    *h.rest.history.lock().expect("lock") = Some(Ok(vec![MessageLite { bot: true, system: false }]));
    let r2 = h.row(&r).await;
    h.fire(&r2, 1).await.expect("ok");
    assert_eq!(h.rest.count(), 1, "nobody chatted: skipped");
    *h.rest.history.lock().expect("lock") = Some(Ok(vec![MessageLite { bot: true, system: false }, MessageLite { bot: false, system: false }]));
    h.fire(&r2, 1).await.expect("ok");
    assert_eq!(h.rest.count(), 2, "a human spoke: sent");
    h.done().await;
}

#[tokio::test]
async fn fire_send_failures_revert_the_claim_retry_or_pause() {
    let Some(h) = harness("955140000000000007", Permissions::empty()).await else { return };
    let once = h.create("retry-once", Schedule { mode: ReminderMode::OneShot, run_at: Some(Utc::now() - chrono::Duration::seconds(5)), ..interval(60, 0) }, |_| {}).await;
    h.rest.fail_once.lock().expect("lock").push(RestError { code: Some(0), status: Some(503), message: "err 0".into(), timeout: false });
    let first = h.fire(&once, 1).await;
    assert!(first.is_err(), "a transient failure asks the scheduler for another attempt");
    let row = h.row(&once).await;
    assert!(row.enabled && row.last_sent_at.is_none(), "the claim was given back");
    assert!(matches!(h.fire(&once, 2).await.expect("ok"), JobOutcome::Done));
    assert_eq!(h.rest.count(), 1, "sent once on the retry");
    assert!(!h.row(&once).await.enabled, "a one-shot is finished after it was sent");

    let rec = h.create("permanent", interval(5, DUE), |_| {}).await;
    h.rest.fail_with(Some(RestError::code(50013, "Missing Permissions")));
    assert!(matches!(h.fire(&rec, 1).await.expect("ok"), JobOutcome::Reschedule { .. }), "a permission error is not retried");
    assert_eq!(h.row(&rec).await.last_sent_at, None, "the claim was given back");
    h.rest.fail_with(Some(RestError { code: Some(0), status: Some(500), message: "boom".into(), timeout: false }));
    assert!(matches!(h.fire(&rec, 10).await.expect("ok"), JobOutcome::Reschedule { .. }), "the final attempt reschedules, never fails");
    h.rest.fail_with(Some(RestError::code(50035, "Invalid Form Body")));
    assert_eq!(h.fire(&rec, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&rec).await.enabled, "a rejected message pauses the reminder");
    let gone = h.create("gone-send", interval(5, DUE), |_| {}).await;
    h.rest.fail_with(Some(RestError::code(10003, "Unknown Channel")));
    assert_eq!(h.fire(&gone, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&gone).await.enabled);
    h.rest.fail_with(None);
    h.done().await;
}

#[tokio::test]
async fn fire_pauses_on_a_gone_or_wrong_channel_and_never_on_a_cache_miss_alone() {
    let Some(h) = harness("955140000000000008", Permissions::empty()).await else { return };
    let miss = h.create("miss", interval(5, DUE), |n| n.channel_id = "810000000000000444".into()).await;
    assert!(matches!(h.fire(&miss, 10).await.expect("ok"), JobOutcome::Reschedule { .. }));
    assert!(h.row(&miss).await.enabled, "a transient lookup failure pauses nothing");
    assert!(h.fire(&miss, 1).await.is_err(), "with attempts left it is retried");
    h.rest.on_fetch(Some(Err(RestError::code(10003, "Unknown Channel"))));
    assert_eq!(h.fire(&miss, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&miss).await.enabled);
    let voice = h.create("voice", interval(5, DUE), |n| n.channel_id = "810000000000000078".into()).await;
    assert_eq!(h.fire(&voice, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&voice).await.enabled);
    h.done().await;

    let Some(h) = harness("955140000000000009", Permissions::VIEW_CHANNEL).await else { return };
    let r = h.create("no-view", interval(5, DUE), |_| {}).await;
    assert_eq!(h.fire(&r, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&r).await.enabled, "View Channel lost pauses");
    h.done().await;
    let Some(h) = harness("955140000000000010", Permissions::SEND_MESSAGES).await else { return };
    let r = h.create("no-send", interval(5, DUE), |_| {}).await;
    assert!(matches!(h.fire(&r, 1).await.expect("ok"), JobOutcome::Reschedule { .. }));
    assert!(h.row(&r).await.enabled, "a missing Send Messages skips, it does not pause");
    assert_eq!(h.rest.count(), 0);
    h.done().await;
}

#[tokio::test]
async fn fire_one_shot_future_waits_and_stale_is_paused() {
    let Some(h) = harness("955140000000000011", Permissions::empty()).await else { return };
    let future = h.create("future", Schedule { mode: ReminderMode::OneShot, run_at: Some(Utc::now() + chrono::Duration::minutes(10)), ..interval(60, 0) }, |_| {}).await;
    assert!(matches!(h.fire(&future, 1).await.expect("ok"), JobOutcome::Reschedule { next_run_at, .. } if next_run_at.timestamp_millis() == future.run_at.map(|d| d.timestamp_millis()).unwrap_or(0)));
    let stale = h.create("stale", Schedule { mode: ReminderMode::OneShot, run_at: Some(Utc::now() - chrono::Duration::minutes(61)), ..interval(60, 0) }, |_| {}).await;
    assert_eq!(h.fire(&stale, 1).await.expect("ok"), JobOutcome::Done);
    assert!(!h.row(&stale).await.enabled);
    assert_eq!(h.rest.count(), 0);
    h.done().await;
}

/// `guild.channels.fetch(id)` refuses another guild's channel (GuildChannelUnowned: no code, so transient): retried
/// while attempts are left, skipped on the last one, never sent there and never paused.
#[tokio::test]
async fn fire_never_sends_in_another_guilds_channel_found_over_rest() {
    let Some(h) = harness("955140000000000031", Permissions::empty()).await else { return };
    let r = h.create("elsewhere", interval(5, DUE), |n| n.channel_id = "810000000000000445".into()).await;
    h.rest.on_fetch(Some(Ok(Some(rest_channel(&h.t, h.g, "955140000000000099", "810000000000000445")))));
    assert!(h.fire(&r, 1).await.is_err(), "with attempts left it is retried");
    assert!(matches!(h.fire(&r, 10).await.expect("ok"), JobOutcome::Reschedule { .. }));
    assert_eq!(h.rest.count(), 0);
    assert!(h.row(&r).await.enabled);
    h.rest.on_fetch(Some(Ok(None)));
    assert!(matches!(h.fire(&r, 10).await.expect("ok"), JobOutcome::Reschedule { .. }));
    assert!(h.row(&r).await.enabled, "a channel that is not a guild channel pauses nothing either");
    h.rest.on_fetch(Some(Ok(Some(rest_channel(&h.t, h.g, h.g, "810000000000000445")))));
    assert!(matches!(h.fire(&r, 1).await.expect("ok"), JobOutcome::Reschedule { .. }));
    assert_eq!(h.rest.count(), 1, "this guild's channel found over REST is sent to");
    h.done().await;
}

/// `/reminder resume` on a reminder whose channel the cache misses: the channel fetched over REST is checked as it is
/// (the JS `checkChannelUsable(channel)`), and another guild's channel is the "took too long" answer.
#[tokio::test]
async fn resume_checks_a_channel_fetched_over_rest() {
    let Some(h) = harness("955140000000000032", Permissions::empty()).await else { return };
    h.t.install(registry());
    let r = h.create("fetched", interval(60, 0), |n| n.channel_id = "810000000000000446".into()).await;
    svc::pause_reminder(&h.t, &r).await.expect("paused");
    h.rest.on_fetch(Some(Ok(Some(rest_channel(&h.t, h.g, "955140000000000098", "810000000000000446")))));
    let calls = route(&h, admin("resume", h.g).option("name", STRING, json!("fetched"))).await;
    assert_eq!(last_title(&calls), "❌  Temporarily Unavailable");
    assert!(!h.row(&r).await.enabled);
    h.rest.on_fetch(Some(Ok(Some(rest_channel(&h.t, h.g, h.g, "810000000000000446")))));
    let calls = route(&h, admin("resume", h.g).option("name", STRING, json!("fetched"))).await;
    assert_eq!(last_title(&calls), "✅  Reminder Resumed", "{}", last_desc(&calls));
    assert!(h.row(&r).await.enabled);
    h.done().await;
}

// ── The form and the command through the router ────────────────────────────────

fn admin(sub: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash("reminder").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).channel(CH).subcommand(sub)
}

const STRING: u8 = bot_core::command::option_type::STRING;
const CHANNEL: u8 = bot_core::command::option_type::CHANNEL;
const BOOLEAN: u8 = bot_core::command::option_type::BOOLEAN;

async fn route(h: &H, i: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    h.t.responses.clear();
    bot_core::router::route_interaction(&h.t, i.build()).await;
    h.t.responses.calls()
}

fn last_title(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.last().and_then(|c| c.embed_title()).unwrap_or("").to_string()
}

fn last_desc(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.last().and_then(|c| c.embed_description()).unwrap_or("").to_string()
}

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_feeds::register(&mut reg);
    reg
}

#[tokio::test]
async fn reminder_create_with_a_message_is_public_and_warns_about_user_variables() {
    let Some(h) = harness("955140000000000012", Permissions::empty()).await else { return };
    h.t.install(registry());
    let calls = route(&h, admin("create", h.g).option("name", STRING, json!("Stand Up")).option("channel", CHANNEL, json!(CH)).option("mode", STRING, json!("interval")).option("interval", STRING, json!("3m"))).await;
    assert_eq!(last_title(&calls), "❌  Invalid Schedule");
    assert_eq!(last_desc(&calls), "The shortest interval is 5 minutes.");
    let calls = route(&h, admin("create", h.g).option("name", STRING, json!("x")).option("channel", CHANNEL, json!("810000000000000999")).option("mode", STRING, json!("time")).option("time", STRING, json!("09:00"))).await;
    assert_eq!(last_title(&calls), "❌  Channel Not Found");
    let calls = route(&h, admin("create", h.g).option("name", STRING, json!("!!!")).option("channel", CHANNEL, json!(CH)).option("mode", STRING, json!("time")).option("time", STRING, json!("09:00"))).await;
    assert_eq!(last_title(&calls), "❌  Invalid Name");

    let calls = route(
        &h,
        admin("create", h.g)
            .option("name", STRING, json!("Stand Up"))
            .option("channel", CHANNEL, json!(CH))
            .option("mode", STRING, json!("time"))
            .option("time", STRING, json!("9:30pm"))
            .option("weekdays", STRING, json!("weekdays"))
            .option("message", STRING, json!("Hi {user}")),
    )
    .await;
    assert_eq!(calls[0].method, "deferReply");
    assert!(!calls[0].ephemeral());
    assert_eq!(last_title(&calls), "✅  Reminder Created");
    let d = last_desc(&calls);
    assert!(d.starts_with(&format!("**stand-up** → <#{CH}>\nWeekdays at 21:30\nNext: <t:")), "{d}");
    assert!(d.contains("user variables like `{user}`"));
    let r = svc::find_reminder_by_name(h.t.db(), h.g, "stand-up").await.expect("q").expect("row");
    assert_eq!(r.template, json!({ "content": "Hi {user}", "embed": null, "buttons": [] }));
    assert_eq!(r.created_by, ADMIN);
    assert!(h.t.scheduler().get_job(&svc::job_key(&r.id)).await.is_some());

    // The form path saves first and opens the editor.
    let calls = route(&h, admin("create", h.g).option("name", STRING, json!("form")).option("channel", CHANNEL, json!(CH)).option("mode", STRING, json!("interval")).option("interval", STRING, json!("2h"))).await;
    assert_eq!(calls.last().map(|c| c.method), Some("showModal"));
    let f = svc::find_reminder_by_name(h.t.db(), h.g, "form").await.expect("q").expect("saved before the form");
    assert_eq!(calls.last().map(|c| c.body["custom_id"].clone()), Some(json!(format!("reminder-modal:{}", f.id))));
    assert_eq!(f.template, svc::default_template());

    // Name only opens the editor; schedule options edit.
    let calls = route(&h, admin("edit", h.g).option("name", STRING, json!("form"))).await;
    assert_eq!(calls.last().map(|c| c.method), Some("showModal"));
    let calls = route(&h, admin("edit", h.g).option("name", STRING, json!("form")).option("window_start", STRING, json!("09:00")).option("require_activity", BOOLEAN, json!(true))).await;
    assert_eq!(last_title(&calls), "✅  Reminder Updated");
    let f2 = h.row(&f).await;
    assert_eq!((f2.window_start.as_deref(), f2.require_activity, f2.interval_minutes), (Some("09:00"), true, Some(120)));

    // The form submit saves the template, defaults an empty form, refuses non-admins and other guilds.
    h.t.responses.clear();
    let modal = FakeInteraction::modal(&format!("reminder-modal:{}", f.id), &[("tpl-content", "Form text {server}")]).guild(h.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).channel(CH);
    bot_core::router::route_interaction(&h.t, modal.build()).await;
    let calls = h.t.responses.calls();
    assert_eq!(last_title(&calls), "✅  Reminder Saved");
    assert_eq!(h.row(&f).await.template["content"], "Form text {server}");
    h.t.responses.clear();
    let empty = FakeInteraction::modal(&format!("reminder-modal:{}", f.id), &[("tpl-content", "  ")]).guild(h.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).channel(CH);
    bot_core::router::route_interaction(&h.t, empty.build()).await;
    assert_eq!(h.row(&f).await.template, svc::default_template(), "an empty form means the default text");
    h.t.responses.clear();
    let stranger = FakeInteraction::modal(&format!("reminder-modal:{}", f.id), &[("tpl-content", "x")]).guild(h.g).user("600000000000000123").channel(CH);
    bot_core::router::route_interaction(&h.t, stranger.build()).await;
    assert_eq!(last_title(&h.t.responses.calls()), "❌  Insufficient Permissions");
    h.t.responses.clear();
    let missing = FakeInteraction::modal("reminder-modal:nope", &[("tpl-content", "x")]).guild(h.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).channel(CH);
    bot_core::router::route_interaction(&h.t, missing.build()).await;
    assert_eq!(last_title(&h.t.responses.calls()), "❌  Reminder Not Found");

    // list, pause, resume, next, test, autocomplete.
    let calls = route(&h, admin("list", h.g)).await;
    assert_eq!(last_title(&calls), "⏰ Reminders");
    assert!(last_desc(&calls).contains("**stand-up** ·") && last_desc(&calls).contains("**form** ·"));
    let calls = route(&h, admin("pause", h.g).option("name", STRING, json!("form"))).await;
    assert_eq!(last_title(&calls), "✅  Reminder Paused");
    assert!(h.t.scheduler().get_job(&svc::job_key(&f.id)).await.is_none());
    let calls = route(&h, admin("pause", h.g).option("name", STRING, json!("form"))).await;
    assert_eq!(last_title(&calls), "❌  Already Paused");
    let calls = route(&h, admin("next", h.g).option("name", STRING, json!("form"))).await;
    assert!(last_desc(&calls).contains("⏸️ Paused — no upcoming runs."));
    let calls = route(&h, admin("resume", h.g).option("name", STRING, json!("form"))).await;
    assert_eq!(last_title(&calls), "✅  Reminder Resumed");
    let calls = route(&h, admin("resume", h.g).option("name", STRING, json!("form"))).await;
    assert_eq!(last_title(&calls), "✅  Schedule Refreshed");
    let calls = route(&h, admin("next", h.g).option("name", STRING, json!("form"))).await;
    assert_eq!(last_title(&calls), "⏰ Next run");
    assert!(last_desc(&calls).contains("Next: <t:") && last_desc(&calls).contains("Upcoming: "));
    let calls = route(&h, admin("test", h.g).option("name", STRING, json!("stand-up"))).await;
    assert!(calls[0].ephemeral());
    assert_eq!(last_title(&calls), "✅  Test Sent");
    let sent = h.rest.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": [] }), "role pings are suppressed in tests");
    assert!(h.row(&r).await.last_sent_at.is_none(), "a preview touches nothing");
    let calls = route(&h, admin("next", h.g).option("name", STRING, json!("nope"))).await;
    assert_eq!(last_title(&calls), "❌  Reminder Not Found");

    h.t.responses.clear();
    let ac = FakeInteraction::autocomplete("reminder").guild(h.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).subcommand("pause").focused("name", "Stand");
    bot_core::router::route_interaction(&h.t, ac.build()).await;
    let choices = h.t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(choices, vec![json!({ "name": "▶ stand-up · Time of day", "value": "stand-up" })]);
    h.done().await;
}

// ── Cross-language ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_read_each_others_reminders_and_jobs() {
    let Some(h) = harness("955140000000000020", Permissions::empty()).await else { return };
    let input = json!({ "name": "js-made", "channelId": CH, "template": { "content": "From JS", "embed": null, "buttons": [] }, "pingRoleId": ROLE_OK,
        "schedule": { "mode": "interval", "interval": "30m", "weekdays": "weekdays", "windowStart": "08:00", "windowEnd": "18:00", "requireActivity": true } });
    let js = run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["rem-create", h.g, &input.to_string()]);
    assert!(js.get("error").is_none(), "{js}");
    let row = svc::find_reminder_by_name(h.t.db(), h.g, "js-made").await.expect("q").expect("row");
    assert_eq!(serde_json::to_value(&row).unwrap_or(Value::Null), js["row"], "the row reads the same through Prisma and Rust");
    let job = h.t.scheduler().get_job(&svc::job_key(&row.id)).await.expect("job");
    assert_eq!((job.r#type.as_str(), job.payload.clone(), job.max_attempts), ("reminder.fire", json!({ "reminderId": row.id }), 10));
    assert_eq!(js["job"]["dedupeKey"], svc::job_key(&row.id));
    // The Rust maths agree with the time the JS scheduled.
    let settings = h.t.settings().get_settings(h.g).await.expect("settings");
    let ours = svc::job_run_at(&Schedule::from(&row), &svc::resolve_time_zone(Some(&settings.timezone)), Utc::now());
    assert_eq!(js["at"], json!(ours.map(|d| bot_db::format_js_date(&d))));

    let mine = h
        .create("rust-made", Schedule { mode: ReminderMode::TimeOfDay, time_of_day: Some("07:15".into()), weekdays: vec![1, 3], ..interval(60, 0) }, |n| {
            n.template = json!({ "content": null, "embed": { "title": "From Rust", "description": "d" }, "buttons": [] })
        })
        .await;
    svc::sync_reminder_job(&h.t, &mine, Some(&settings), Utc::now()).await.expect("ok");
    let back = run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["rem-read", h.g, "Rust Made"]);
    assert_eq!(back["row"], serde_json::to_value(&mine).unwrap_or(Value::Null), "{back}");
    assert_eq!(back["job"]["payload"], json!({ "reminderId": mine.id }));
    assert_eq!((back["job"]["type"].clone(), back["job"]["status"].clone()), (json!("reminder.fire"), json!("PENDING")));
    h.done().await;
}
