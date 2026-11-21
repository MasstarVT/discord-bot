//! The database and Discord half of `tests/events.rs`: the service, the jobs, the hooks, the router, the JS round trip.

use bot_core::prelude::*;
use bot_core::scheduler::{Job, JobCtx};
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, database_url, run_node};
use bot_db::{GuildEventStatus, RsvpStatus};
use feat_events::event_service::{self as svc, RsvpResult};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Permissions as P};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::common::*;

mod channels;

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

const OWNER: &str = "600000000000000099";
const HOST: &str = "600000000000000001";
const U1: &str = "600000000000000011";
const U2: &str = "600000000000000012";
const MODR: &str = "600000000000000020";
const BOT_ROLE: &str = "820000000000000001";
const R1: &str = "820000000000000002";
const MOD_ROLE: &str = "820000000000000003";
const C1: &str = "810000000000000001";
const LOCKED: &str = "810000000000000002";

static NEXT: AtomicU64 = AtomicU64::new(890_000_000_000_000_000);
fn fresh() -> String {
    NEXT.fetch_add(1, Ordering::SeqCst).to_string()
}

fn bot_perms() -> P {
    P::VIEW_CHANNEL
        | P::SEND_MESSAGES
        | P::EMBED_LINKS
        | P::READ_MESSAGE_HISTORY
        | P::MANAGE_ROLES
        | P::MANAGE_CHANNELS
        | P::MANAGE_EVENTS
        | P::CONNECT
        | P::SPEAK
}

fn fake_guild(g: &str, bot: P) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role(BOT_ROLE, 10, bot)
        .role(R1, 2, P::empty())
        .role(MOD_ROLE, 3, P::MODERATE_MEMBERS)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .member(HOST, &[])
        .member(U1, &[])
        .member(U2, &[])
        .member(MODR, &[MOD_ROLE])
        .channel(C1, 0, "general", &[])
        .channel(LOCKED, 0, "locked", &[(g, false, P::empty(), P::SEND_MESSAGES), (BOT_ROLE, false, P::SEND_MESSAGES, P::empty())])
}

fn scheduled_event_json(id: &str, g: &str, status: u8) -> Value {
    json!({
        "id": id, "guild_id": g, "channel_id": null, "creator_id": null, "name": "x", "description": null,
        "scheduled_start_time": "2030-01-01T00:00:00.000000+00:00", "scheduled_end_time": "2030-01-01T01:00:00.000000+00:00",
        "privacy_level": 2, "status": status, "entity_type": 3, "entity_id": null, "entity_metadata": { "location": "x" },
        "user_count": null, "image": null
    })
}

/// Scripts the REST routes the module creates things with (Discord events, channels, roles).
fn script(rest: &FakeRest, g: &str) {
    let gg = g.to_string();
    rest.on_with("POST", "/guilds/*/scheduled-events", move |_| (200, scheduled_event_json("830000000000000001", &gg, 1)));
    let gg = g.to_string();
    rest.on_with("PATCH", "/guilds/*/scheduled-events/*", move |_| (200, scheduled_event_json("830000000000000001", &gg, 1)));
    let gg = g.to_string();
    rest.on_with("POST", "/guilds/*/channels", move |call| {
        let b = &call.body;
        (
            200,
            json!({
                "id": fresh(), "type": b["type"], "guild_id": gg, "name": b["name"], "position": 0,
                "permission_overwrites": b.get("permission_overwrites").cloned().unwrap_or(json!([])),
                "parent_id": b.get("parent_id").cloned().unwrap_or(Value::Null), "nsfw": false
            }),
        )
    });
    let gg = g.to_string();
    rest.on_with("DELETE", "/channels/*", move |call| {
        let id = call.path.rsplit('/').next().unwrap_or_default().to_string();
        (200, json!({ "id": id, "type": 0, "guild_id": gg, "name": "gone", "position": 0, "permission_overwrites": [], "nsfw": false }))
    });
    rest.on_with("POST", "/guilds/*/roles", |call| {
        (
            200,
            json!({
                "id": fresh(), "name": call.body["name"], "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false,
                "mentionable": false, "flags": 0,
                "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
            }),
        )
    });
}

struct Env {
    t: DbTestApp,
    rest: FakeRest,
    g: &'static str,
}

impl Env {
    async fn new(g: &'static str) -> Option<Self> {
        Self::with(g, fake_guild(g, bot_perms())).await
    }

    async fn with(g: &'static str, guild: FakeGuild) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([g]);
        t.reset_guilds(&[g]).await;
        let rest = FakeRest::new();
        script(&rest, g);
        t.set_discord(rest.discord(&[guild.json()]));
        t.install(registry());
        t.settings()
            .update_settings(g, SettingsPatch::new().set("eventsEnabled", true).set("eventChannelId", Value::Null).set("eventReminderMinutes", 15))
            .await
            .expect("settings row");
        Some(Self { t, rest, g })
    }

    fn gid(&self) -> GuildId {
        GuildId::new(self.g.parse().unwrap_or(1))
    }

    fn data(&self, over: impl FnOnce(&mut svc::NewEvent)) -> svc::NewEvent {
        let now = self.t.clock().now_ms();
        let mut d = svc::NewEvent {
            title: "Movie night".into(),
            description: Some("Snacks".into()),
            location: Some("Voice 1".into()),
            starts_at: chrono::DateTime::from_timestamp_millis(now + 2 * 86_400_000).unwrap_or_default(),
            ends_at: chrono::DateTime::from_timestamp_millis(now + 2 * 86_400_000 + 2 * 3_600_000),
            host_id: HOST.into(),
            role_id: None,
            capacity: None,
            reminder_minutes: 15,
        };
        over(&mut d);
        d
    }

    async fn create(&self, over: impl FnOnce(&mut svc::NewEvent)) -> (bot_db::GuildEvent, Vec<String>) {
        let settings = self.t.settings().get_settings_safe(self.g).await;
        let data = self.data(over);
        svc::create_event(&self.t, self.gid(), ChannelId::new(C1.parse().unwrap_or(1)), settings.as_deref(), &data, None)
            .await
            .expect("no db error")
            .expect("created")
    }

    /// A bare open event row (no Discord), like the JS `seedEvent`.
    async fn seed(&self, over: impl FnOnce(&mut svc::NewEvent)) -> bot_db::GuildEvent {
        let data = self.data(over);
        match svc::create_event_row(&self.t, self.g, &data, C1, None).await.expect("row") {
            svc::CreateRow::Created(e) => *e,
            other => panic!("not created: {other:?}"),
        }
    }

    async fn event(&self, id: &str) -> bot_db::GuildEvent {
        svc::find_event(&self.t, id).await.expect("db").expect("row")
    }

    async fn job(&self, key: &str) -> Option<bot_db::ScheduledJob> {
        self.t.scheduler().get_job(key).await
    }

    fn ctx(&self) -> JobCtx {
        JobCtx { app: self.t.app.clone(), guild_id: self.g.into(), late_by_ms: 0, attempt: 1 }
    }

    fn job_of(&self, kind: &str, payload: Value) -> Job {
        Job {
            id: "J".into(),
            guild_id: self.g.into(),
            job_type: kind.into(),
            payload,
            run_at: chrono::Utc::now(),
            attempts: 1,
            max_attempts: 5,
            dedupe_key: None,
            version: 1,
        }
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

async fn going(t: &DbTestApp, g: &str, event: &str, user: &str, status: RsvpStatus) -> RsvpResult {
    svc::apply_rsvp(t, event, g, user, status).await.expect("rsvp").result
}

fn button(env: &Env, id: &str, user: &str, roles: &[&str]) -> serenity::all::Interaction {
    FakeInteraction::button(id).guild(env.g).user(user).roles(roles).channel(C1).build()
}

async fn route(env: &Env, i: serenity::all::Interaction) {
    env.t.responses.clear();
    bot_core::router::route_interaction(&env.t, i).await;
}

#[tokio::test]
async fn create_event_posts_stores_ids_makes_the_discord_event_and_schedules_both_jobs() {
    let Some(env) = Env::new("930000000000000001").await else { return };
    let (event, notes) = env.create(|_| {}).await;
    assert!(notes.is_empty(), "{notes:?}");
    let posts = env.rest.calls_to("POST", &format!("/channels/{C1}/messages"));
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["embeds"][0]["title"], "📅 Movie night");
    let ids: Vec<String> = posts[0].body["components"][0]["components"].as_array().into_iter().flatten().map(|b| s(&b["custom_id"])).collect();
    assert_eq!(ids, ["going", "maybe", "declined", "calendar"].map(|a| format!("event:{}:{a}", event.id)));
    assert_eq!(posts[0].body["allowed_mentions"]["parse"], json!([]));
    assert!(event.message_id.is_some());
    assert_eq!(event.discord_event_id.as_deref(), Some("830000000000000001"));
    let de = env.rest.calls_to("POST", "/guilds/*/scheduled-events");
    assert_eq!(de[0].body["entity_type"], 3);
    assert_eq!(de[0].body["privacy_level"], 2);
    assert_eq!(de[0].body["entity_metadata"]["location"], "Voice 1");
    assert_eq!(de[0].reason.as_deref(), Some("Event created with /event"));
    let end = env.job(&svc::end_job_key(&event.id)).await.expect("end job");
    assert_eq!(end.r#type, "event.end");
    assert_eq!(end.payload, json!({ "eventId": event.id }));
    assert_eq!(end.run_at.timestamp_millis(), event.ends_at.map(|e| e.timestamp_millis()).unwrap_or(0));
    let remind = env.job(&svc::remind_job_key(&event.id)).await.expect("remind job");
    assert_eq!(remind.run_at.timestamp_millis(), event.starts_at.timestamp_millis() - 15 * 60_000);
    assert!(env.job(&svc::voice_job_key(&event.id)).await.is_none(), "no chat, no voice job");
    env.done().await;
}

#[tokio::test]
async fn no_duration_ends_after_two_hours_and_a_zero_or_past_reminder_is_not_scheduled() {
    let Some(env) = Env::new("930000000000000002").await else { return };
    let (event, _) = env
        .create(|d| {
            d.ends_at = None;
            d.reminder_minutes = 0;
        })
        .await;
    let end = env.job(&svc::end_job_key(&event.id)).await.expect("end job");
    assert_eq!(end.run_at.timestamp_millis(), event.starts_at.timestamp_millis() + 2 * 3_600_000);
    assert!(env.job(&svc::remind_job_key(&event.id)).await.is_none());
    let now = env.t.clock().now_ms();
    let (soon, _) = env
        .create(|d| {
            d.starts_at = chrono::DateTime::from_timestamp_millis(now + 5 * 60_000).unwrap_or_default();
            d.reminder_minutes = 15;
        })
        .await;
    assert!(env.job(&svc::remind_job_key(&soon.id)).await.is_none(), "the reminder time already passed");
    env.done().await;
}

#[tokio::test]
async fn the_discord_event_needs_the_setting_a_location_and_manage_events() {
    let Some(env) = Env::new("930000000000000003").await else { return };
    let (no_loc, notes) = env.create(|d| d.location = None).await;
    assert_eq!(notes, ["No Discord event was created: it needs a location."]);
    assert!(no_loc.discord_event_id.is_none());
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventCreateDiscordEvent", false)).await.expect("settings");
    let (_, notes) = env.create(|_| {}).await;
    assert!(notes.is_empty(), "the setting is off: nothing, not even a note");
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventCreateDiscordEvent", true)).await.expect("settings");
    env.rest.fail("POST", "/guilds/*/scheduled-events", 400, 50035, "Invalid Form Body");
    let (failed, notes) = env.create(|_| {}).await;
    assert!(failed.discord_event_id.is_none());
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("I could not create the Discord event ("), "{notes:?}");
    env.done().await;

    let Some(env) = Env::with("930000000000000004", fake_guild("930000000000000004", bot_perms() - P::MANAGE_EVENTS)).await else { return };
    let (_, notes) = env.create(|_| {}).await;
    assert_eq!(notes, ["No Discord event was created: I need the **Manage Events** permission."]);
    env.done().await;
}

#[tokio::test]
async fn a_failed_post_removes_the_row_again() {
    let Some(env) = Env::new("930000000000000005").await else { return };
    env.rest.fail("POST", "/channels/*/messages", 403, 50001, "Missing Access");
    let settings = env.t.settings().get_settings_safe(env.g).await;
    let out = svc::create_event(&env.t, env.gid(), ChannelId::new(C1.parse().unwrap_or(1)), settings.as_deref(), &env.data(|_| {}), None).await.expect("db");
    assert_eq!(out.err(), Some(svc::CreateFailure::Send));
    let (events, _) = svc::list_open_events(&env.t, env.g).await.expect("list");
    assert!(events.is_empty());
    env.done().await;
}

#[tokio::test]
async fn caps_count_open_events_only_and_overdue_ones_are_ended_to_make_room() {
    let Some(env) = Env::new("930000000000000006").await else { return };
    let mut ids = Vec::new();
    for _ in 0..svc::consts::ACTIVE_MAX {
        ids.push(env.seed(|_| {}).await.id);
    }
    let data = env.data(|_| {});
    assert_eq!(svc::create_event_row(&env.t, env.g, &data, C1, None).await.expect("db"), svc::CreateRow::Limit);
    // A closed event frees a place.
    assert!(matches!(
        svc::close_event(&env.t, &ids[0], GuildEventStatus::Ended, svc::CloseReason::Manual, Some(env.g)).await.expect("db"),
        svc::CloseOutcome::Closed { .. }
    ));
    assert!(matches!(svc::create_event_row(&env.t, env.g, &data, C1, None).await.expect("db"), svc::CreateRow::Created(_)));
    // Full again; one event is long over: create_event ends it and makes room.
    let past = env.t.clock().now_ms() - 5 * 3_600_000;
    sqlx::query(r#"UPDATE "GuildEvent" SET "startsAt" = $2, "endsAt" = NULL WHERE "id" = $1"#)
        .bind(&ids[1])
        .bind(chrono::DateTime::from_timestamp_millis(past).unwrap_or_default().naive_utc())
        .execute(env.t.db())
        .await
        .expect("move");
    let (created, _) = env.create(|_| {}).await;
    assert!(svc::is_open(Some(&created)));
    assert_eq!(env.event(&ids[1]).await.status, GuildEventStatus::Ended);
    env.done().await;

    // The host cap: 3 open events per member below staff.
    let Some(env) = Env::new("930000000000000007").await else { return };
    for _ in 0..svc::consts::HOST_MAX {
        let d = env.data(|_| {});
        assert!(matches!(svc::create_event_row(&env.t, env.g, &d, C1, Some(svc::consts::HOST_MAX)).await.expect("db"), svc::CreateRow::Created(_)));
    }
    let d = env.data(|_| {});
    assert_eq!(svc::create_event_row(&env.t, env.g, &d, C1, Some(svc::consts::HOST_MAX)).await.expect("db"), svc::CreateRow::HostLimit);
    let other = env.data(|d| d.host_id = U1.into());
    assert!(matches!(svc::create_event_row(&env.t, env.g, &other, C1, Some(svc::consts::HOST_MAX)).await.expect("db"), svc::CreateRow::Created(_)));
    env.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rsvp_capacity_is_exact_under_concurrency_and_leaving_frees_a_place() {
    let Some(env) = Env::new("930000000000000008").await else { return };
    let event = env.seed(|d| d.capacity = Some(2)).await;
    let mut tasks = Vec::new();
    for i in 0..6 {
        let (app, id, g) = (env.t.app.clone(), event.id.clone(), env.g);
        tasks.push(tokio::spawn(async move { svc::apply_rsvp(&app, &id, g, &format!("60000000000000010{i}"), RsvpStatus::Going).await }));
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.expect("joined").expect("rsvp").result);
    }
    assert_eq!(results.iter().filter(|r| **r == RsvpResult::Set).count(), 2);
    assert_eq!(results.iter().filter(|r| **r == RsvpResult::Full).count(), 4);
    let tally = svc::get_tally(&env.t, &event.id).await.expect("tally");
    assert_eq!(tally.going, 2);
    let first = tally.going_ids[0].clone();
    assert_eq!(going(&env.t, env.g, &event.id, &first, RsvpStatus::Going).await, RsvpResult::Same);
    assert_eq!(going(&env.t, env.g, &event.id, &first, RsvpStatus::Maybe).await, RsvpResult::Set);
    assert_eq!(going(&env.t, env.g, &event.id, U1, RsvpStatus::Going).await, RsvpResult::Set, "a place was freed");
    assert_eq!(going(&env.t, "930000000000000099", &event.id, U2, RsvpStatus::Going).await, RsvpResult::Gone, "another guild");
    svc::close_event(&env.t, &event.id, GuildEventStatus::Cancelled, svc::CloseReason::Manual, None).await.expect("closed");
    assert_eq!(going(&env.t, env.g, &event.id, U2, RsvpStatus::Going).await, RsvpResult::Closed);
    let t = svc::get_tally(&env.t, &event.id).await.expect("tally");
    assert_eq!((t.going, t.maybe, t.declined), (2, 1, 0));
    env.done().await;
}

#[tokio::test]
async fn rsvp_button_gives_the_role_on_going_takes_it_back_on_leaving_and_replies_privately() {
    let Some(env) = Env::new("930000000000000009").await else { return };
    let (event, _) = env.create(|d| d.role_id = Some(R1.into())).await;
    route(&env, button(&env, &format!("event:{}:going", event.id), U1, &[])).await;
    let calls = env.t.responses.calls();
    assert_eq!(env.t.responses.names(), ["deferReply", "editReply"]);
    assert!(calls[0].ephemeral());
    let content = calls[1].content().unwrap_or_default().to_string();
    assert!(content.starts_with("✅ You are marked as **Going** for **Movie night**.\nAdd it to your calendar"), "{content}");
    assert!(calls[1].body["components"][0]["components"][0]["url"].as_str().unwrap_or("").starts_with("https://calendar.google.com/"));
    let adds = env.rest.calls_to("PUT", &format!("/guilds/{}/members/{U1}/roles/{R1}", env.g));
    assert_eq!(adds.len(), 1);
    assert_eq!(adds[0].reason.as_deref(), Some("Event RSVP: going to Movie night"));
    let granted: (bool,) = sqlx::query_as(r#"SELECT "roleGranted" FROM "EventRsvp" WHERE "eventId" = $1 AND "userId" = $2"#)
        .bind(&event.id)
        .bind(U1)
        .fetch_one(env.t.db())
        .await
        .expect("row");
    assert!(granted.0);

    // Moving to Maybe: no file again, the role goes back.
    route(&env, button(&env, &format!("event:{}:maybe", event.id), U1, &[R1])).await;
    assert_eq!(env.t.responses.calls()[1].content(), Some("❔ You are marked as **Maybe** for **Movie night**."));
    assert_eq!(env.rest.calls_to("DELETE", &format!("/guilds/{}/members/{U1}/roles/{R1}", env.g)).len(), 1);
    route(&env, button(&env, &format!("event:{}:maybe", event.id), U1, &[])).await;
    assert_eq!(env.t.responses.calls()[1].content(), Some("You are already marked as **Maybe** for **Movie night**."));

    // Bad ids are inactive; closed and unknown events are refused.
    route(&env, button(&env, &format!("event:{}:sideways", event.id), U1, &[])).await;
    assert_eq!(env.t.responses.calls()[0].content(), Some("This button is no longer active."));
    route(&env, button(&env, "event:cnope:going", U1, &[])).await;
    assert_eq!(env.t.responses.calls()[1].content(), Some("This event no longer exists."));
    svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Manual, None).await.expect("closed");
    route(&env, button(&env, &format!("event:{}:going", event.id), U2, &[])).await;
    assert_eq!(env.t.responses.calls()[1].content(), Some("This event is no longer open."));
    env.done().await;
}

#[tokio::test]
async fn a_role_the_bot_cannot_give_is_reported_and_the_rsvp_still_counts() {
    let Some(env) = Env::new("930000000000000010").await else { return };
    let (event, _) = env.create(|d| d.role_id = Some(MOD_ROLE.into())).await;
    route(&env, button(&env, &format!("event:{}:going", event.id), U1, &[])).await;
    let content = env.t.responses.calls()[1].content().unwrap_or_default().to_string();
    assert!(content.contains("⚠️ I could not give you the event role: That role has dangerous permissions:"), "{content}");
    assert_eq!(svc::get_tally(&env.t, &event.id).await.expect("tally").going, 1);
    // A member who already held the role keeps it when they leave Going (the bot never gave it).
    let (held, _) = env.create(|d| d.role_id = Some(R1.into())).await;
    route(&env, button(&env, &format!("event:{}:going", held.id), U2, &[R1])).await;
    route(&env, button(&env, &format!("event:{}:declined", held.id), U2, &[R1])).await;
    assert!(env.rest.calls_to("DELETE", &format!("/guilds/{}/members/{U2}/roles/{R1}", env.g)).is_empty());
    env.done().await;
}

#[tokio::test]
async fn add_to_calendar_answers_privately_with_links_and_a_file() {
    let Some(env) = Env::new("930000000000000011").await else { return };
    let (event, _) = env.create(|_| {}).await;
    route(&env, button(&env, &format!("event:{}:calendar", event.id), U1, &[])).await;
    let calls = env.t.responses.calls();
    assert_eq!(env.t.responses.names(), ["deferReply", "editReply"]);
    assert!(calls[0].ephemeral());
    assert!(calls[1].content().unwrap_or_default().starts_with("📅 **Movie night**\nAdd it to your calendar"));
    let labels: Vec<String> = calls[1].body["components"][0]["components"].as_array().into_iter().flatten().map(|b| s(&b["label"])).collect();
    assert_eq!(labels, ["Google Calendar", "Outlook"]);
    // The .ics goes with the edited reply (`EditInteractionResponse::new_attachment`).
    assert_eq!(calls[1].body["attachments"][0]["filename"], "movie-night.ics", "{}", calls[1].body);
    assert_eq!(svc::get_tally(&env.t, &event.id).await.expect("tally").going, 0, "no RSVP");
    env.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_event_has_one_winner_edits_the_post_and_cancels_the_jobs() {
    let Some(env) = Env::new("930000000000000012").await else { return };
    let (event, _) = env.create(|_| {}).await;
    let (a, b) = tokio::join!(
        svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Timer, Some(env.g)),
        svc::close_event(&env.t, &event.id, GuildEventStatus::Cancelled, svc::CloseReason::Manual, Some(env.g)),
    );
    let outcomes = [a.expect("db"), b.expect("db")];
    assert_eq!(outcomes.iter().filter(|o| matches!(o, svc::CloseOutcome::Closed { .. })).count(), 1, "one winner");
    assert_eq!(outcomes.iter().filter(|o| matches!(o, svc::CloseOutcome::AlreadyClosed(_))).count(), 1);
    assert!(env.job(&svc::remind_job_key(&event.id)).await.is_none());
    let edits = env.rest.calls_to("PATCH", &format!("/channels/{C1}/messages/*"));
    assert!(!edits.is_empty());
    let last = edits.last().map(|c| c.body.clone()).unwrap_or_default();
    let disabled: Vec<bool> = last["components"][0]["components"].as_array().into_iter().flatten().map(|b| b["disabled"] == json!(true)).collect();
    assert_eq!(disabled, [true, true, true, true]);
    let title = s(&last["embeds"][0]["title"]);
    assert!(title.starts_with("🏁 Ended:") || title.starts_with("🚫 Cancelled:"), "{title}");
    assert_eq!(
        svc::close_event(&env.t, "cnothing", GuildEventStatus::Ended, svc::CloseReason::Timer, Some(env.g)).await.expect("db"),
        svc::CloseOutcome::NotFound
    );
    env.done().await;
}

#[tokio::test]
async fn cancel_deletes_the_discord_event_and_takes_the_role_only_from_members_the_bot_gave_it_to() {
    let guild = fake_guild("930000000000000013", bot_perms()).member("600000000000000031", &[R1]).member("600000000000000032", &[R1]);
    let Some(env) = Env::with("930000000000000013", guild).await else { return };
    let (event, _) = env.create(|d| d.role_id = Some(R1.into())).await;
    for (u, granted) in [("600000000000000031", true), ("600000000000000032", false), ("600000000000000033", true)] {
        going(&env.t, env.g, &event.id, u, RsvpStatus::Going).await;
        if granted {
            svc::mark_role_granted(&env.t, &event.id, u, true).await.expect("flag");
        }
    }
    let out = svc::cancel_event(&env.t, &event.id, Some(env.g), svc::CloseReason::Manual).await.expect("db");
    assert!(matches!(out, svc::CloseOutcome::Closed { discord_failed: false, .. }));
    assert_eq!(env.event(&event.id).await.status, GuildEventStatus::Cancelled);
    assert_eq!(env.rest.calls_to("DELETE", "/guilds/*/scheduled-events/830000000000000001").len(), 1);
    let removed = env.rest.calls_to("DELETE", &format!("/guilds/{}/members/*/roles/{R1}", env.g));
    assert_eq!(removed.len(), 1, "only the member the bot gave it to: {removed:?}");
    assert!(removed[0].path.contains("600000000000000031"));
    assert_eq!(removed[0].reason.as_deref(), Some("Event is over"));
    let flags: Vec<(String, bool)> = sqlx::query_as(r#"SELECT "userId", "roleGranted" FROM "EventRsvp" WHERE "eventId" = $1 ORDER BY "userId""#)
        .bind(&event.id)
        .fetch_all(env.t.db())
        .await
        .expect("rows");
    // The member who left (not cached: Unknown Member) keeps the flag, Role Persistence's marker.
    assert_eq!(flags, [("600000000000000031".into(), false), ("600000000000000032".into(), false), ("600000000000000033".into(), true)]);
    assert!(env.event(&event.id).await.discord_event_id.is_none());
    env.done().await;
}

#[tokio::test]
async fn strip_roles_is_bounded_with_a_cursor_and_the_end_job_continues_it() {
    let Some(env) = Env::new("930000000000000014").await else { return };
    let event = env.seed(|d| d.role_id = Some(R1.into())).await;
    sqlx::query(
        r#"INSERT INTO "EventRsvp" ("eventId", "userId", "status", "roleGranted", "updatedAt") SELECT $1, (700000000000000000 + n)::text, 'GOING', true, now() FROM generate_series(1, 120) n"#,
    )
    .bind(&event.id)
    .execute(env.t.db())
    .await
    .expect("rows");
    svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Manual, None).await.expect("closed");
    let closed = env.event(&event.id).await;
    let more = svc::strip_roles(&env.t, &closed, None, 0).await.expect("strip");
    assert_eq!(more.as_deref(), Some("700000000000000100"));
    assert_eq!(svc::strip_roles(&env.t, &closed, more.as_deref(), 0).await.expect("strip"), None, "the last batch");
    assert_eq!(svc::strip_roles(&env.t, &closed, None, svc::consts::STRIP_RUNS_MAX - 1).await.expect("strip"), None, "the run cap");
    // The end job with a cursor reschedules itself with the next one.
    let out = svc::handle_end_job(env.job_of("event.end", json!({ "eventId": event.id, "after": "700000000000000010", "run": 3 })), env.ctx()).await.expect("job");
    match out {
        JobOutcome::Reschedule { payload: Some(p), .. } => assert_eq!(p, json!({ "eventId": event.id, "after": "700000000000000110", "run": 4 })),
        other => panic!("{other:?}"),
    }
    // Without a cursor it ends an open event.
    let now = env.t.clock().now_ms();
    let open = env
        .seed(|d| {
            d.starts_at = chrono::DateTime::from_timestamp_millis(now - 3 * 3_600_000).unwrap_or_default(); // its end (start + 2 h) has passed
            d.ends_at = None;
        })
        .await;
    assert_eq!(svc::handle_end_job(env.job_of("event.end", json!({ "eventId": open.id })), env.ctx()).await.expect("job"), JobOutcome::Done);
    assert_eq!(env.event(&open.id).await.status, GuildEventStatus::Ended);
    let bad = svc::handle_end_job(env.job_of("event.end", json!({})), env.ctx()).await;
    assert!(bad.is_err_and(|e| e.downcast_ref::<bot_core::PermanentJobError>().is_some()), "a missing eventId is permanent");
    env.done().await;
}

#[tokio::test]
async fn the_reminder_pings_the_members_going_once_and_remembers_its_messages() {
    let Some(env) = Env::new("930000000000000015").await else { return };
    let (event, _) = env.create(|_| {}).await;
    for u in [U1, U2] {
        going(&env.t, env.g, &event.id, u, RsvpStatus::Going).await;
    }
    going(&env.t, env.g, &event.id, HOST, RsvpStatus::Maybe).await;
    env.rest.clear_calls();
    let job = env.job_of("event.remind", json!({ "eventId": event.id }));
    assert_eq!(svc::handle_remind_job(job.clone(), env.ctx()).await.expect("job"), JobOutcome::Done);
    let sent = env.rest.calls_to("POST", &format!("/channels/{C1}/messages"));
    assert_eq!(sent.len(), 1);
    let content = s(&sent[0].body["content"]);
    assert!(content.starts_with("⏰ **Movie night** starts <t:"), "{content}");
    assert!(content.ends_with(&format!("<@{U1}> <@{U2}>")), "{content}");
    assert_eq!(sent[0].body["allowed_mentions"]["users"], json!([U1, U2]));
    let after = env.event(&event.id).await;
    assert!(after.reminder_sent_at.is_some());
    assert_eq!(after.reminder_message_ids.len(), 1);
    // A retry never pings twice.
    env.rest.clear_calls();
    svc::handle_remind_job(job, env.ctx()).await.expect("job");
    assert!(env.rest.calls_to("POST", "/channels/*/messages").is_empty());
    // A moved event reminds again, and the ids are added to the older ones.
    sqlx::query(r#"UPDATE "GuildEvent" SET "reminderSentAt" = NULL WHERE "id" = $1"#).bind(&event.id).execute(env.t.db()).await.expect("reset");
    svc::handle_remind_job(env.job_of("event.remind", json!({ "eventId": event.id })), env.ctx()).await.expect("job");
    assert_eq!(env.event(&event.id).await.reminder_message_ids.len(), 2);
    env.done().await;
}

#[tokio::test]
async fn the_reminder_skips_closed_started_empty_and_module_off_and_a_cancelled_discord_event_cancels() {
    let Some(env) = Env::new("930000000000000016").await else { return };
    let remind = |id: &str| svc::handle_remind_job(env.job_of("event.remind", json!({ "eventId": id })), env.ctx());
    let now = env.t.clock().now_ms();
    let started = env.seed(|d| d.starts_at = chrono::DateTime::from_timestamp_millis(now - 60_000).unwrap_or_default()).await;
    going(&env.t, env.g, &started.id, U1, RsvpStatus::Going).await;
    remind(&started.id).await.expect("job");
    assert!(env.event(&started.id).await.reminder_sent_at.is_none(), "too late to be a reminder");
    let nobody = env.seed(|_| {}).await;
    remind(&nobody.id).await.expect("job");
    assert!(env.event(&nobody.id).await.reminder_sent_at.is_some(), "claimed, nothing to send");
    let closed = env.seed(|_| {}).await;
    going(&env.t, env.g, &closed.id, U1, RsvpStatus::Going).await;
    svc::close_event(&env.t, &closed.id, GuildEventStatus::Cancelled, svc::CloseReason::Manual, None).await.expect("closed");
    env.rest.clear_calls();
    remind(&closed.id).await.expect("job");
    assert!(env.rest.calls_to("POST", "/channels/*/messages").is_empty());
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventsEnabled", false)).await.expect("settings");
    let off = env.seed(|_| {}).await;
    going(&env.t, env.g, &off.id, U1, RsvpStatus::Going).await;
    remind(&off.id).await.expect("job");
    assert!(env.event(&off.id).await.reminder_sent_at.is_none(), "module off: untouched");
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventsEnabled", true)).await.expect("settings");
    // The Discord event was cancelled while the bot was away.
    let (linked, _) = env.create(|_| {}).await;
    going(&env.t, env.g, &linked.id, U1, RsvpStatus::Going).await;
    env.rest.on("GET", "/guilds/*/scheduled-events/*", 200, scheduled_event_json("830000000000000001", env.g, 4));
    env.rest.clear_calls();
    remind(&linked.id).await.expect("job");
    assert_eq!(env.event(&linked.id).await.status, GuildEventStatus::Cancelled);
    assert!(env.rest.calls_to("POST", &format!("/channels/{C1}/messages")).is_empty(), "no ping");
    assert!(svc::handle_remind_job(env.job_of("event.remind", json!({ "eventId": "" })), env.ctx()).await.is_err());
    env.done().await;
}

#[tokio::test]
async fn edit_event_reschedules_resets_the_reminder_and_updates_the_discord_event_and_the_post() {
    let Some(env) = Env::new("930000000000000017").await else { return };
    let (event, _) = env.create(|_| {}).await;
    sqlx::query(r#"UPDATE "GuildEvent" SET "reminderSentAt" = now() WHERE "id" = $1"#).bind(&event.id).execute(env.t.db()).await.expect("sent");
    let event = env.event(&event.id).await;
    let moved = chrono::DateTime::from_timestamp_millis(event.starts_at.timestamp_millis() + 86_400_000).unwrap_or_default();
    let changes = svc::EventChanges { title: Some("Film night".into()), starts_at: Some(moved), ends_at: Some(None), ..Default::default() };
    env.rest.clear_calls();
    let (updated, notes) = svc::edit_event(&env.t, env.gid(), &event, &changes).await.expect("db").expect("open");
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(updated.title, "Film night");
    assert!(updated.reminder_sent_at.is_none(), "a moved event reminds again");
    assert!(updated.ends_at.is_none());
    let end = env.job(&svc::end_job_key(&event.id)).await.expect("end");
    assert_eq!(end.run_at.timestamp_millis(), moved.timestamp_millis() + 2 * 3_600_000);
    let patch = env.rest.calls_to("PATCH", "/guilds/*/scheduled-events/830000000000000001");
    assert_eq!(patch[0].body["name"], "Film night");
    assert!(patch[0].body.get("scheduled_start_time").is_some());
    assert_eq!(patch[0].reason.as_deref(), Some("Event edited with /event"));
    assert!(!env.rest.calls_to("PATCH", &format!("/channels/{C1}/messages/*")).is_empty(), "the post is rendered now");
    // Only the end moves: the start is not sent again (Discord rejects it for a started event).
    env.rest.clear_calls();
    let end_only = svc::EventChanges {
        ends_at: Some(Some(chrono::DateTime::from_timestamp_millis(moved.timestamp_millis() + 3 * 3_600_000).unwrap_or_default())),
        ..Default::default()
    };
    svc::edit_event(&env.t, env.gid(), &updated, &end_only).await.expect("db").expect("open");
    let patch = env.rest.calls_to("PATCH", "/guilds/*/scheduled-events/*");
    assert!(patch[0].body.get("scheduled_start_time").is_none());
    assert!(patch[0].body.get("scheduled_end_time").is_some());
    // Lowering the capacity below the members going keeps them and says so; a vanished Discord event is unlinked.
    for u in [U1, U2] {
        going(&env.t, env.g, &event.id, u, RsvpStatus::Going).await;
    }
    env.rest.fail("PATCH", "/guilds/*/scheduled-events/*", 404, 10070, "Unknown Guild Scheduled Event");
    let current = env.event(&event.id).await;
    let (_, notes) =
        svc::edit_event(&env.t, env.gid(), &current, &svc::EventChanges { capacity: Some(Some(1)), ..Default::default() }).await.expect("db").expect("open");
    assert_eq!(notes, ["The linked Discord event no longer exists.", "2 members are already going: they keep their place."]);
    assert!(env.event(&event.id).await.discord_event_id.is_none());
    svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Manual, None).await.expect("closed");
    assert!(svc::edit_event(&env.t, env.gid(), &current, &changes).await.expect("db").is_none(), "a closed event is not edited");
    env.done().await;
}

#[tokio::test]
async fn edit_button_opens_the_form_for_the_host_or_staff_and_refuses_others() {
    let Some(env) = Env::new("930000000000000018").await else { return };
    let (event, _) = env.create(|_| {}).await;
    route(&env, button(&env, &format!("event-edit:{}:when", event.id), HOST, &[])).await;
    assert_eq!(env.t.responses.names(), ["showModal"], "nothing is acknowledged before the modal");
    let modal = &env.t.responses.calls()[0].body;
    assert_eq!(modal["custom_id"], format!("event-modal:{}:when", event.id));
    assert_eq!(modal["title"], "Edit date and time");
    route(&env, button(&env, &format!("event-edit:{}:details", event.id), MODR, &[MOD_ROLE])).await;
    assert_eq!(env.t.responses.names(), ["showModal"], "staff may edit any event");
    route(&env, button(&env, &format!("event-edit:{}:details", event.id), U1, &[])).await;
    assert_eq!(env.t.responses.calls()[0].embed_title(), Some("❌  Not Your Event"));
    route(&env, button(&env, &format!("event-edit:{}:nope", event.id), HOST, &[])).await;
    assert_eq!(env.t.responses.calls()[0].embed_title(), Some("❌  Panel Expired"));
    svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Manual, None).await.expect("closed");
    route(&env, button(&env, &format!("event-edit:{}:details", event.id), HOST, &[])).await;
    assert_eq!(env.t.responses.calls()[0].embed_title(), Some("⚠️  Event Closed"));
    env.done().await;
}

fn modal(env: &Env, id: &str, user: &str, fields: &[(&str, &str)], from_panel: bool) -> serenity::all::Interaction {
    let mut f = FakeInteraction::modal(id, fields).guild(env.g).user(user).channel(C1);
    if from_panel {
        f.json_mut()["message"] = json!({
            "id": "100000000000000077", "channel_id": C1, "author": { "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true },
            "content": "", "timestamp": "2025-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0, "flags": 64
        });
    }
    f.build()
}

#[tokio::test]
async fn edit_forms_save_only_their_own_fields_and_update_the_panel_in_place() {
    let Some(env) = Env::new("930000000000000019").await else { return };
    let (event, _) = env.create(|_| {}).await;
    let details = [("event-title", " New title "), ("event-description", ""), ("event-location", "Hall")];
    route(&env, modal(&env, &format!("event-modal:{}:details", event.id), HOST, &details, true)).await;
    assert_eq!(env.t.responses.names(), ["deferUpdate", "editReply"]);
    let body = env.t.responses.calls()[1].body.clone();
    assert_eq!(body["embeds"][0]["title"], "✅  Event Updated");
    assert_eq!(body["embeds"][1]["title"], "✏️ New title", "the panel as it is now");
    let after = env.event(&event.id).await;
    assert_eq!((after.title.as_str(), after.description.clone(), after.location.as_deref()), ("New title", None, Some("Hall")));
    assert_eq!(after.starts_at, event.starts_at, "the other forms' fields are untouched");

    let limits = [("event-capacity", "abc"), ("event-reminder", "15")];
    route(&env, modal(&env, &format!("event-modal:{}:limits", event.id), HOST, &limits, true)).await;
    let body = env.t.responses.calls()[1].body.clone();
    assert_eq!(body["embeds"][0]["title"], "❌  Invalid Capacity");
    assert!(s(&body["embeds"][0]["description"]).ends_with("\nNothing was saved."));
    assert_eq!(body["embeds"].as_array().map(|a| a.len()), Some(2), "the panel stays");

    let limits = [("event-capacity", "5"), ("event-reminder", "0")];
    route(&env, modal(&env, &format!("event-modal:{}:limits", event.id), HOST, &limits, false)).await;
    assert_eq!(env.t.responses.names(), ["deferReply", "editReply"], "a dismissed panel gets a private reply");
    let after = env.event(&event.id).await;
    assert_eq!((after.capacity, after.reminder_minutes), (Some(5), 0));
    assert!(env.job(&svc::remind_job_key(&event.id)).await.is_none(), "reminder 0: the job is cancelled");

    // An untouched Date & time form (the start has seconds) is not a change.
    let (day, time) = svc::format_start_fields(&after.starts_at, "UTC");
    let when = [("event-day", day.as_str()), ("event-time", time.as_str()), ("event-zone", "UTC"), ("event-duration", "2h")];
    route(&env, modal(&env, &format!("event-modal:{}:when", event.id), HOST, &when, true)).await;
    assert_eq!(env.t.responses.calls()[1].body["embeds"][0]["title"], "✅  Event Updated");
    assert_eq!(env.event(&event.id).await.starts_at, after.starts_at);
    let bad_zone = [("event-day", "tomorrow"), ("event-time", "7pm"), ("event-zone", "IST"), ("event-duration", "")];
    route(&env, modal(&env, &format!("event-modal:{}:when", event.id), HOST, &bad_zone, true)).await;
    let desc = s(&env.t.responses.calls()[1].body["embeds"][0]["description"]);
    assert_eq!(
        desc,
        "`IST` can mean several time zones: `Asia/Kolkata`, `Europe/Dublin`, `Asia/Jerusalem`. Type the one you mean.\nNothing was saved."
    );
    route(&env, modal(&env, &format!("event-modal:{}:details", event.id), U1, &details, true)).await;
    assert_eq!(env.t.responses.calls()[1].body["embeds"][0]["title"], "❌  Not Your Event");
    env.done().await;
}

#[tokio::test]
async fn the_hooks_forget_a_post_cancel_on_channel_delete_clear_roles_and_follow_discord_cancels() {
    let Some(env) = Env::new("930000000000000020").await else { return };
    let (event, _) = env.create(|d| d.role_id = Some(R1.into())).await;
    let msg = event.message_id.clone().unwrap_or_default();
    // messageDelete through the hook runner (the filter passes an uncached message).
    env.t
        .run_hooks(MessageDelete {
            guild_id: env.gid(),
            message_id: serenity::all::MessageId::new(msg.parse().unwrap_or(1)),
            channel_id: ChannelId::new(C1.parse().unwrap_or(1)),
            message: None,
            partial: true,
            channel: None,
        })
        .await;
    let after = env.event(&event.id).await;
    assert!(after.message_id.is_none());
    assert!(svc::is_open(Some(&after)), "the event stays");
    svc::on_role_delete(&env.t, env.g, R1).await.expect("hook");
    assert!(env.event(&event.id).await.role_id.is_none());
    // The Discord event cancelled in Discord: before the start, the event is cancelled.
    svc::on_discord_event_cancelled(&env.t, env.g, "830000000000000001").await.expect("hook");
    assert_eq!(env.event(&event.id).await.status, GuildEventStatus::Cancelled);
    // channelDelete: open events posted there are cancelled; others are untouched.
    let (posted, _) = env.create(|_| {}).await;
    let elsewhere = env.seed(|_| {}).await;
    sqlx::query(r#"UPDATE "GuildEvent" SET "channelId" = '810000000000000555' WHERE "id" = $1"#).bind(&elsewhere.id).execute(env.t.db()).await.expect("move");
    svc::on_channel_delete(&env.t, env.g, C1).await.expect("hook");
    assert_eq!(env.event(&posted.id).await.status, GuildEventStatus::Cancelled);
    assert!(svc::is_open(Some(&env.event(&elsewhere.id).await)));
    env.done().await;
}

#[tokio::test]
async fn autocomplete_and_event_references() {
    let Some(env) = Env::new("930000000000000021").await else { return };
    let a = env.seed(|d| d.title = "Movie night".into()).await;
    let b = env
        .seed(|d| {
            d.title = "Game night".into();
            d.host_id = U1.into();
        })
        .await;
    let c = env.seed(|d| d.title = "Quiz".into()).await;
    svc::close_event(&env.t, &c.id, GuildEventStatus::Ended, svc::CloseReason::Manual, None).await.expect("closed");
    let ids = |rows: Vec<svc::EventChoice>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    let edit = ids(svc::find_events_for_autocomplete(&env.t, env.g, "edit", "", None).await.expect("db"));
    assert_eq!(edit.len(), 2);
    assert!(!edit.contains(&c.id));
    assert_eq!(ids(svc::find_events_for_autocomplete(&env.t, env.g, "info", "", None).await.expect("db")).len(), 3);
    assert_eq!(ids(svc::find_events_for_autocomplete(&env.t, env.g, "edit", "NIGHT", Some(U1)).await.expect("db")), std::slice::from_ref(&b.id));
    assert_eq!(ids(svc::find_events_for_autocomplete(&env.t, env.g, "info", "qu", None).await.expect("db")), std::slice::from_ref(&c.id));
    // resolveEventRef: an id of this guild, or a link to its post; nothing from another guild.
    assert_eq!(svc::resolve_event_ref(&env.t, env.g, &a.id).await.expect("db").map(|e| e.id), Some(a.id.clone()));
    assert!(svc::resolve_event_ref(&env.t, "930000000000000098", &a.id).await.expect("db").is_none());
    sqlx::query(r#"UPDATE "GuildEvent" SET "messageId" = '710000000000000001' WHERE "id" = $1"#).bind(&a.id).execute(env.t.db()).await.expect("msg");
    let link = format!("https://discord.com/channels/{}/{C1}/710000000000000001", env.g);
    assert_eq!(svc::resolve_event_ref(&env.t, env.g, &link).await.expect("db").map(|e| e.id), Some(a.id.clone()));
    let other = format!("https://discord.com/channels/930000000000000098/{C1}/710000000000000001");
    assert!(svc::resolve_event_ref(&env.t, env.g, &other).await.expect("db").is_none());
    assert!(svc::resolve_event_ref(&env.t, env.g, "  ").await.expect("db").is_none());
    // Through the router: a member below staff sees only their own events for edit.
    env.t.responses.clear();
    let ac = FakeInteraction::autocomplete("event").guild(env.g).user(U1).subcommand("edit").focused("event", "").build();
    bot_core::router::route_interaction(&env.t, ac).await;
    let choices = env.t.responses.calls()[0].choices();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0]["value"], json!(b.id));
    assert!(s(&choices[0]["name"]).starts_with("🟢 Game night · "));
    env.done().await;
}

#[tokio::test]
async fn event_channels_chat_and_role_voice_job_and_cleanup() {
    let Some(env) = Env::new("930000000000000022").await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    let (event, notes) = env.create(|_| {}).await;
    assert!(notes.is_empty(), "{notes:?}");
    let created = env.rest.calls_to("POST", "/guilds/*/channels");
    assert_eq!(created[0].body, json!({ "name": "Events", "type": 4 }), "a new Events category");
    assert_eq!(created[1].body["name"], "movie-night");
    assert_eq!(created[1].body["type"], 0);
    let ows = created[1].body["permission_overwrites"].as_array().cloned().unwrap_or_default();
    assert_eq!(ows[0]["deny"], "1024", "@everyone cannot see it");
    let role = env.rest.calls_to("POST", "/guilds/*/roles");
    assert_eq!(role[0].body["name"], "Event: Movie night");
    assert!(event.text_channel_id.is_some() && event.access_role_id.is_some());
    let st = env.t.settings().get_settings(env.g).await.expect("settings");
    assert!(st.event_category_id.is_some(), "the category is remembered");
    let access = event.access_role_id.clone().unwrap_or_default();
    assert_eq!(env.rest.calls_to("PUT", &format!("/guilds/{}/members/{HOST}/roles/{access}", env.g)).len(), 1, "the host is in");
    let chat = env.rest.calls_to("POST", &format!("/channels/{}/messages", event.text_channel_id.clone().unwrap_or_default()));
    assert!(s(&chat[0].body["content"]).starts_with("💬 This is the chat for **Movie night**."));
    let voice_job = env.job(&svc::voice_job_key(&event.id)).await.expect("voice job");
    assert_eq!(voice_job.run_at.timestamp_millis(), event.starts_at.timestamp_millis() - 30 * 60_000);

    // The voice job opens the channel beside the chat.
    env.rest.clear_calls();
    assert_eq!(svc::handle_voice_job(env.job_of("event.voice", json!({ "eventId": event.id })), env.ctx()).await.expect("job"), JobOutcome::Done);
    let voice = env.rest.calls_to("POST", "/guilds/*/channels");
    assert_eq!(voice[0].body["type"], 2);
    assert_eq!(voice[0].body["name"], "Movie night");
    let opened = env.event(&event.id).await;
    let voice_id = opened.voice_channel_id.clone().expect("voice linked");
    assert!(s(&env.rest.calls_to("POST", "/channels/*/messages")[0].body["content"]).starts_with("🔊 The voice channel is open: <#"));

    // A Going member gets the access role; Can't go loses it.
    route(&env, button(&env, &format!("event:{}:going", event.id), U1, &[])).await;
    assert!(env.t.responses.calls()[1].content().unwrap_or_default().contains("💬 Event chat: <#"));
    assert_eq!(env.rest.calls_to("PUT", &format!("/guilds/{}/members/{U1}/roles/{access}", env.g)).len(), 1);
    route(&env, button(&env, &format!("event:{}:declined", event.id), U1, &[&access])).await;
    assert_eq!(env.rest.calls_to("DELETE", &format!("/guilds/{}/members/{U1}/roles/{access}", env.g)).len(), 1);

    // Closing queues the cleanup; the empty voice channel goes, then the chat, the role and the post.
    svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Manual, None).await.expect("closed");
    assert!(env.job(&svc::cleanup_job_key(&event.id)).await.is_some());
    env.rest.clear_calls();
    assert_eq!(svc::handle_cleanup_job(env.job_of("event.cleanup", json!({ "eventId": event.id })), env.ctx()).await.expect("job"), JobOutcome::Done);
    assert_eq!(env.rest.calls_to("DELETE", &format!("/channels/{voice_id}")).len(), 1);
    assert_eq!(env.rest.calls_to("DELETE", &format!("/channels/{}", opened.text_channel_id.clone().unwrap_or_default())).len(), 1);
    assert_eq!(env.rest.calls_to("DELETE", &format!("/guilds/{}/roles/{access}", env.g)).len(), 1);
    assert_eq!(env.rest.calls_to("DELETE", &format!("/channels/{C1}/messages/*")).len(), 1, "the post goes last");
    let done = env.event(&event.id).await;
    assert!(done.voice_channel_id.is_none() && done.text_channel_id.is_none() && done.access_role_id.is_none() && done.message_id.is_none());
    assert!(bot_core::deletion_tracker::was_deleted_by_bot(&env.t, &voice_id).is_some(), "logging is told it was the bot");
    env.done().await;
}

#[tokio::test]
async fn cleanup_waits_while_people_are_in_the_voice_channel() {
    const VOICE: &str = "810000000000000009";
    let guild = fake_guild("930000000000000023", bot_perms()).channel(VOICE, 2, "Movie night", &[]).voice_state(U1, VOICE);
    let Some(env) = Env::with("930000000000000023", guild).await else { return };
    let event = env.seed(|_| {}).await;
    sqlx::query(r#"UPDATE "GuildEvent" SET "voiceChannelId" = $2, "status" = 'ENDED' WHERE "id" = $1"#)
        .bind(&event.id)
        .bind(VOICE)
        .execute(env.t.db())
        .await
        .expect("link");
    let out = svc::handle_cleanup_job(env.job_of("event.cleanup", json!({ "eventId": event.id })), env.ctx()).await.expect("job");
    assert!(matches!(out, JobOutcome::Reschedule { payload: None, .. }));
    assert!(env.rest.calls_to("DELETE", "/channels/*").is_empty());
    assert!(svc::is_waiting_voice(&env.t, VOICE));
    // An open event is skipped.
    let open = env.seed(|_| {}).await;
    assert_eq!(svc::handle_cleanup_job(env.job_of("event.cleanup", json!({ "eventId": open.id })), env.ctx()).await.expect("job"), JobOutcome::Done);
    env.done().await;
}

#[tokio::test]
async fn event_commands_through_the_router() {
    let Some(env) = Env::new("930000000000000024").await else { return };
    let slash = |sub: &str| FakeInteraction::slash("event").guild(env.g).user(U1).channel(C1).subcommand(sub);
    use bot_core::command::option_type as ot;
    let create = slash("create").option("title", ot::STRING, json!("Board games")).option("start", ot::STRING, json!("in 3d")).build();
    route(&env, create).await;
    let calls = env.t.responses.calls();
    assert_eq!(env.t.responses.names(), ["deferReply", "editReply"]);
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Event Created"));
    let desc = calls[1].embed_description().unwrap_or_default().to_string();
    assert!(desc.starts_with("**Board games** is posted in <#810000000000000001>: [jump to it](https://discord.com/channels/"), "{desc}");
    assert!(desc.contains("The start was read in UTC, this server's timezone."), "{desc}");
    assert!(desc.contains("Members who are going are pinged 15 minutes before the start."), "{desc}");
    let past = slash("create").option("title", ot::STRING, json!("Old")).option("start", ot::STRING, json!("2020-01-01 10:00")).build();
    route(&env, past).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("❌  Start In The Past"));
    let locked =
        slash("create").option("title", ot::STRING, json!("X")).option("start", ot::STRING, json!("in 3d")).option("channel", ot::CHANNEL, json!(LOCKED)).build();
    route(&env, locked).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("❌  Cannot Post There"));
    assert_eq!(env.t.responses.calls()[1].embed_description(), Some("You cannot send messages in <#810000000000000002>."));
    let role = slash("create").option("title", ot::STRING, json!("X")).option("start", ot::STRING, json!("in 3d")).option("role", ot::ROLE, json!(R1)).build();
    route(&env, role).await;
    assert_eq!(env.t.responses.calls()[1].embed_description(), Some("You need the **Manage Roles** permission to attach a role to an event."));

    // list, attendees, info, cancel: private replies.
    route(&env, slash("list").build()).await;
    assert!(env.t.responses.calls()[1].embed_title().unwrap_or_default().starts_with("📅 Open events (1/25)"));
    let (events, _) = svc::list_open_events(&env.t, env.g).await.expect("list");
    let id = events[0].id.clone();
    route(&env, slash("attendees").option("event", ot::STRING, json!(id)).build()).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("ℹ️  No RSVPs Yet"));
    going(&env.t, env.g, &id, U2, RsvpStatus::Maybe).await;
    route(&env, slash("attendees").option("event", ot::STRING, json!(id)).build()).await;
    let want = format!("**✅ 0 going · ❔ 1 maybe · ❌ 0 can't go**\n\n❔ <@{U2}>");
    assert_eq!(env.t.responses.calls()[1].embed_description(), Some(want.as_str()));
    route(&env, slash("info").option("event", ot::STRING, json!(id)).build()).await;
    let info = env.t.responses.calls()[1].embed_description().unwrap_or_default().to_string();
    assert!(info.starts_with("🟢 **Scheduled**\n**When:** <t:"), "{info}");
    assert!(info.contains("**Going:** 0 · **Maybe:** 1 · **Can't go:** 0"));
    route(&env, FakeInteraction::slash("event").guild(env.g).user(U2).subcommand("cancel").option("event", ot::STRING, json!(id)).build()).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("❌  Not Your Event"));
    route(&env, slash("cancel").option("event", ot::STRING, json!(id)).build()).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("✅  Event Cancelled"));
    route(&env, slash("cancel").option("event", ot::STRING, json!(id)).build()).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("⚠️  Event Closed"));
    route(&env, slash("edit").option("event", ot::STRING, json!("cnope")).build()).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("❌  Event Not Found"));
    env.done().await;
}

#[tokio::test]
async fn event_create_fetches_a_picked_channel_the_cache_lacks() {
    // JS: `guild.channels.cache.get(picked.id) ?? picked`: an uncached picked channel is still judged and used.
    let Some(env) = Env::new("930000000000000031").await else { return };
    const UNCACHED: &str = "810000000000000077";
    env.rest.on(
        "GET",
        &format!("/channels/{UNCACHED}"),
        200,
        json!({ "id": UNCACHED, "type": 0, "guild_id": env.g, "name": "late", "position": 3, "permission_overwrites": [], "nsfw": false }),
    );
    use bot_core::command::option_type as ot;
    let create = FakeInteraction::slash("event")
        .guild(env.g)
        .user(U1)
        .channel(C1)
        .subcommand("create")
        .option("title", ot::STRING, json!("Quiz"))
        .option("start", ot::STRING, json!("in 3d"))
        .option("channel", ot::CHANNEL, json!(UNCACHED))
        .build();
    route(&env, create).await;
    let calls = env.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Event Created"), "{:?}", calls[1].embed_description());
    let desc = calls[1].embed_description().unwrap_or_default().to_string();
    assert!(desc.starts_with(&format!("**Quiz** is posted in <#{UNCACHED}>")), "{desc}");
    assert_eq!(env.rest.calls_to("POST", &format!("/channels/{UNCACHED}/messages")).len(), 1);
    env.done().await;
}

#[tokio::test]
async fn create_event_retry_failure_after_the_overdue_sweep_answers_the_first_limit() {
    // JS createEvent: the sweep and the second createEventRow share one try/catch; a failing retry is only warned
    // about and the answer is the first try's LIMIT.
    let Some(env) = Env::new("930000000000000032").await else { return };
    let now = env.t.clock().now_ms();
    let past = |m: i64| chrono::DateTime::from_timestamp_millis(now - m).unwrap_or_default();
    env.seed(|d| {
        d.starts_at = past(3 * 3_600_000);
        d.ends_at = Some(past(2 * 3_600_000));
    })
    .await;
    for _ in 1..svc::consts::ACTIVE_MAX {
        env.seed(|_| {}).await;
    }
    let settings = env.t.settings().get_settings_safe(env.g).await;
    // A NUL byte makes PostgreSQL refuse the INSERT of the second try (the first try stops at the cap before it).
    let data = env.data(|d| d.title = "bad\u{0}title".into());
    let out = svc::create_event(&env.t, env.gid(), ChannelId::new(C1.parse().unwrap_or(1)), settings.as_deref(), &data, None).await;
    assert!(matches!(out, Ok(Err(svc::CreateFailure::Limit))), "{out:?}");
    env.done().await;
}

#[tokio::test]
async fn raw_sql_runs_in_a_rolled_back_transaction() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query(svc::CREATE_LOCK_SQL).bind("event-create:0").execute(&mut *tx).await.expect("advisory lock");
    let row: Option<(String, String, String, Option<i32>)> = sqlx::query_as(svc::EVENT_LOCK_SQL).bind("0").fetch_optional(&mut *tx).await.expect("row lock");
    assert!(row.is_none());
    tx.rollback().await.expect("rollback");
}

// ── Cross-language ────────────────────────────────────────────────────────────

const JS: &str = "rust/fixtures/gen/feat-events-js.mjs";

#[tokio::test]
async fn js_and_rust_read_each_others_events_rsvps_and_jobs() {
    let Some(env) = Env::new("930000000000000025").await else { return };
    let g = env.g;
    // JS writes: an event, RSVPs, the jobs; Rust reads them.
    let js = run_node(
        JS,
        &[
            "create",
            g,
            r#"{"title":"JS night","hostId":"600000000000000001","channelId":"810000000000000001","startsAt":"2031-01-01T10:00:00Z","endsAt":"2031-01-01T12:00:00Z","reminderMinutes":30,"capacity":5,"location":"Hall"}"#,
        ],
    );
    assert_eq!(js["ok"], true, "{js}");
    let id = s(&js["event"]["id"]);
    for (u, st) in [(U1, "GOING"), (U2, "MAYBE"), (HOST, "GOING")] {
        assert_eq!(run_node(JS, &["rsvp", &id, g, u, st])["result"], "set");
    }
    let row = env.event(&id).await;
    let mut want = js["event"].clone();
    want["updatedAt"] = to_json(&row)["updatedAt"].clone();
    assert_eq!(to_json(&row), want, "the row reads the same");
    let tally = svc::get_tally(&env.t, &id).await.expect("tally");
    let js_tally = run_node(JS, &["tally", &id]);
    assert_eq!(json!({ "going": tally.going, "maybe": tally.maybe, "declined": tally.declined, "goingIds": tally.going_ids }), js_tally);
    assert_eq!(svc::apply_rsvp(&env.t, &id, g, U1, RsvpStatus::Going).await.expect("rsvp").result, RsvpResult::Same, "JS wrote GOING");
    let scheduled = run_node(JS, &["schedule", &id]);
    assert_eq!(scheduled["failed"], false);
    let end = env.job(&svc::end_job_key(&id)).await.expect("JS end job");
    assert_eq!((end.r#type.as_str(), end.payload.clone()), ("event.end", json!({ "eventId": id })));
    let remind = env.job(&svc::remind_job_key(&id)).await.expect("JS remind job");
    assert_eq!(remind.payload, json!({ "eventId": id }));
    // The Rust handler runs the JS job.
    sqlx::query(r#"UPDATE "GuildEvent" SET "startsAt" = now() - interval '3 hours', "endsAt" = NULL WHERE "id" = $1"#).bind(&id).execute(env.t.db()).await.expect("end passed");
    let job = bot_core::scheduler::Job::from_row(&end);
    assert_eq!(svc::handle_end_job(job, env.ctx()).await.expect("job"), JobOutcome::Done);
    assert_eq!(env.event(&id).await.status, GuildEventStatus::Ended);
    let (open, _) = svc::list_open_events(&env.t, g).await.expect("list");
    assert!(open.is_empty());

    // Rust writes: an event, RSVPs (the ordering column too), jobs; the JS reads them.
    let (event, _) = env.create(|d| d.capacity = Some(3)).await;
    for u in [U2, U1] {
        going(&env.t, g, &event.id, u, RsvpStatus::Going).await;
    }
    going(&env.t, g, &event.id, HOST, RsvpStatus::Declined).await;
    svc::mark_role_granted(&env.t, &event.id, U2, true).await.expect("flag");
    let back = run_node(JS, &["find", &event.id]);
    let mut ours = to_json(&env.event(&event.id).await);
    let mut theirs = back.clone();
    if let Some(m) = theirs.as_object_mut() {
        m.remove("rsvps");
    }
    if let Some(m) = ours.as_object_mut() {
        m.remove("rsvps");
    }
    assert_eq!(ours, theirs, "the row reads the same through Prisma and Rust");
    let rsvps: Vec<(String, String, bool)> =
        back["rsvps"].as_array().into_iter().flatten().map(|r| (s(&r["userId"]), s(&r["status"]), r["roleGranted"] == json!(true))).collect();
    assert_eq!(rsvps, [(HOST.into(), "DECLINED".into(), false), (U1.into(), "GOING".into(), false), (U2.into(), "GOING".into(), true)]);
    let js_tally = run_node(JS, &["tally", &event.id]);
    assert_eq!(js_tally["goingIds"], json!([U1, U2]), "U2's flag bumped its updatedAt, as Prisma's updateMany does");
    assert_eq!(run_node(JS, &["rsvp", &event.id, g, "600000000000000040", "GOING"])["result"], "set");
    assert_eq!(run_node(JS, &["rsvp", &event.id, g, "600000000000000041", "GOING"])["result"], "full", "the JS sees the capacity Rust stored");
    let list = run_node(JS, &["list", g]);
    assert_eq!(list["ids"], json!([event.id]));
    assert_eq!(list["going"][&event.id], 3);
    let jobs = run_node(JS, &["jobs", g]);
    let keys: Vec<String> = jobs.as_array().into_iter().flatten().map(|j| s(&j["dedupeKey"])).collect();
    assert!(keys.contains(&svc::end_job_key(&event.id)) && keys.contains(&svc::remind_job_key(&event.id)), "{keys:?}");
    for j in jobs.as_array().into_iter().flatten().filter(|j| s(&j["dedupeKey"]).ends_with(&event.id)) {
        assert_eq!(j["payload"], json!({ "eventId": event.id }));
    }
    assert_eq!(run_node(JS, &["ref", g, &event.id]), json!(event.id));
    let auto = run_node(JS, &["auto", g, "edit", "mov", "-"]);
    let rust_auto: Vec<String> = svc::find_events_for_autocomplete(&env.t, g, "edit", "mov", None).await.expect("db").into_iter().map(|r| r.id).collect();
    assert_eq!(auto, json!(rust_auto));
    // Prisma's `contains` treats % and _ as wildcards, and so does the Rust query.
    let wild = run_node(JS, &["auto", g, "info", "m%n_ght", "-"]);
    let rust_wild: Vec<String> =
        svc::find_events_for_autocomplete(&env.t, g, "info", "m%n_ght", None).await.expect("db").into_iter().map(|r| r.id).collect();
    assert_eq!(wild, json!(rust_wild));
    // The JS closes an event Rust made; Rust sees one winner.
    let closed = run_node(JS, &["close", &event.id, g, "CANCELLED"]);
    assert_eq!(closed["ok"], true, "{closed}");
    assert!(matches!(
        svc::close_event(&env.t, &event.id, GuildEventStatus::Ended, svc::CloseReason::Timer, Some(g)).await.expect("db"),
        svc::CloseOutcome::AlreadyClosed(_)
    ));
    env.done().await;
}
