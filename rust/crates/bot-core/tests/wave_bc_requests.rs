//! The core additions requested by the later waves (`rust/docs/wave-b-core-requests.txt`,
//! `rust/docs/wave-c-core-requests.txt`).

use bot_core::interaction::Responder;
use bot_core::interaction_helpers::Payload;
use bot_core::scheduler::{CancelJobs, ScheduleJob};
use bot_core::testing::{FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, message_json};
use bot_core::{App, Registry, discord};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Interaction, Permissions, RoleId};
use serenity::builder::{CreateChannel, CreateScheduledEvent, EditRole};

const G: &str = "870000000000000001";
const OWNER: &str = "870000000000000002";

fn gid() -> GuildId {
    GuildId::new(G.parse().unwrap())
}

fn guild() -> FakeGuild {
    FakeGuild::new(G, OWNER).everyone(Permissions::VIEW_CHANNEL).member(TEST_BOT_ID, &[]).channel("870000000000000030", 0, "general", &[])
}

#[tokio::test]
async fn fake_rest_answers_role_channel_and_event_writes_with_objects_serenity_reads() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild()], &rest);
    let http = t.http().unwrap();

    let role = gid().create_role(http.as_ref(), EditRole::new().name("Mods").colour(0x00ff00)).await.expect("create_role reads the echo");
    assert_eq!(role.name, "Mods");
    assert_eq!(role.colour.0, 0x00ff00);
    let edited = gid().edit_role(http.as_ref(), RoleId::new(870000000000000050), EditRole::new().name("Admins")).await.expect("edit_role");
    assert_eq!(edited.id.get(), 870000000000000050);
    assert_eq!(edited.name, "Admins");

    let ch = gid().create_channel(http.as_ref(), CreateChannel::new("tickets")).await.expect("create_channel");
    assert_eq!(ch.name, "tickets");
    assert_eq!(ch.guild_id, gid());
    let deleted = ChannelId::new(870000000000000031).delete(http.as_ref()).await.expect("delete_channel reads the echo");
    assert_eq!(deleted.id().get(), 870000000000000031);

    let start = serenity::all::Timestamp::parse("2031-01-01T00:00:00Z").unwrap();
    let ev = gid()
        .create_scheduled_event(
            http.as_ref(),
            CreateScheduledEvent::new(serenity::all::ScheduledEventType::External, "Game night", start)
                .location("Online")
                .end_time(serenity::all::Timestamp::parse("2031-01-01T02:00:00Z").unwrap()),
        )
        .await
        .expect("create_scheduled_event");
    assert_eq!(ev.name, "Game night");
    assert_eq!(ev.guild_id, gid());
}

#[tokio::test]
async fn a_reply_echo_completes_the_message_reference() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild()], &rest);
    let http = t.http().unwrap();
    let body = json!({ "content": "winner!", "message_reference": { "message_id": "870000000000000077" } });
    let m = http.send_message(ChannelId::new(870000000000000030), Vec::new(), &body).await.expect("serenity reads a reply echo");
    let r = m.message_reference.expect("the reference");
    assert_eq!(r.channel_id.get(), 870000000000000030);
    assert_eq!(r.message_id.map(|i| i.get()), Some(870000000000000077));
}

#[test]
fn message_json_is_public_and_readable() {
    let v = message_json("870000000000000090");
    let m: serenity::all::Message = serde_json::from_value(v).unwrap();
    assert_eq!(m.id.get(), 870000000000000090);
}

#[test]
fn rendered_has_an_empty_default() {
    let r = bot_core::custom_embed::Rendered::default();
    assert!(r.content.is_none() && r.embeds.is_empty() && r.link_rows.is_empty() && r.extra.is_empty());
    assert!(bot_core::custom_embed::Rendered::empty().components().is_empty());
}

#[test]
fn an_edit_can_clear_stale_attachments() {
    let p = Payload::text("hi");
    let plain = serde_json::to_value(p.to_edit()).unwrap();
    assert!(plain.get("attachments").is_none(), "{plain}");
    let cleared = serde_json::to_value(p.to_edit_clearing_attachments()).unwrap();
    assert_eq!(cleared["attachments"], json!([]));
    assert_eq!(cleared["content"], json!("hi"));
}

#[tokio::test]
async fn recorded_calls_carry_their_interaction() {
    let t = TestApp::new();
    let mut ids = Vec::new();
    for text in ["one", "two"] {
        let Interaction::Command(ci) = FakeInteraction::slash("ping").guild(G).build() else { panic!("a command") };
        ids.push(ci.id.get());
        let r = Responder::new(t.interaction_backend(), ci.id, ci.token.clone());
        r.defer_reply(false).await.unwrap();
        r.edit_reply(serenity::builder::EditInteractionResponse::new().content(text)).await.unwrap();
    }
    assert_eq!(t.responses.names_of(ids[0]), vec!["deferReply", "editReply"]);
    let second = t.responses.calls_of(ids[1]);
    assert_eq!(second.len(), 2);
    assert_eq!(second[1].content(), Some("two"));
    assert_ne!(second[0].token, t.responses.calls_of(ids[0])[0].token);
}

#[test]
fn role_options_can_get_a_default_resolved_role() {
    let plain = FakeInteraction::slash("info").guild(G).option("role", 8, json!("870000000000000060")).build();
    let Interaction::Command(ci) = plain else { panic!() };
    assert!(ci.data.resolved.roles.is_empty());
    let resolved = FakeInteraction::slash("info").guild(G).option("role", 8, json!("870000000000000060")).resolve_roles_by_default().build();
    let Interaction::Command(ci) = resolved else { panic!() };
    assert_eq!(ci.data.resolved.roles.get(&RoleId::new(870000000000000060)).map(|r| r.name.clone()), Some("role870000000000000060".into()));
}

#[tokio::test]
async fn incidents_written_back_to_the_cached_guild() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild()], &rest);
    let data: serenity::all::IncidentsData =
        serde_json::from_value(json!({ "invites_disabled_until": "2031-01-01T00:00:00+00:00", "dms_disabled_until": null })).unwrap();
    assert!(discord::cache_guild_incidents(&t.app, gid(), Some(&data)));
    let cached = t.app.discord().unwrap().cache.guild(gid()).map(|g| g.incidents_data).unwrap();
    assert_eq!(cached.and_then(|d| d.invites_disabled_until).map(|ts| ts.unix_timestamp()), Some(1_924_992_000));
    // The rest of the guild survives the re-insert.
    assert!(t.app.discord().unwrap().cache.guild(gid()).unwrap().channels.contains_key(&ChannelId::new(870000000000000030)));
    assert!(!discord::cache_guild_incidents(&t.app, GuildId::new(1), Some(&data)));
}

#[tokio::test]
async fn rest_get_json_returns_the_raw_answer() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_rest(&rest);
    rest.on("GET", "/guilds/870000000000000001", 200, json!({ "id": G, "approximate_member_count": 12 }));
    let http = t.http().unwrap();
    let v = discord::rest_get_json(&http, serenity::http::Route::Guild { guild_id: gid() }, Some(vec![("with_counts", "true".into())])).await.unwrap();
    assert_eq!(v["approximate_member_count"], json!(12));
    let call = &rest.calls_to("GET", "/guilds/*")[0];
    assert_eq!(call.query, vec![("with_counts".to_string(), "true".to_string())]);
}

#[tokio::test]
async fn try_cancel_jobs_reports_what_cancel_jobs_hides() {
    let t = TestApp::new();
    let now = bot_core::clock::Clock::now_ms(&t.clock);
    let at = chrono::DateTime::from_timestamp_millis(now + 60_000).unwrap();
    t.app.scheduler().schedule_job(ScheduleJob::new(G, bot_config::JobType::CaptchaTimeout, at).dedupe_key("captcha:x")).await.unwrap();
    let n = t.app.scheduler().try_cancel_jobs(CancelJobs { guild_id: Some(G.into()), job_type: Some("captcha.timeout".into()), ..Default::default() }).await;
    assert_eq!(n.unwrap(), 1);
    assert_eq!(t.app.scheduler().try_cancel_jobs(CancelJobs::default()).await.unwrap(), 0);

    // A database that cannot be reached: an error, where cancel_jobs answers 0.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(300))
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .unwrap();
    let app = App::builder().postgres(pool).build();
    let filter = || CancelJobs { guild_id: Some(G.into()), ..Default::default() };
    assert!(app.scheduler().try_cancel_jobs(filter()).await.is_err());
    assert_eq!(app.scheduler().cancel_jobs(filter()).await, 0);
}

#[test]
fn registry_parts_and_command_order() {
    let mut reg = Registry::new();
    for name in ["zeta", "alpha", "mid"] {
        reg.command(bot_core::command::CommandDef::new("test", bot_core::command::SlashCommand::new(name, "d"), None, |_| async { Ok(()) }));
    }
    reg.sort_commands_by(|a, b| a.name().cmp(b.name()));
    let names: Vec<String> = reg.commands().iter().map(|c| c.name().to_string()).collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    let (commands, plugins) = reg.into_parts();
    assert_eq!(commands.len(), 3);
    assert!(plugins.is_empty());
}

#[test]
fn deploy_config_defaults_the_hash_file_to_the_root() {
    let root = std::path::Path::new("some-root");
    let cfg = bot_core::command::DeployConfig::from_env_in(root);
    match std::env::var("DEPLOY_HASH_FILE").ok().filter(|v| !v.is_empty()) {
        Some(v) => assert_eq!(cfg.hash_file, std::path::PathBuf::from(v)),
        None => assert_eq!(cfg.hash_file, root.join(".deploy-hash")),
    }
}

#[test]
fn the_redis_backend_exposes_is_ready() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let backend = bot_core::cache::RedisBackend::connect(bot_core::cache::RedisOptions::new(Some("redis://127.0.0.1:1"))).unwrap();
        assert!(!backend.is_ready());
        backend.disconnect();
    });
}

#[tokio::test]
async fn capture_global_follows_spawned_tasks() {
    let (_, lines) = bot_util::logger::capture_global(async {
        tokio::spawn(async { bot_util::log_warn!("from a spawned task") }).await.unwrap();
    })
    .await;
    assert!(lines.iter().any(|(_, l)| l.contains("from a spawned task")), "{lines:?}");
    let (_, plain) = bot_util::logger::capture(async {
        tokio::spawn(async { bot_util::log_info!("not captured by capture()") }).await.unwrap();
    })
    .await;
    assert!(plain.is_empty());
}

#[allow(dead_code)]
fn unused(_: Value) {}
