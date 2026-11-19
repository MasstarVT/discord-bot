//! Event channels in depth: the voice job's skips and failures, the cleanup's failure branches, the waiting voice
//! channel, category sharing, the chat's refusals and rollback, renames, access repair, and more of `/event create`.

use super::*;

const TEXT: &str = "810000000000000011";
const VOICE: &str = "810000000000000012";
const ACCESS: &str = "820000000000000011";

fn channel_guild(g: &str) -> FakeGuild {
    fake_guild(g, bot_perms()).role(ACCESS, 1, P::empty()).channel(TEXT, 0, "movie-night", &[]).channel(VOICE, 2, "Movie night", &[])
}

async fn link(env: &Env, id: &str, text: Option<&str>, voice: Option<&str>, access: Option<&str>, status: &str) {
    sqlx::query(
        r#"UPDATE "GuildEvent" SET "textChannelId" = $2, "voiceChannelId" = $3, "accessRoleId" = $4, "status" = $5::"GuildEventStatus" WHERE "id" = $1"#,
    )
    .bind(id)
    .bind(text)
    .bind(voice)
    .bind(access)
    .bind(status)
    .execute(env.t.db())
    .await
    .expect("link");
}

#[tokio::test]
async fn the_voice_job_skips_closed_off_lost_and_late_events() {
    let Some(env) = Env::with("930000000000000030", channel_guild("930000000000000030")).await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    let voice = |id: &str| svc::handle_voice_job(env.job_of("event.voice", json!({ "eventId": id })), env.ctx());
    let none_created = |env: &Env| env.rest.calls_to("POST", "/guilds/*/channels").is_empty();
    let closed = env.seed(|_| {}).await;
    link(&env, &closed.id, Some(TEXT), None, Some(ACCESS), "ENDED").await;
    assert_eq!(voice(&closed.id).await.expect("job"), JobOutcome::Done);
    let no_role = env.seed(|_| {}).await;
    link(&env, &no_role.id, Some(TEXT), None, None, "SCHEDULED").await;
    voice(&no_role.id).await.expect("job");
    let now = env.t.clock().now_ms();
    let late = env
        .seed(|d| {
            d.starts_at = chrono::DateTime::from_timestamp_millis(now - 3 * 3_600_000).unwrap_or_default();
            d.ends_at = chrono::DateTime::from_timestamp_millis(now - 3_600_000);
        })
        .await;
    link(&env, &late.id, Some(TEXT), None, Some(ACCESS), "SCHEDULED").await;
    voice(&late.id).await.expect("job");
    assert!(none_created(&env));
    let open = env.seed(|_| {}).await;
    link(&env, &open.id, Some(TEXT), None, Some(ACCESS), "SCHEDULED").await;
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", false)).await.expect("settings");
    voice(&open.id).await.expect("job");
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true).set("eventsEnabled", false)).await.expect("settings");
    voice(&open.id).await.expect("job");
    assert!(none_created(&env), "module or channels off: nothing");
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventsEnabled", true)).await.expect("settings");
    // A lost chat (Discord answers Unknown Channel) is forgotten.
    let lost = env.seed(|_| {}).await;
    link(&env, &lost.id, Some("810000000000000444"), None, Some(ACCESS), "SCHEDULED").await;
    voice(&lost.id).await.expect("job");
    assert!(env.event(&lost.id).await.text_channel_id.is_none());
    assert!(none_created(&env));
    env.done().await;
}

#[tokio::test]
async fn the_voice_job_reports_refusals_retries_failures_and_loses_to_a_close() {
    let Some(env) = Env::with("930000000000000031", channel_guild("930000000000000031")).await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    let event = env.seed(|_| {}).await;
    link(&env, &event.id, Some(TEXT), None, Some(ACCESS), "SCHEDULED").await;
    let job = || env.job_of("event.voice", json!({ "eventId": event.id }));
    env.rest.fail("POST", "/guilds/*/channels", 400, 30013, "Maximum number of guild channels reached");
    assert_eq!(svc::handle_voice_job(job(), env.ctx()).await.expect("job"), JobOutcome::Done);
    let told = env.rest.calls_to("POST", &format!("/channels/{TEXT}/messages"));
    assert!(s(&told[0].body["content"]).starts_with("⚠️ I could not open the voice channel ("), "{:?}", told[0].body);
    env.rest.fail("POST", "/guilds/*/channels", 502, 0, "Bad Gateway");
    assert!(svc::handle_voice_job(job(), env.ctx()).await.is_err(), "Discord's own failures retry");
    // A close that lands while the channel is being made wins: the new channel is removed again.
    let url = database_url().unwrap_or_default();
    let (id, gg) = (event.id.clone(), env.g.to_string());
    env.rest.on_with("POST", "/guilds/*/channels", move |call| {
        let (url, id) = (url.clone(), id.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(async {
                use sqlx::Connection;
                let mut c = sqlx::PgConnection::connect(&url).await.expect("connect");
                sqlx::query(r#"UPDATE "GuildEvent" SET "status" = 'ENDED' WHERE "id" = $1"#).bind(&id).execute(&mut c).await.expect("close");
            });
        })
        .join();
        let body = json!({
            "id": "810000000000000555", "type": call.body["type"], "guild_id": gg, "name": call.body["name"], "position": 0,
            "permission_overwrites": [], "nsfw": false
        });
        (200, body)
    });
    assert_eq!(svc::handle_voice_job(job(), env.ctx()).await.expect("job"), JobOutcome::Done);
    assert_eq!(env.rest.calls_to("DELETE", "/channels/810000000000000555").len(), 1);
    assert!(env.event(&event.id).await.voice_channel_id.is_none());
    env.done().await;

    // A missing permission in the category is told in the chat.
    let g = "930000000000000032";
    let guild = fake_guild(g, bot_perms() - P::MANAGE_CHANNELS).role(ACCESS, 1, P::empty()).channel(TEXT, 0, "movie-night", &[]);
    let Some(env) = Env::with(g, guild).await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    let event = env.seed(|_| {}).await;
    link(&env, &event.id, Some(TEXT), None, Some(ACCESS), "SCHEDULED").await;
    svc::handle_voice_job(env.job_of("event.voice", json!({ "eventId": event.id })), env.ctx()).await.expect("job");
    let told = env.rest.calls_to("POST", &format!("/channels/{TEXT}/messages"));
    assert_eq!(
        s(&told[0].body["content"]),
        "⚠️ I could not open the voice channel: I need **Manage Channels**, **Manage Roles**, **Connect** and **Speak** in this category."
    );
    env.done().await;
}

#[tokio::test]
async fn the_cleanup_keeps_what_it_may_not_delete_and_retries_other_failures() {
    let Some(env) = Env::with("930000000000000033", channel_guild("930000000000000033")).await else { return };
    let cleanup = |id: &str| svc::handle_cleanup_job(env.job_of("event.cleanup", json!({ "eventId": id })), env.ctx());
    // A refused delete (Missing Permissions) stays linked, and the post stays with it.
    let (refused, _) = env.create(|_| {}).await;
    link(&env, &refused.id, Some(TEXT), None, Some(ACCESS), "ENDED").await;
    env.rest.fail("DELETE", &format!("/channels/{TEXT}"), 403, 50013, "Missing Permissions");
    assert_eq!(cleanup(&refused.id).await.expect("job"), JobOutcome::Done);
    let after = env.event(&refused.id).await;
    assert_eq!(after.text_channel_id.as_deref(), Some(TEXT));
    assert!(after.access_role_id.is_none(), "the role still goes");
    assert!(after.message_id.is_some(), "the post stays while a channel could not be deleted");
    assert!(env.rest.calls_to("DELETE", &format!("/channels/{C1}/messages/*")).is_empty());
    // A channel already gone counts as deleted; a post already gone still finishes.
    env.rest.fail("DELETE", &format!("/channels/{TEXT}"), 404, 10003, "Unknown Channel");
    env.rest.fail("DELETE", &format!("/channels/{C1}/messages/*"), 404, 10008, "Unknown Message");
    sqlx::query(r#"UPDATE "GuildEvent" SET "reminderMessageIds" = ARRAY['720000000000000001'] WHERE "id" = $1"#)
        .bind(&refused.id)
        .execute(env.t.db())
        .await
        .expect("reminders");
    assert_eq!(cleanup(&refused.id).await.expect("job"), JobOutcome::Done);
    let done = env.event(&refused.id).await;
    assert!(done.text_channel_id.is_none() && done.message_id.is_none() && done.reminder_message_ids.is_empty());
    assert_eq!(env.rest.calls_to("DELETE", &format!("/channels/{C1}/messages/*")).len(), 2, "the post and the reminder");
    // Anything else retries.
    let (other, _) = env.create(|_| {}).await;
    link(&env, &other.id, None, Some(VOICE), None, "CANCELLED").await;
    env.rest.fail("DELETE", &format!("/channels/{VOICE}"), 500, 0, "Internal Server Error");
    assert!(cleanup(&other.id).await.is_err());
    assert_eq!(env.event(&other.id).await.voice_channel_id.as_deref(), Some(VOICE));
    env.done().await;
}

#[tokio::test]
async fn the_last_human_leaving_a_waiting_voice_channel_queues_the_cleanup_after_a_grace() {
    let guild = channel_guild("930000000000000034").voice_state(U1, VOICE);
    let Some(env) = Env::with("930000000000000034", guild).await else { return };
    let event = env.seed(|_| {}).await;
    link(&env, &event.id, Some(TEXT), Some(VOICE), Some(ACCESS), "ENDED").await;
    let out = svc::handle_cleanup_job(env.job_of("event.cleanup", json!({ "eventId": event.id })), env.ctx()).await.expect("job");
    assert!(matches!(out, JobOutcome::Reschedule { .. }));
    assert!(svc::is_waiting_voice(&env.t, VOICE));
    // U1 leaves: the cache sees the voice channel empty, then the gateway runs the hooks.
    let state = json!({
        "guild_id": env.g, "channel_id": null, "user_id": U1, "session_id": "s", "deaf": false, "mute": false, "self_deaf": false,
        "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null
    });
    let mut ev: serenity::all::VoiceStateUpdateEvent = serde_json::from_value(state.clone()).expect("voice event");
    if let Some(d) = env.t.discord() {
        d.cache.update(&mut ev);
    }
    let mut old = state.clone();
    old["channel_id"] = json!(VOICE);
    let old_state: serenity::all::VoiceState = serde_json::from_value(old).expect("old state");
    let new_state: serenity::all::VoiceState = serde_json::from_value(state).expect("new state");
    let member = bot_core::interaction_helpers::cached_guild(&env.t, env.gid())
        .and_then(|g| g.members.values().find(|m| m.user.id.to_string() == U1).cloned())
        .expect("member");
    let before = chrono::Utc::now().timestamp_millis();
    env.t.run_hooks(VoiceStateUpdate { guild_id: env.gid(), old_state: Some(old_state), new_state, member, change: VoiceChange::Leave }).await;
    assert!(!svc::is_waiting_voice(&env.t, VOICE));
    let job = env.job(&svc::cleanup_job_key(&event.id)).await.expect("cleanup queued");
    let at = job.run_at.timestamp_millis();
    assert!(at >= before + 59_000 && at <= before + 70_000, "about a minute later: {}", at - before);
    env.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_events_at_once_share_one_category_and_an_existing_one_is_used() {
    let Some(env) = Env::new("930000000000000035").await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    let (a, b) = tokio::join!(env.create(|_| {}), env.create(|d| d.title = "Second".into()));
    let cats: Vec<_> = env.rest.calls_to("POST", "/guilds/*/channels").into_iter().filter(|c| c.body["type"] == 4).collect();
    assert_eq!(cats.len(), 1, "one category made");
    let parents: Vec<Value> = env.rest.calls_to("POST", "/guilds/*/channels").into_iter().filter(|c| c.body["type"] == 0).map(|c| c.body["parent_id"].clone()).collect();
    assert_eq!(parents.len(), 2);
    assert_eq!(parents[0], parents[1], "both chats in it");
    assert!(a.0.text_channel_id.is_some() && b.0.text_channel_id.is_some());
    env.done().await;

    // An existing category called Events is used (and remembered).
    let g = "930000000000000036";
    let Some(env) = Env::with(g, fake_guild(g, bot_perms()).channel("810000000000000077", 4, "events", &[])).await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    env.create(|_| {}).await;
    let made = env.rest.calls_to("POST", "/guilds/*/channels");
    assert!(made.iter().all(|c| c.body["type"] != 4));
    assert_eq!(made[0].body["parent_id"], "810000000000000077");
    let st = env.t.settings().get_settings(env.g).await.expect("settings");
    assert_eq!(st.event_category_id.as_deref(), Some("810000000000000077"));
    env.done().await;
}

#[tokio::test]
async fn a_chat_refusal_is_a_note_and_a_failed_post_takes_the_chat_back() {
    let g = "930000000000000037";
    // Manage Roles is denied in the category: no chat, a note, the event stands.
    let guild = fake_guild(g, bot_perms()).channel("810000000000000078", 4, "Events", &[(BOT_ROLE, false, P::empty(), P::MANAGE_ROLES)]);
    let Some(env) = Env::with(g, guild).await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    let (event, notes) = env.create(|_| {}).await;
    assert_eq!(
        notes,
        ["No event channels were made: I need **Manage Channels**, **Manage Roles**, **Send Messages** and **Read Message History** in <#810000000000000078>."]
    );
    assert!(event.text_channel_id.is_none() && event.message_id.is_some());
    env.done().await;

    let Some(env) = Env::new("930000000000000038").await else { return };
    env.t.settings().update_settings(env.g, SettingsPatch::new().set("eventChannelsEnabled", true)).await.expect("settings");
    env.rest.fail("POST", &format!("/channels/{C1}/messages"), 403, 50001, "Missing Access");
    let settings = env.t.settings().get_settings_safe(env.g).await;
    let out = svc::create_event(&env.t, env.gid(), ChannelId::new(C1.parse().unwrap_or(1)), settings.as_deref(), &env.data(|_| {}), None).await.expect("db");
    assert_eq!(out.err(), Some(svc::CreateFailure::Send));
    let chat = env.rest.calls_to("POST", "/guilds/*/channels").into_iter().find(|c| c.body["type"] == 0).expect("chat made");
    let _ = chat;
    assert_eq!(env.rest.calls_to("DELETE", "/channels/*").len(), 1, "the chat goes");
    assert_eq!(env.rest.calls_to("DELETE", "/guilds/*/roles/*").len(), 1, "and its role");
    let (events, _) = svc::list_open_events(&env.t, env.g).await.expect("list");
    assert!(events.is_empty());
    env.done().await;
}

#[tokio::test]
async fn a_new_title_renames_the_channels_and_a_moved_start_moves_the_voice_job() {
    let Some(env) = Env::with("930000000000000039", channel_guild("930000000000000039")).await else { return };
    let event = env.seed(|_| {}).await;
    link(&env, &event.id, Some(TEXT), None, Some(ACCESS), "SCHEDULED").await;
    let event = env.event(&event.id).await;
    let moved = chrono::DateTime::from_timestamp_millis(event.starts_at.timestamp_millis() + 86_400_000).unwrap_or_default();
    let changes = svc::EventChanges { title: Some("Film Club".into()), starts_at: Some(moved), ..Default::default() };
    svc::edit_event(&env.t, env.gid(), &event, &changes).await.expect("db").expect("open");
    let renames = env.rest.calls_to("PATCH", &format!("/channels/{TEXT}"));
    assert_eq!(renames[0].body["name"], "film-club");
    assert_eq!(renames[0].reason.as_deref(), Some("Event renamed"));
    let voice_job = env.job(&svc::voice_job_key(&event.id)).await.expect("voice job");
    assert_eq!(voice_job.run_at.timestamp_millis(), moved.timestamp_millis() - 30 * 60_000);
    // With the voice channel open, both are renamed and no voice job is needed.
    link(&env, &event.id, Some(TEXT), Some(VOICE), Some(ACCESS), "SCHEDULED").await;
    env.t.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(svc::voice_job_key(&event.id)), ..Default::default() }).await;
    let current = env.event(&event.id).await;
    let later = chrono::DateTime::from_timestamp_millis(moved.timestamp_millis() + 3_600_000).unwrap_or_default();
    svc::edit_event(&env.t, env.gid(), &current, &svc::EventChanges { title: Some("Film Night".into()), starts_at: Some(later), ..Default::default() })
        .await
        .expect("db")
        .expect("open");
    assert_eq!(env.rest.calls_to("PATCH", &format!("/channels/{VOICE}"))[0].body["name"], "Film Night");
    assert!(env.job(&svc::voice_job_key(&event.id)).await.is_none());
    env.done().await;
}

#[tokio::test]
async fn the_host_keeps_access_and_a_second_going_repairs_a_missing_role() {
    let Some(env) = Env::with("930000000000000040", channel_guild("930000000000000040")).await else { return };
    let (event, _) = env.create(|d| d.role_id = Some(R1.into())).await;
    link(&env, &event.id, Some(TEXT), None, Some(ACCESS), "SCHEDULED").await;
    // The host answers Can't go: they keep the chat.
    route(&env, button(&env, &format!("event:{}:declined", event.id), HOST, &[ACCESS])).await;
    assert!(env.rest.calls_to("DELETE", &format!("/guilds/{}/members/{HOST}/roles/{ACCESS}", env.g)).is_empty());
    // A member marked Going without the role (an earlier click failed to give it) clicks Going again.
    going(&env.t, env.g, &event.id, U2, RsvpStatus::Going).await;
    route(&env, button(&env, &format!("event:{}:going", event.id), U2, &[])).await;
    assert_eq!(env.t.responses.calls()[1].content().map(|c| c.starts_with("You are already marked as **Going**")), Some(true));
    assert_eq!(env.rest.calls_to("PUT", &format!("/guilds/{}/members/{U2}/roles/{R1}", env.g)).len(), 1, "repaired");
    assert_eq!(env.rest.calls_to("PUT", &format!("/guilds/{}/members/{U2}/roles/{ACCESS}", env.g)).len(), 1);
    env.done().await;
}

#[tokio::test]
async fn event_create_uses_the_announcement_channel_the_saved_zone_and_the_host_cap() {
    use bot_core::command::option_type as ot;
    const TZU: &str = "600000000000000051";
    let g = "930000000000000041";
    // The member cannot write in the channel the command ran in; the configured announcement channel is fine.
    let guild = fake_guild(g, bot_perms()).channel("810000000000000020", 0, "announcements", &[(g, false, P::empty(), P::SEND_MESSAGES), (BOT_ROLE, false, P::SEND_MESSAGES, P::empty())]).member(TZU, &[]);
    let Some(env) = Env::with(g, guild).await else { return };
    let create = |user: &str, channel: &str| {
        FakeInteraction::slash("event")
            .guild(g)
            .user(user)
            .channel(channel)
            .subcommand("create")
            .option("title", ot::STRING, json!("Board games"))
            .option("start", ot::STRING, json!("tomorrow 18:30"))
            .build()
    };
    route(&env, create(TZU, LOCKED)).await;
    assert_eq!(env.t.responses.calls()[1].embed_description(), Some("You cannot send messages in <#810000000000000002>."), "below staff, here is checked");
    env.t.settings().update_settings(g, SettingsPatch::new().set("eventChannelId", "810000000000000020")).await.expect("settings");
    feat_translation::user_timezone_service::set_user_timezone(&env.t, TZU, "America/Chicago").await.expect("zone");
    route(&env, create(TZU, LOCKED)).await;
    let desc = env.t.responses.calls()[1].embed_description().unwrap_or_default().to_string();
    assert!(desc.contains("is posted in <#810000000000000020>"), "{desc}");
    assert!(desc.contains("The start was read in America/Chicago (UTC"), "{desc}");
    assert!(desc.contains("your saved timezone (`/timezone set`)."), "{desc}");
    // Below staff: at most 3 open events per host.
    for _ in 0..2 {
        route(&env, create(TZU, C1)).await;
    }
    route(&env, create(TZU, C1)).await;
    assert_eq!(
        env.t.responses.calls()[1].embed_description(),
        Some("You already host 3 open events. Cancel one with `/event cancel` or wait until one ends.")
    );
    // Staff may cancel another host's event.
    let (events, _) = svc::list_open_events(&env.t, g).await.expect("list");
    let cancel = FakeInteraction::slash("event").guild(g).user(MODR).roles(&[MOD_ROLE]).subcommand("cancel").option("event", ot::STRING, json!(events[0].id)).build();
    route(&env, cancel).await;
    assert_eq!(env.t.responses.calls()[1].embed_title(), Some("✅  Event Cancelled"));
    let _ = feat_translation::user_timezone_service::clear_user_timezone(&env.t, TZU).await;
    env.done().await;
}
