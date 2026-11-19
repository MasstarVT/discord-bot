//! Database tests (TEST_DATABASE_URL, TEST_REDIS_URL): the hub CRUD and every RAW_SQL statement, the whole create /
//! leave / keep-alive / claim / ban / transfer lifecycle against the fake REST and an offline serenity cache, the
//! cleanup job, reconciliation, channelDelete, the commands through the router, the voice-roles commands, and the
//! cross-language checks in both directions (`rust/fixtures/gen/feat-tempvoice-js.mjs`).

mod common;

use std::sync::Arc;

use bot_core::prelude::{ChannelDelete, VoiceChange, VoiceStateUpdate};
use bot_core::router::route_interaction;
use bot_core::scheduler::{Job, JobCtx, JobOutcome};
use bot_core::testing::{DbTestApp, FakeInteraction, database_url, run_node};
use common::*;
use feat_tempvoice::temp_voice_service::{self as tv, ActionSource, ActionValue, RoleListOutcome, VoiceActionReq};
use feat_tempvoice::voice_role_service as vr;
use serde_json::{Value, json};
use serenity::all::Permissions;

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_tempvoice::register(&mut reg);
    reg
}

/// Ids of one test: guild, channels, users and roles, unique per test number.
struct Ids {
    n: u32,
}

impl Ids {
    fn g(&self) -> String {
        format!("95502100000000{:04}", self.n)
    }
    fn ch(&self, k: u32) -> String {
        format!("96{:04}0000000000{:02}", self.n, k)
    }
    fn u(&self, k: u32) -> String {
        format!("84{:04}0000000000{:02}", self.n, k)
    }
    fn r(&self, k: u32) -> String {
        format!("87{:04}0000000000{:02}", self.n, k)
    }
    fn cat(&self) -> String {
        self.ch(1)
    }
    fn hub(&self) -> String {
        self.ch(2)
    }
    fn owner(&self) -> String {
        self.u(99)
    }
}

/// The standard guild: a category with the hub voice channel, a loose voice channel and a text channel; members
/// u1..u3, a moderator u4 (Ban Members), a bot member u9; the guild owner u99.
fn base_guild(ids: &Ids, bot_perms: Permissions) -> TestGuild {
    TestGuild::new(&ids.g(), &ids.owner(), bot_perms)
        .role(&ids.r(1), 5, Permissions::BAN_MEMBERS)
        .role(&ids.r(2), 3, Permissions::empty())
        .role(&ids.r(3), 2, Permissions::empty())
        .member(&ids.u(1), &[])
        .member(&ids.u(2), &[])
        .member(&ids.u(3), &[])
        .member(&ids.u(4), &[&ids.r(1)])
        .bot_member(&ids.u(9))
        .channel(&ids.cat(), 4, "Voice", None, &[])
        .channel(&ids.hub(), 2, "➕ Join to Create", Some(&ids.cat()), &[])
        .channel(&ids.ch(3), 2, "loose", None, &[])
        .channel(&ids.ch(4), 0, "general", None, &[])
}

struct W {
    t: DbTestApp,
    rest: Arc<FakeRest>,
    ids: Ids,
}

impl W {
    fn g(&self) -> String {
        self.ids.g()
    }

    /// Moves `user` (to `None`: disconnects) in the cache and dispatches voiceStateUpdate, then waits for the queued
    /// temp-voice work.
    async fn mv(&self, user: &str, to: Option<&str>) {
        let g = gid(&self.g());
        let cache = self.t.discord().unwrap().cache.clone();
        let old = cache.guild(g).and_then(|gd| gd.voice_states.get(&uid(user)).cloned());
        set_voice(&cache, g, uid(user), to.map(cid));
        let new = cache.guild(g).and_then(|gd| gd.voice_states.get(&uid(user)).cloned()).unwrap_or_else(|| voice_state(g, uid(user), None));
        let change = VoiceChange::classify(old.as_ref().and_then(|s| s.channel_id), new.channel_id);
        let m = member(&self.t, &self.g(), user);
        self.t.run_hooks(VoiceStateUpdate { guild_id: g, old_state: old, new_state: new, member: m, change }).await;
        tv::idle(&self.t).await;
    }

    async fn create_hub(&self, data: tv::HubData) -> bot_db::TempVoiceHub {
        let mut all: tv::HubData = vec![("channelId", json!(self.ids.hub()))];
        all.extend(data);
        tv::create_hub(&self.t, &self.g(), all).await.unwrap()
    }

    async fn row_of_owner(&self, owner: &str) -> Option<bot_db::TempVoiceChannel> {
        let rows: Vec<bot_db::TempVoiceChannel> =
            sqlx::query_as(r#"SELECT * FROM "TempVoiceChannel" WHERE "guildId" = $1 AND "ownerId" = $2"#).bind(self.g()).bind(owner).fetch_all(&self.t.pool).await.unwrap();
        rows.into_iter().next()
    }

    async fn row(&self, channel: &str) -> Option<bot_db::TempVoiceChannel> {
        sqlx::query_as(r#"SELECT * FROM "TempVoiceChannel" WHERE "channelId" = $1"#).bind(channel).fetch_optional(&self.t.pool).await.unwrap()
    }

    async fn hub_row(&self) -> Option<bot_db::TempVoiceHub> {
        sqlx::query_as(r#"SELECT * FROM "TempVoiceHub" WHERE "guildId" = $1"#).bind(self.g()).fetch_optional(&self.t.pool).await.unwrap()
    }

    async fn action(&self, actor: &str, channel: &str, action: &str, target: Option<&str>, value: ActionValue) -> (bool, String, String) {
        let settings = self.t.settings().get_settings_safe(&self.g()).await;
        let r = tv::run_voice_action(
            &self.t,
            VoiceActionReq {
                guild_id: gid(&self.g()),
                actor: member(&self.t, &self.g(), actor),
                channel_id: channel.to_string(),
                action: action.to_string(),
                target_id: target.map(str::to_string),
                value,
                settings,
                source: ActionSource::Command,
            },
        )
        .await;
        let e = embed_json(&r.embed);
        (r.ok, e["title"].as_str().unwrap_or_default().to_string(), e["description"].as_str().unwrap_or_default().to_string())
    }
}

async fn world(n: u32, build: impl FnOnce(TestGuild, &Ids) -> TestGuild) -> Option<W> {
    let url = database_url()?;
    let ids = Ids { n };
    let t = match bot_core::testing::redis_url() {
        Some(r) => DbTestApp::with_redis(&url, &r).await,
        None => DbTestApp::new(&url).await,
    };
    t.reset_guilds(&[&ids.g()]).await;
    let guild = build(base_guild(&ids, admin()), &ids);
    install_guilds(&t, &[guild]);
    let rest = FakeRest::install(&t);
    t.install(registry());
    t.settings().ensure_guild_settings(&ids.g()).await.unwrap();
    Some(W { t, rest, ids })
}

/// The voice channel the hub created for `owner` (from the row).
async fn temp_of(w: &W, owner: &str) -> String {
    w.row_of_owner(owner).await.expect("a temp channel row").channel_id
}

// ── CRUD and RAW_SQL ──────────────────────────────────────────────────────────

#[tokio::test]
async fn raw_sql_executes_against_the_migrated_schema() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    for (name, sql, binds) in tv::RAW_SQL {
        let mut tx = pool.begin().await.unwrap();
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn hub_crud_cap_duplicates_and_role_lists() {
    let Some(w) = world(1, |g, _| g).await else { return };
    let g = w.g();
    let hub = w.create_hub(vec![("keepAliveMinutes", json!(5)), ("bitrate", json!(64000))]).await;
    assert_eq!(hub.keep_alive_minutes, 5);
    assert_eq!(hub.name_format, tv::DEFAULT_NAME_FORMAT, "schema defaults apply to what was not given");
    assert!(hub.sync_permissions && hub.text_channel_members_only && !hub.create_text_channel);
    let dup = tv::create_hub(&w.t, &g, vec![("channelId", json!(w.ids.hub()))]).await.unwrap_err();
    assert!(tv::is_duplicate_hub(&dup), "{dup}");
    assert_eq!(tv::find_hub_by_channel(&w.t.pool, &g, &w.ids.hub()).await.unwrap().map(|h| h.id), Some(hub.id.clone()));
    assert!(tv::find_hub_by_channel(&w.t.pool, "955021000000009999", &w.ids.hub()).await.unwrap().is_none(), "another guild's hub");
    let updated = tv::update_hub(&w.t, &g, &hub.id, vec![("userLimit", json!(4))]).await.unwrap().unwrap();
    assert_eq!(updated.user_limit, 4);
    assert!(tv::update_hub(&w.t, "955021000000009999", &hub.id, vec![("userLimit", json!(4))]).await.unwrap().is_none());

    for i in 10..19 {
        tv::create_hub(&w.t, &g, vec![("channelId", json!(w.ids.ch(i)))]).await.unwrap();
    }
    assert_eq!(tv::count_hubs(&w.t.pool, &g).await.unwrap(), 10);
    let over = tv::create_hub(&w.t, &g, vec![("channelId", json!(w.ids.ch(30)))]).await.unwrap_err();
    assert!(over.downcast_ref::<tv::HubLimitError>().is_some());

    let r = w.ids.r(2);
    let upd = |field: &'static str, op: &'static str, role: String| {
        let (t, g, h) = (w.t.app.clone(), g.clone(), hub.id.clone());
        async move { tv::update_hub_role_list(&t, &g, &h, field, op, &role).await.unwrap() }
    };
    assert_eq!(upd("moderatorRoleIds", "add", r.clone()).await, RoleListOutcome::Ok);
    assert_eq!(upd("moderatorRoleIds", "add", r.clone()).await, RoleListOutcome::Exists);
    assert_eq!(upd("moderatorRoleIds", "remove", r.clone()).await, RoleListOutcome::Ok);
    assert_eq!(upd("moderatorRoleIds", "remove", r.clone()).await, RoleListOutcome::Missing);
    for i in 0..10 {
        assert_eq!(upd("ignoredRoleIds", "add", w.ids.r(40 + i)).await, RoleListOutcome::Ok);
    }
    assert_eq!(upd("ignoredRoleIds", "add", w.ids.r(60)).await, RoleListOutcome::Limit);
    assert!(tv::update_hub_role_list(&w.t, &g, &hub.id, "nope", "add", "1").await.is_err());
    assert_eq!(tv::update_hub_role_list(&w.t, &g, "missing-hub", "ignoredRoleIds", "add", "1").await.unwrap(), RoleListOutcome::Gone);

    let hubs = tv::list_hubs(&w.t.pool, &g).await.unwrap();
    assert_eq!(hubs.len(), 10);
    assert_eq!(hubs[0].hub.id, hub.id, "oldest first");
    assert!(hubs.iter().all(|h| h.channels == 0));
    assert!(!tv::is_temp_channel(&w.t.pool, &w.ids.hub()).await.unwrap());
    w.t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn the_hub_cap_holds_under_concurrent_creation() {
    let Some(w) = world(2, |g, _| g).await else { return };
    let mut futs = Vec::new();
    for i in 0..14 {
        let (t, g, ch) = (w.t.app.clone(), w.g(), w.ids.ch(10 + i));
        futs.push(tokio::spawn(async move { tv::create_hub(&t, &g, vec![("channelId", json!(ch))]).await.is_ok() }));
    }
    let mut ok = 0;
    for f in futs {
        if f.await.unwrap() {
            ok += 1;
        }
    }
    assert_eq!(ok, 10);
    assert_eq!(tv::count_hubs(&w.t.pool, &w.g()).await.unwrap(), 10);
    w.t.reset_guilds(&[&w.g()]).await;
}

// ── The lifecycle ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_hub_join_creates_the_channel_the_text_channel_and_the_panel_and_an_empty_channel_is_deleted() {
    let Some(w) = world(3, |g, _| g).await else { return };
    let u1 = w.ids.u(1);
    w.create_hub(vec![("createTextChannel", json!(true)), ("nameFormat", json!("{username} room"))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;

    let creates = w.rest.calls_of("create_channel");
    assert_eq!(creates.len(), 2, "{:?}", w.rest.ops());
    assert_eq!(creates[0]["name"], format!("user{u1} room"));
    assert_eq!(creates[0]["type"], 2);
    assert_eq!(creates[0]["parent"], json!(w.ids.cat()));
    assert_eq!(creates[0]["reason"], format!("Temporary channel for user{u1}"));
    let owner_ow = creates[0]["overwrites"].as_array().unwrap().iter().find(|o| o["id"] == json!(u1)).unwrap().clone();
    assert_eq!(owner_ow["allow"], tv::OWNER_PERMS.to_string());
    assert_eq!(creates[1]["type"], 0);
    assert_eq!(creates[1]["name"], format!("user{u1}-room"));
    assert_eq!(creates[1]["topic"], format!("Text chat for 🔊 user{u1} room"));
    let row = w.row_of_owner(&u1).await.expect("row");
    assert_eq!(row.index, 1);
    let text = row.text_channel_id.clone().expect("text channel");
    assert_eq!(voice_of(&w.t, &w.g(), &u1), Some(row.channel_id.clone()), "moved into the new channel");
    let sends = w.rest.calls_of("send_message");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["channel"], json!(text), "the panel goes into the text channel");
    assert_eq!(sends[0]["message"]["components"].as_array().unwrap().len(), 2);
    assert_eq!(sends[0]["message"]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(w.rest.calls_of("pin_message").len(), 1);
    assert!(w.row(&row.channel_id).await.unwrap().control_message_id.is_some());
    assert!(tv::is_temp_channel(&w.t.pool, &row.channel_id).await.unwrap());

    // A second join right away: back into the own channel instead of a new one.
    w.mv(&u1, Some(&w.ids.ch(3))).await; // the channel stays (not empty? it is empty now: keep alive 0 deletes it)
    assert!(w.row(&row.channel_id).await.is_none(), "keep alive 0: deleted at once when emptied");
    let deletes: Vec<String> = w.rest.calls_of("delete_channel").iter().map(|d| d["channel"].as_str().unwrap().to_string()).collect();
    assert!(deletes.contains(&row.channel_id) && deletes.contains(&text), "{deletes:?}");
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&w.t, &row.channel_id).as_deref(), Some("tempvoice"));

    // The creation cooldown: a new channel within 5 s is refused (one retry is queued).
    w.rest.clear();
    w.mv(&u1, Some(&w.ids.hub())).await;
    assert!(w.rest.calls_of("create_channel").is_empty(), "cooldown");
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn keep_alive_schedules_the_cleanup_job_a_return_cancels_it_and_the_job_deletes() {
    let Some(w) = world(4, |g, _| g).await else { return };
    let u1 = w.ids.u(1);
    w.create_hub(vec![("keepAliveMinutes", json!(5))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let ch = temp_of(&w, &u1).await;
    w.mv(&u1, None).await;
    let row = w.row(&ch).await.unwrap();
    assert!(row.empty_since.is_some() && row.owner_left_at.is_some());
    let job = w.t.scheduler().get_job(&tv::cleanup_key(&ch)).await.expect("keep-alive job");
    assert_eq!(job.r#type, "tempvoice.cleanup");
    assert_eq!(job.payload, json!({ "channelId": ch }));
    assert_eq!(job.max_attempts, 5);
    let due = job.run_at.timestamp_millis() - row.empty_since.unwrap().timestamp_millis();
    assert!((299_000..=301_000).contains(&due), "{due}");

    w.mv(&u1, Some(&ch)).await;
    let row = w.row(&ch).await.unwrap();
    assert!(row.empty_since.is_none() && row.owner_left_at.is_none());
    assert!(w.t.scheduler().get_job(&tv::cleanup_key(&ch)).await.is_none(), "the return cancels the job");

    w.mv(&u1, None).await;
    let job = w.t.scheduler().get_job(&tv::cleanup_key(&ch)).await.unwrap();
    let ctx = JobCtx { app: w.t.app.clone(), guild_id: w.g(), late_by_ms: 0, attempt: 1 };
    // Someone is back: the job only clears emptySince.
    set_voice(&w.t.discord().unwrap().cache, gid(&w.g()), uid(&w.ids.u(2)), Some(cid(&ch)));
    assert_eq!(tv::handle_cleanup_job(Job::from_row(&job), ctx.clone()).await.unwrap(), JobOutcome::Done);
    assert!(w.row(&ch).await.unwrap().empty_since.is_none());
    set_voice(&w.t.discord().unwrap().cache, gid(&w.g()), uid(&w.ids.u(2)), None);
    // A failed delete keeps the row and fails the run (the scheduler retries).
    w.rest.fail_op("delete_channel", 50013);
    assert!(tv::handle_cleanup_job(Job::from_row(&job), ctx.clone()).await.is_err());
    assert!(w.row(&ch).await.is_some());
    w.rest.clear_fail("delete_channel");
    assert_eq!(tv::handle_cleanup_job(Job::from_row(&job), ctx.clone()).await.unwrap(), JobOutcome::Done);
    assert!(w.row(&ch).await.is_none());
    // A payload without a snowflake is ignored.
    let mut bad = Job::from_row(&job);
    bad.payload = json!({ "channelId": "nope" });
    assert_eq!(tv::handle_cleanup_job(bad, ctx).await.unwrap(), JobOutcome::Done);
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn rule_20_a_rest_10003_purges_any_other_answer_keeps_the_row() {
    let Some(w) = world(5, |g, _| g).await else { return };
    let u1 = w.ids.u(1);
    w.create_hub(vec![("keepAliveMinutes", json!(5))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let ch = temp_of(&w, &u1).await;
    let g = gid(&w.g());
    // The channel vanishes from the cache without a delete event.
    let cached = channel(&w.t, &w.g(), &ch).unwrap();
    let mut ev: serenity::all::ChannelDeleteEvent = serde_json::from_value(serde_json::to_value(&cached).unwrap()).unwrap();
    w.t.discord().unwrap().cache.update(&mut ev);
    let m = member(&w.t, &w.g(), &u1);
    w.rest.fail_op("fetch_channel", 500);
    tv::on_leave_temp(&w.t, g, &ch, &m).await.unwrap();
    assert!(w.row(&ch).await.is_some(), "a 500 keeps the row");
    let job = w.t.scheduler().get_job(&tv::cleanup_key(&ch)).await;
    assert!(job.is_none(), "nothing was decided");
    // The cleanup job looks again later.
    let fake_job = Job { id: "x".into(), guild_id: w.g(), job_type: "tempvoice.cleanup".into(), payload: json!({ "channelId": ch }), run_at: chrono::Utc::now(), attempts: 1, max_attempts: 5, dedupe_key: None, version: 1 };
    let ctx = JobCtx { app: w.t.app.clone(), guild_id: w.g(), late_by_ms: 0, attempt: 1 };
    assert!(matches!(tv::handle_cleanup_job(fake_job.clone(), ctx.clone()).await.unwrap(), JobOutcome::Reschedule { .. }));
    w.rest.clear_fail("fetch_channel");
    tv::on_leave_temp(&w.t, g, &ch, &m).await.unwrap();
    assert!(w.row(&ch).await.is_none(), "a REST 10003 purges the row");
    // A runVoiceAction on a channel that is not cached and gone.
    let (ok, title, _) = w.action(&u1, &ch, "lock", None, ActionValue::None).await;
    assert!(!ok);
    assert_eq!(title, "❌  Not In A Temporary Channel");
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn voice_actions_lock_hide_limit_rename_kick_ban_transfer_claim() {
    let Some(w) = world(6, |g, _| g).await else { return };
    let (u1, u2, u3, moder) = (w.ids.u(1), w.ids.u(2), w.ids.u(3), w.ids.u(4));
    w.create_hub(vec![("keepAliveMinutes", json!(5)), ("ownershipLockMinutes", json!(10))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let ch = temp_of(&w, &u1).await;
    w.mv(&u2, Some(&ch)).await;
    let name = channel(&w.t, &w.g(), &ch).unwrap().name;

    let (ok, title, desc) = w.action(&u1, &ch, "lock", None, ActionValue::None).await;
    assert!(ok, "{title} {desc}");
    assert_eq!(title, "✅  Channel Locked");
    assert_eq!(desc, format!("🔒 New members can no longer join **{}**.", bot_core::markdown::escape_markdown(&name)));
    assert!(w.row(&ch).await.unwrap().locked);
    let ow = w.rest.calls_of("set_overwrites").last().unwrap().clone();
    let everyone = ow["overwrites"].as_array().unwrap().iter().find(|o| o["id"] == json!(w.g())).unwrap().clone();
    assert_eq!(everyone["deny"], (1u64 << 20).to_string());
    assert_eq!(w.action(&u1, &ch, "lock", None, ActionValue::None).await.1, "⚠️  No Change");
    assert!(w.action(&u1, &ch, "unlock", None, ActionValue::None).await.0);
    assert!(w.action(&u1, &ch, "hide", None, ActionValue::None).await.0);
    assert!(w.row(&ch).await.unwrap().hidden);
    assert!(w.action(&u1, &ch, "reveal", None, ActionValue::None).await.0);
    assert_eq!(w.action(&u1, &ch, "reveal", None, ActionValue::None).await.2, "This channel is already visible.");

    // Not the owner, not a moderator.
    assert_eq!(w.action(&u3, &ch, "lock", None, ActionValue::None).await.1, "❌  Not Your Channel");
    // A moderator may control any channel.
    assert!(w.action(&moder, &ch, "lock", None, ActionValue::None).await.0);
    assert!(w.action(&moder, &ch, "unlock", None, ActionValue::None).await.0);

    // limit
    let (ok, _, desc) = w.action(&u1, &ch, "limit", None, ActionValue::Int(4)).await;
    assert!(ok);
    assert!(desc.contains("now allows **4** members"), "{desc}");
    assert_eq!(w.action(&u1, &ch, "limit", None, ActionValue::Int(4)).await.2, "The member limit is already 4.");
    assert_eq!(w.action(&u1, &ch, "limit", None, ActionValue::Int(100)).await.1, "❌  Invalid Limit");
    // rename, twice in 10 minutes at most
    assert_eq!(w.action(&u1, &ch, "rename", None, ActionValue::Text("  ".into())).await.1, "❌  Invalid Name");
    assert!(w.action(&u1, &ch, "rename", None, ActionValue::Text("First".into())).await.0);
    assert_eq!(w.action(&u1, &ch, "rename", None, ActionValue::Text("First".into())).await.1, "⚠️  No Change");
    let (ok, _, desc) = w.action(&u1, &ch, "rename", None, ActionValue::Text("Second  name".into())).await;
    assert!(ok);
    assert_eq!(desc, "✏️ The channel is now called **Second name**.");
    let (ok, title, desc) = w.action(&u1, &ch, "rename", None, ActionValue::Text("Third".into())).await;
    assert!(!ok);
    assert_eq!(title, "❌  Slow Down");
    assert!(desc.starts_with("Discord allows only 2 renames per channel every 10 minutes. Try again <t:"), "{desc}");
    let stamps = tv::rename_stamps(&w.t, &ch).await;
    assert_eq!(stamps.len(), 2);

    // kick
    assert_eq!(w.action(&u1, &ch, "kick", Some(&u1), ActionValue::None).await.1, "❌  Invalid Member");
    assert_eq!(w.action(&u1, &ch, "kick", Some(&u3), ActionValue::None).await.2, format!("<@{u3}> is not in this voice channel."));
    assert_eq!(w.action(&u1, &ch, "kick", Some(&moder), ActionValue::None).await.1, "❌  Protected Member");
    assert_eq!(w.action(&u1, &ch, "kick", Some("849999999999999999"), ActionValue::None).await.2, "That user is not in this server.");
    let (ok, _, desc) = w.action(&u1, &ch, "kick", Some(&u2), ActionValue::None).await;
    assert!(ok, "{desc}");
    assert_eq!(voice_of(&w.t, &w.g(), &u2), None);

    // ban: a connected member is disconnected; unban
    set_voice(&w.t.discord().unwrap().cache, gid(&w.g()), uid(&u2), Some(cid(&ch)));
    let (ok, _, desc) = w.action(&u1, &ch, "ban", Some(&u2), ActionValue::None).await;
    assert!(ok, "{desc}");
    assert_eq!(w.row(&ch).await.unwrap().banned_user_ids, vec![u2.clone()]);
    assert_eq!(voice_of(&w.t, &w.g(), &u2), None);
    let ow = w.rest.calls_of("set_overwrites").last().unwrap().clone();
    let banned = ow["overwrites"].as_array().unwrap().iter().find(|o| o["id"] == json!(u2)).unwrap().clone();
    assert_eq!(banned["deny"], (1u64 << 20).to_string());
    assert_eq!(w.action(&u1, &ch, "ban", Some(&u2), ActionValue::None).await.2, format!("<@{u2}> is already banned from this channel."));
    // A banned member moved in by someone with Move Members is disconnected again.
    w.rest.clear();
    w.mv(&u2, Some(&ch)).await;
    assert_eq!(w.rest.calls_of("move_member").len(), 1);
    assert_eq!(voice_of(&w.t, &w.g(), &u2), None);
    assert!(w.action(&u1, &ch, "unban", Some(&u2), ActionValue::None).await.0);
    assert_eq!(w.action(&u1, &ch, "unban", Some(&u2), ActionValue::None).await.2, format!("<@{u2}> is not banned from this channel."));

    // transfer
    assert_eq!(w.action(&u1, &ch, "transfer", Some(&u3), ActionValue::None).await.1, "❌  Not In Channel");
    assert_eq!(w.action(&u1, &ch, "transfer", Some(&u1), ActionValue::None).await.1, "⚠️  No Change");
    w.mv(&u2, Some(&ch)).await;
    w.rest.clear();
    let (ok, title, _) = w.action(&u1, &ch, "transfer", Some(&u2), ActionValue::None).await;
    assert!(ok, "{title}");
    assert_eq!(w.row(&ch).await.unwrap().owner_id, u2);
    let notices = w.rest.calls_of("send_message");
    assert_eq!(notices.last().unwrap()["message"]["content"], format!("👑 <@{u2}> is now the owner of this channel."));
    assert_eq!(w.rest.calls_of("edit_message").len(), 1, "the panel is refreshed");

    // claim: the owner (u2) leaves, u1 is still there; the 10-minute ownership lock holds for members, not for moderators.
    w.mv(&u2, None).await;
    assert!(w.row(&ch).await.unwrap().owner_left_at.is_some(), "the owner leaving a non-empty channel starts the clock");
    let (ok, title, desc) = w.action(&u1, &ch, "claim", None, ActionValue::None).await;
    assert!(!ok);
    assert_eq!(title, "❌  Not Yet");
    assert!(desc.starts_with("This channel can be claimed <t:"), "{desc}");
    assert_eq!(w.action(&u3, &ch, "claim", None, ActionValue::None).await.1, "❌  Not In Channel");
    w.mv(&moder, Some(&ch)).await;
    let (ok, title, _) = w.action(&moder, &ch, "claim", None, ActionValue::None).await;
    assert!(ok, "{title}");
    assert_eq!(w.row(&ch).await.unwrap().owner_id, moder);
    assert_eq!(w.action(&moder, &ch, "claim", None, ActionValue::None).await.2, "You already own this channel.");
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn a_missing_bot_permission_is_reported() {
    let Some(url) = database_url() else { return };
    let ids = Ids { n: 7 };
    let t = DbTestApp::new(&url).await;
    t.reset_guilds(&[&ids.g()]).await;
    install_guilds(&t, &[base_guild(&ids, voice_perms() - Permissions::MANAGE_ROLES)]);
    let rest = FakeRest::install(&t);
    t.install(registry());
    let w = W { t, rest, ids };
    w.create_hub(vec![]).await;
    // Without Manage Roles no channel can be created.
    w.mv(&w.ids.u(1), Some(&w.ids.hub())).await;
    assert!(w.rest.calls_of("create_channel").is_empty());
    let hub = w.hub_row().await.unwrap();
    let problems = tv::hub_problems(&w.t, gid(&w.g()), &hub);
    assert_eq!(problems, vec!["I'm missing Manage Roles in **Voice**".to_string()]);
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn panel_buttons_selects_and_modals_drive_the_same_actions() {
    let Some(w) = world(8, |g, _| g).await else { return };
    let (u1, u2) = (w.ids.u(1), w.ids.u(2));
    w.create_hub(vec![("keepAliveMinutes", json!(5))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let ch = temp_of(&w, &u1).await;
    w.mv(&u2, Some(&ch)).await;
    let route = |i: FakeInteraction| route_interaction(&w.t, i.guild(&w.g()).user(&u1).build());

    route(FakeInteraction::button(&format!("voice:lock:{ch}"))).await;
    let calls = w.t.responses.calls();
    assert_eq!(w.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Channel Locked"));
    w.t.responses.clear();

    route(FakeInteraction::button(&format!("voice:limit:{ch}"))).await;
    let c = w.t.responses.calls();
    assert_eq!(c[0].method, "showModal");
    assert_eq!(c[0].body["custom_id"], format!("voice-modal:limit:{ch}"));
    w.t.responses.clear();

    route(FakeInteraction::button(&format!("voice:kick:{ch}"))).await;
    let c = w.t.responses.calls();
    assert_eq!(c[0].method, "reply");
    assert_eq!(c[0].content(), Some("Choose a member to disconnect."));
    assert!(c[0].ephemeral());
    w.t.responses.clear();

    route(FakeInteraction::user_select(&format!("voice-select:kick:{ch}"), &[&u2])).await;
    let c = w.t.responses.calls();
    assert_eq!(w.t.responses.names(), vec!["deferUpdate", "editReply"]);
    assert_eq!(c[1].embed_title(), Some("✅  Member Kicked"));
    assert_eq!(c[1].body["components"], json!([]));
    w.t.responses.clear();

    route(FakeInteraction::modal(&format!("voice-modal:rename:{ch}"), &[("voice-value", "Panel name")])).await;
    let c = w.t.responses.calls();
    assert_eq!(c[1].embed_title(), Some("✅  Channel Renamed"));
    w.t.responses.clear();

    // Not the owner: refused before the modal.
    route_interaction(&w.t, FakeInteraction::button(&format!("voice:rename:{ch}")).guild(&w.g()).user(&w.ids.u(3)).build()).await;
    let c = w.t.responses.calls();
    assert_eq!(c[0].embed_title(), Some("❌  Not Your Channel"));
    assert!(c[0].ephemeral());
    w.t.responses.clear();

    // A stale channel id answers "Channel Gone".
    route(FakeInteraction::button("voice:lock:960000000000009999")).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("❌  Channel Gone"));
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn reconcile_repairs_rows_and_serves_members_waiting_in_a_hub() {
    let Some(w) = world(9, |g, _| g).await else { return };
    let g = gid(&w.g());
    let (u1, u2, u3) = (w.ids.u(1), w.ids.u(2), w.ids.u(3));
    w.create_hub(vec![("keepAliveMinutes", json!(0))]).await;
    for u in [&u1, &u2, &u3] {
        w.mv(u, Some(&w.ids.hub())).await;
        // past the creation cooldown of the next member: each member has their own
    }
    let (c1, c2, c3) = (temp_of(&w, &u1).await, temp_of(&w, &u2).await, temp_of(&w, &u3).await);
    let cache = w.t.discord().unwrap().cache.clone();
    // c1: deleted while the bot was offline; c2: emptied while offline; c3: occupied, its owner gone, u1 there instead.
    let gone = channel(&w.t, &w.g(), &c1).unwrap();
    let mut ev: serenity::all::ChannelDeleteEvent = serde_json::from_value(serde_json::to_value(&gone).unwrap()).unwrap();
    cache.update(&mut ev);
    set_voice(&cache, g, uid(&u1), Some(cid(&c3)));
    set_voice(&cache, g, uid(&u2), None);
    set_voice(&cache, g, uid(&u3), None);
    // u2 (still in the creation cooldown of its last channel) and u4 (never had one) wait in the hub.
    set_voice(&cache, g, uid(&u2), Some(cid(&w.ids.hub())));
    let u4 = w.ids.u(4);
    set_voice(&cache, g, uid(&u4), Some(cid(&w.ids.hub())));
    w.rest.clear();
    tv::reconcile_guild(&w.t, g, None).await;
    tv::idle(&w.t).await;
    assert!(w.row(&c1).await.is_none(), "a confirmed 10003 purges");
    assert!(w.row(&c2).await.is_none(), "empty with keep alive 0: deleted");
    let r3 = w.row(&c3).await.unwrap();
    assert!(r3.owner_left_at.is_some(), "the owner left while the bot was not watching");
    assert_eq!(w.rest.calls_of("create_channel").len(), 1, "{:?}", w.rest.ops());
    let r4 = w.row_of_owner(&u4).await.expect("a channel for the member waiting in the hub");
    assert_eq!(voice_of(&w.t, &w.g(), &u4), Some(r4.channel_id));
    assert!(w.row_of_owner(&u2).await.is_none(), "u2 waits out its cooldown");
    assert!(w.hub_row().await.is_some());
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn the_channel_delete_hook_cleans_rows_text_channels_hubs_and_categories() {
    let Some(w) = world(10, |g, _| g).await else { return };
    let g = gid(&w.g());
    let u1 = w.ids.u(1);
    w.create_hub(vec![("createTextChannel", json!(true)), ("keepAliveMinutes", json!(5)), ("categoryId", json!(w.ids.cat()))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let row = w.row_of_owner(&u1).await.unwrap();
    let text = row.text_channel_id.clone().unwrap();
    let cache = w.t.discord().unwrap().cache.clone();
    let delete = |id: &str| {
        let ch = channel(&w.t, &w.g(), id).unwrap();
        let mut ev: serenity::all::ChannelDeleteEvent = serde_json::from_value(serde_json::to_value(&ch).unwrap()).unwrap();
        cache.update(&mut ev);
        ch
    };
    // The text channel goes: the row forgets it.
    let t_ch = delete(&text);
    w.t.run_hooks(ChannelDelete { guild_id: g, channel: t_ch }).await;
    tv::idle(&w.t).await;
    let r = w.row(&row.channel_id).await.unwrap();
    assert!(r.text_channel_id.is_none() && r.control_message_id.is_none());
    // The hub channel goes while a channel is open: the hub is retired, not deleted.
    let hub_ch = delete(&w.ids.hub());
    w.t.run_hooks(ChannelDelete { guild_id: g, channel: hub_ch }).await;
    tv::idle(&w.t).await;
    assert!(w.hub_row().await.is_some());
    // The category goes: the hub forgets it.
    let cat = delete(&w.ids.cat());
    w.t.run_hooks(ChannelDelete { guild_id: g, channel: cat }).await;
    tv::idle(&w.t).await;
    assert!(w.hub_row().await.unwrap().category_id.is_none());
    // The temp channel goes: the row is purged and the retired hub disappears with its last channel.
    let v = delete(&row.channel_id);
    w.t.run_hooks(ChannelDelete { guild_id: g, channel: v }).await;
    tv::idle(&w.t).await;
    assert!(w.row(&row.channel_id).await.is_none());
    assert!(w.hub_row().await.is_none(), "a retired hub is deleted when its last channel closes");
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn delete_hub_removes_rows_and_channels_and_reports_failures() {
    let Some(w) = world(11, |g, _| g).await else { return };
    let g = gid(&w.g());
    let (u1, u2) = (w.ids.u(1), w.ids.u(2));
    let hub = w.create_hub(vec![("keepAliveMinutes", json!(5))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    w.mv(&u2, Some(&w.ids.hub())).await;
    let c1 = temp_of(&w, &u1).await;
    assert_eq!(tv::list_hubs(&w.t.pool, &w.g()).await.unwrap()[0].channels, 2);
    w.rest.fail_op("delete_channel", 50013);
    let r = tv::delete_hub(&w.t, g, &hub, true, Some("admin")).await.unwrap();
    assert_eq!(r, tv::DeleteHubResult { not_found: false, deleted: 0, failed: 2, hub_channel_failed: true });
    assert!(w.row(&c1).await.is_none(), "the cascade removed the rows");
    assert_eq!(w.rest.calls_of("delete_channel")[0]["reason"], "Temp voice hub deleted by admin");
    let again = tv::delete_hub(&w.t, g, &hub, false, None).await.unwrap();
    assert!(again.not_found);
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn voice_clean_deletes_only_empty_channels_and_only_for_moderators() {
    let Some(w) = world(12, |g, _| g).await else { return };
    let (u1, u2, moder) = (w.ids.u(1), w.ids.u(2), w.ids.u(4));
    w.create_hub(vec![("keepAliveMinutes", json!(5))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    w.mv(&u2, Some(&w.ids.hub())).await;
    let (c1, c2) = (temp_of(&w, &u1).await, temp_of(&w, &u2).await);
    w.mv(&u1, None).await; // c1 is empty now (kept alive)
    route_interaction(&w.t, FakeInteraction::slash("voice").guild(&w.g()).user(&u2).subcommand("clean").build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("❌  Insufficient Permissions"));
    w.t.responses.clear();
    route_interaction(&w.t, FakeInteraction::slash("voice").guild(&w.g()).user(&moder).roles(&[&w.ids.r(1)]).subcommand("clean").build()).await;
    let c = w.t.responses.calls();
    assert_eq!(c[1].embed_title(), Some("✅  Channels Cleaned"));
    assert_eq!(c[1].embed_description(), Some("🧹 Deleted **1** empty temporary channel(s)."));
    assert!(w.row(&c1).await.is_none());
    assert!(w.row(&c2).await.is_some());
    w.t.responses.clear();
    // /voice acts on the channel the member is connected to.
    route_interaction(
        &w.t,
        FakeInteraction::slash("voice").guild(&w.g()).user(&u2).subcommand("limit").option("member_limit", bot_core::command::option_type::INTEGER, json!(3)).build(),
    )
    .await;
    let c = w.t.responses.calls();
    assert!(c[0].ephemeral());
    assert_eq!(c[1].embed_title(), Some("✅  Limit Updated"));
    assert_eq!(channel(&w.t, &w.g(), &c2).unwrap().user_limit, Some(3));
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn the_companion_text_channel_follows_the_voice_members() {
    let Some(w) = world(13, |g, _| g).await else { return };
    let (u1, u2) = (w.ids.u(1), w.ids.u(2));
    w.create_hub(vec![("createTextChannel", json!(true)), ("keepAliveMinutes", json!(5))]).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let row = w.row_of_owner(&u1).await.unwrap();
    let text = row.text_channel_id.clone().unwrap();
    w.rest.clear();
    w.mv(&u2, Some(&row.channel_id)).await;
    assert!(tv::text_sync_pending(&w.t, &row.channel_id), "debounced");
    tokio::time::sleep(std::time::Duration::from_millis(tv::TEXT_SYNC_DELAY_MS + 400)).await;
    tv::idle(&w.t).await;
    let sets = w.rest.calls_of("set_overwrites");
    assert_eq!(sets.len(), 1, "{:?}", w.rest.ops());
    assert_eq!(sets[0]["channel"], json!(text));
    let list = sets[0]["overwrites"].as_array().unwrap();
    for u in [&u1, &u2] {
        let o = list.iter().find(|o| o["id"] == json!(u)).unwrap();
        assert_eq!(o["allow"], (1u64 << 10).to_string());
    }
    assert_eq!(list.iter().find(|o| o["id"] == json!(w.g())).unwrap()["deny"], (1u64 << 10).to_string());
    w.t.reset_guilds(&[&w.g()]).await;
}

// ── /voice-hub ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn voice_hub_create_judges_an_uncached_category_by_fetching_it() {
    // JS: `category` is the option's resolved channel, so the bot's permissions are judged in it even when the cache
    // lacks it. Rust fetches it (cache, else REST) after the defer.
    let Some(url) = database_url() else { return };
    let ids = Ids { n: 31 };
    let t = DbTestApp::new(&url).await;
    t.reset_guilds(&[&ids.g()]).await;
    let guild = base_guild(&ids, voice_perms());
    let late = ids.ch(7);
    let bc = bot_core::testing::FakeRest::new();
    bc.on(
        "GET",
        &format!("/channels/{late}"),
        200,
        json!({
            "id": late, "type": 4, "guild_id": ids.g(), "name": "Late", "position": 9, "nsfw": false,
            "permission_overwrites": [{ "id": "810000000000000001", "type": 0, "allow": "0", "deny": "16" }]
        }),
    );
    t.set_discord(bc.discord(&[guild.json()]));
    t.set_guild_directory(Arc::new(bot_core::discord::StaticGuilds::available([ids.g()])));
    let _rest = FakeRest::install(&t);
    t.install(registry());
    t.settings().ensure_guild_settings(&ids.g()).await.unwrap();
    use bot_core::command::option_type as ot;
    let i = FakeInteraction::slash("voice-hub")
        .guild(&ids.g())
        .user(&ids.owner())
        .permissions(Permissions::ADMINISTRATOR)
        .subcommand("create")
        .option("category", ot::CHANNEL, json!(late))
        .build();
    route_interaction(&t, i).await;
    let c = t.responses.calls();
    assert_eq!(c[1].embed_title(), Some("❌  Missing Permissions"), "{:?}", c[1].embed_description());
    assert_eq!(c[1].embed_description(), Some("I need **Manage Channels** in **Late** to create temporary channels."));
    assert_eq!(tv::count_hubs(&t.pool, &ids.g()).await.unwrap(), 0);
    t.reset_guilds(&[&ids.g()]).await;
}

fn hub_cmd(w: &W, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("voice-hub").guild(&w.g()).user(&w.ids.owner()).permissions(Permissions::ADMINISTRATOR).subcommand(sub)
}

#[tokio::test]
async fn voice_hub_create_edit_list_and_role_groups() {
    let Some(w) = world(14, |g, _| g).await else { return };
    use bot_core::command::option_type as ot;
    // create with an existing channel
    route_interaction(
        &w.t,
        hub_cmd(&w, "create").option("channel", ot::CHANNEL, json!(w.ids.hub())).option("keep_alive", ot::INTEGER, json!(5)).option("bitrate", ot::INTEGER, json!(384)).build(),
    )
    .await;
    let c = w.t.responses.calls();
    assert_eq!(w.t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(c[1].embed_title(), Some("✅  Hub Created"));
    let d = c[1].embed_description().unwrap().to_string();
    assert!(d.starts_with(&format!("Join <#{}> to get your own voice channel.", w.ids.hub())), "{d}");
    assert!(d.contains("**Preview:** #1 - "), "{d}");
    assert!(d.contains("This server currently allows up to 96 kbps"), "{d}");
    let hub = w.hub_row().await.unwrap();
    assert_eq!((hub.keep_alive_minutes, hub.bitrate), (5, Some(384_000)));
    w.t.responses.clear();
    // already a hub
    route_interaction(&w.t, hub_cmd(&w, "create").option("channel", ot::CHANNEL, json!(w.ids.hub())).build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("❌  Already A Hub"));
    w.t.responses.clear();
    // a bad name format
    route_interaction(&w.t, hub_cmd(&w, "create").option("name_format", ot::STRING, json!("{user}")).build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("❌  Invalid Name Format"));
    w.t.responses.clear();
    // no channel: the hub channel is created
    route_interaction(&w.t, hub_cmd(&w, "create").option("category", ot::CHANNEL, json!(w.ids.cat())).build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("✅  Hub Created"));
    let made = w.rest.calls_of("create_channel");
    assert_eq!(made.last().unwrap()["name"], tv::HUB_CHANNEL_NAME);
    assert_eq!(made.last().unwrap()["parent"], json!(w.ids.cat()));
    assert_eq!(tv::count_hubs(&w.t.pool, &w.g()).await.unwrap(), 2);
    w.t.responses.clear();

    // edit
    route_interaction(&w.t, hub_cmd(&w, "edit").option("hub", ot::CHANNEL, json!(w.ids.hub())).build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("❌  Nothing To Change"));
    w.t.responses.clear();
    route_interaction(
        &w.t,
        hub_cmd(&w, "edit").option("hub", ot::CHANNEL, json!(w.ids.hub())).option("limit", ot::INTEGER, json!(3)).option("text_channel", ot::BOOLEAN, json!(true)).build(),
    )
    .await;
    let c = w.t.responses.calls();
    assert_eq!(c[1].embed_title(), Some("✅  Hub Updated"));
    let d = c[1].embed_description().unwrap();
    assert!(d.starts_with("• **Limit:** none → 3\n• **Text channel:** off → private\n\nName, limit, bitrate"), "{d}");
    w.t.responses.clear();
    route_interaction(&w.t, hub_cmd(&w, "edit").option("hub", ot::CHANNEL, json!(w.ids.hub())).option("limit", ot::INTEGER, json!(3)).build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("⚠️  No Change"));
    w.t.responses.clear();
    route_interaction(&w.t, hub_cmd(&w, "edit").option("hub", ot::CHANNEL, json!(w.ids.ch(3))).option("limit", ot::INTEGER, json!(3)).build()).await;
    assert_eq!(w.t.responses.calls()[1].embed_description(), Some(format!("<#{}> is not a hub. See `/voice-hub list`.", w.ids.ch(3)).as_str()));
    w.t.responses.clear();

    // list
    route_interaction(&w.t, hub_cmd(&w, "list").build()).await;
    let c = w.t.responses.calls();
    assert_eq!(c[1].embed_title(), Some("ℹ️  Temporary Channel Hubs"));
    assert_eq!(c[1].embed_description(), Some("2/10 hubs"));
    let fields = c[1].body["embeds"][0]["fields"].as_array().unwrap().clone();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0]["name"], "🔊 #➕ Join to Create");
    assert!(fields[0]["value"].as_str().unwrap().contains("Active channels: 0"));
    w.t.responses.clear();

    // role groups
    let role_cmd = |group: &str, sub: &str, role: &str| {
        FakeInteraction::slash("voice-hub")
            .guild(&w.g())
            .user(&w.ids.owner())
            .group(group, sub)
            .option("hub", ot::CHANNEL, json!(w.ids.hub()))
            .option("role", ot::ROLE, json!(role))
            .build()
    };
    route_interaction(&w.t, role_cmd("mod-role", "add", &w.ids.r(2))).await;
    assert_eq!(w.t.responses.calls()[1].embed_title(), Some("✅  Moderator Role Added"));
    route_interaction(&w.t, role_cmd("mod-role", "add", &w.ids.r(2))).await;
    assert_eq!(w.t.responses.calls()[3].embed_description(), Some(format!("<@&{}> is already a moderator role.", w.ids.r(2)).as_str()));
    route_interaction(&w.t, role_cmd("ignore-role", "remove", &w.ids.r(3))).await;
    assert_eq!(w.t.responses.calls()[5].embed_description(), Some(format!("<@&{}> is not an ignored role.", w.ids.r(3)).as_str()));
    route_interaction(&w.t, role_cmd("ignore-role", "add", &w.g())).await;
    assert_eq!(w.t.responses.calls()[7].embed_description(), Some("@everyone cannot be used here."));
    let hub = tv::find_hub_by_channel(&w.t.pool, &w.g(), &w.ids.hub()).await.unwrap().unwrap();
    assert_eq!(hub.moderator_role_ids, vec![w.ids.r(2)]);
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn voice_hub_delete_asks_for_confirmation_first() {
    let Some(w) = world(15, |g, _| g).await else { return };
    use bot_core::command::option_type as ot;
    w.create_hub(vec![]).await;
    let t = w.t.app.clone();
    let i = hub_cmd(&w, "delete").option("hub", ot::CHANNEL, json!(w.ids.hub())).option("delete_channel", ot::BOOLEAN, json!(true)).build();
    let run = tokio::spawn(async move { route_interaction(&t, i).await });
    for _ in 0..200 {
        if !w.t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = w.t.responses.calls().into_iter().find(|c| c.body["components"].as_array().is_some_and(|a| !a.is_empty())).expect("the prompt");
    assert_eq!(prompt.embed_title(), Some("⚠️  Delete hub?"));
    assert_eq!(
        prompt.embed_description(),
        Some(format!("Delete the hub <#{}>? Its **0** temporary channel(s) will be deleted too. The hub channel itself will also be deleted.", w.ids.hub()).as_str())
    );
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap().to_string();
    route_interaction(&w.t, FakeInteraction::button(&yes).guild(&w.g()).user(&w.ids.owner()).message_id("1000000000000000100").build()).await;
    run.await.unwrap();
    let last = w.t.responses.calls().into_iter().last().unwrap();
    assert_eq!(last.method, "followUp");
    assert_eq!(last.embed_title(), Some("✅  Hub Deleted"));
    assert!(w.hub_row().await.is_none());
    assert!(w.rest.calls_of("delete_channel").iter().any(|d| d["channel"] == json!(w.ids.hub())));
    w.t.reset_guilds(&[&w.g()]).await;
}

// ── /voicerole ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn voicerole_add_remove_any_list_and_sync() {
    let Some(w) = world(16, |g, ids| g.role(&ids.r(4), 60, Permissions::empty()).role(&ids.r(5), 1, Permissions::ADMINISTRATOR)).await else { return };
    use bot_core::command::option_type as ot;
    let cmd = |sub: &str| FakeInteraction::slash("voicerole").guild(&w.g()).user(&w.ids.owner()).subcommand(sub);
    let last = || w.t.responses.calls().into_iter().last().unwrap();
    route_interaction(&w.t, cmd("add").option("channel", ot::CHANNEL, json!(w.ids.cat())).option("role", ot::ROLE, json!(w.ids.r(2))).build()).await;
    let c = last();
    assert_eq!(c.embed_title(), Some("✅  Voice Role Added"));
    let d = c.embed_description().unwrap();
    assert!(d.starts_with(&format!("Members in any voice channel in the <#{}> category get <@&{}> while they are there.", w.ids.cat(), w.ids.r(2))), "{d}");
    assert!(d.ends_with("⚠️ The **Voice Roles** module is off. Turn it on with `/module enable voiceroles`."), "{d}");
    route_interaction(&w.t, cmd("add").option("channel", ot::CHANNEL, json!(w.ids.cat())).option("role", ot::ROLE, json!(w.ids.r(2))).build()).await;
    assert_eq!(last().embed_title(), Some("ℹ️  No Change"));
    route_interaction(&w.t, cmd("add").option("channel", ot::CHANNEL, json!(w.ids.ch(4))).option("role", ot::ROLE, json!(w.ids.r(2))).build()).await;
    assert_eq!(last().embed_title(), Some("❌  Invalid Channel"));
    route_interaction(&w.t, cmd("add").option("channel", ot::CHANNEL, json!(w.ids.hub())).option("role", ot::ROLE, json!(w.ids.r(4))).build()).await;
    assert_eq!(last().embed_description(), Some("That role is above my highest role."));
    route_interaction(&w.t, cmd("add").option("channel", ot::CHANNEL, json!(w.ids.hub())).option("role", ot::ROLE, json!(w.g())).build()).await;
    assert_eq!(last().embed_description(), Some("@everyone cannot be a voice role."));
    let row = w.t.settings().get_settings(&w.g()).await.unwrap();
    assert_eq!(vr::links_of(Some(&row)).len(), 1);

    let any = |sub: &str| FakeInteraction::slash("voicerole").guild(&w.g()).user(&w.ids.owner()).group("any", sub).option("role", ot::ROLE, json!(w.ids.r(3))).build();
    route_interaction(&w.t, any("add")).await;
    assert_eq!(last().embed_title(), Some("✅  Voice Role Added"));
    route_interaction(&w.t, any("remove")).await;
    assert_eq!(last().embed_title(), Some("✅  Voice Role Removed"));
    assert!(last().embed_description().unwrap().contains("The bot no longer manages"));
    route_interaction(&w.t, any("remove")).await;
    assert_eq!(last().embed_title(), Some("ℹ️  No Change"));

    route_interaction(&w.t, cmd("list").build()).await;
    let c = last();
    assert_eq!(c.embed_title(), Some("🎙️ Voice Roles"));
    let fields = c.body["embeds"][0]["fields"].as_array().unwrap().clone();
    assert_eq!(fields[0]["value"], "❌ Off — `/module enable voiceroles`");
    assert_eq!(fields[2]["name"], "Channel roles (1/50)");
    assert_eq!(fields[2]["value"], format!("✅ 📁 <#{}> → <@&{}>", w.ids.cat(), w.ids.r(2)));
    assert_eq!(fields[3]["value"], "None — `/voicerole any add`");

    route_interaction(&w.t, cmd("sync").build()).await;
    assert_eq!(last().embed_title(), Some("⚠️  Module Is Off"));
    w.t.settings().update_settings(&w.g(), bot_core::settings::SettingsPatch::new().set("voiceRolesEnabled", true)).await.unwrap();
    // u1 is in the hub (a voice channel of the linked category).
    set_voice(&w.t.discord().unwrap().cache, gid(&w.g()), uid(&w.ids.u(1)), Some(cid(&w.ids.hub())));
    route_interaction(&w.t, cmd("sync").build()).await;
    let c = last();
    assert_eq!(c.embed_title(), Some("✅  Voice Roles Synced"));
    assert!(c.embed_description().unwrap().starts_with("Checked **1** member: gave **1** role and took **0** away."), "{:?}", c.embed_description());
    assert_eq!(w.rest.calls_of("add_role")[0]["role"], json!(w.ids.r(2)));

    route_interaction(&w.t, cmd("remove").option("channel", ot::CHANNEL, json!(w.ids.cat())).option("role", ot::ROLE, json!(w.ids.r(2))).build()).await;
    assert_eq!(last().embed_title(), Some("✅  Voice Role Removed"));
    w.t.reset_guilds(&[&w.g()]).await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_writes_rust_reads_and_runs_the_job() {
    let Some(w) = world(17, |g, _| g).await else { return };
    if bot_core::testing::redis_url().is_none() {
        return;
    }
    let ch = w.ids.ch(20);
    let seeded = run_node("rust/fixtures/gen/feat-tempvoice-js.mjs", &["seed", &w.g(), &w.ids.hub(), &ch, &w.ids.u(1), &w.ids.r(2)]);
    assert!(seeded.get("error").is_none(), "{seeded}");
    // Rust reads the JS rows.
    let hub = tv::find_hub_by_channel(&w.t.pool, &w.g(), &w.ids.hub()).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(&hub).unwrap()["id"], seeded["hub"]["id"]);
    assert_eq!(hub.name_format, "{username} room");
    assert_eq!(hub.moderator_role_ids, vec![w.ids.r(2)]);
    let row = tv::load_row(&w.t, &ch).await.unwrap().unwrap();
    assert_eq!(row.row.banned_user_ids, vec!["555555555555555555".to_string()]);
    assert!(row.row.locked && row.row.empty_since.is_some());
    assert_eq!(row.hub.id, hub.id);
    let hubs = tv::list_hubs(&w.t.pool, &w.g()).await.unwrap();
    assert_eq!(hubs[0].channels, 1);
    // The JS rename stamps are read from Redis (merged with the in-memory ones).
    let stamps = tv::rename_stamps(&w.t, &ch).await;
    let js: Vec<f64> = seeded["stamps"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!(stamps, js);
    // The JS voice-role settings.
    let settings = w.t.settings().get_settings(&w.g()).await.unwrap();
    assert_eq!(vr::links_of(Some(&settings)), vec![vr::VoiceRoleLink { channel_id: w.ids.hub(), role_id: w.ids.r(2) }]);
    assert_eq!(vr::any_roles_of(Some(&settings)), vec![w.ids.r(2)]);
    // The JS keep-alive job runs under Rust: the channel does not exist (10003 on an available guild), so the row goes.
    let job = w.t.scheduler().get_job(&tv::cleanup_key(&ch)).await.expect("the JS job");
    let ctx = JobCtx { app: w.t.app.clone(), guild_id: w.g(), late_by_ms: 0, attempt: 1 };
    assert_eq!(tv::handle_cleanup_job(Job::from_row(&job), ctx).await.unwrap(), JobOutcome::Done);
    assert!(tv::load_row(&w.t, &ch).await.unwrap().is_none());
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn rust_writes_js_reads() {
    let Some(w) = world(18, |g, _| g).await else { return };
    if bot_core::testing::redis_url().is_none() {
        return;
    }
    let u1 = w.ids.u(1);
    let hub = w.create_hub(vec![("keepAliveMinutes", json!(5)), ("bitrate", json!(64000)), ("nameFormat", json!("{username} room"))]).await;
    assert_eq!(tv::update_hub_role_list(&w.t, &w.g(), &hub.id, "ignoredRoleIds", "add", &w.ids.r(3)).await.unwrap(), RoleListOutcome::Ok);
    w.mv(&u1, Some(&w.ids.hub())).await;
    let ch = temp_of(&w, &u1).await;
    assert!(w.action(&u1, &ch, "ban", Some(&w.ids.u(2)), ActionValue::None).await.0);
    assert!(w.action(&u1, &ch, "rename", None, ActionValue::Text("Renamed".into())).await.0);
    w.mv(&u1, None).await; // keep-alive job + emptySince
    assert_eq!(vr::add_link(&w.t, &w.g(), &w.ids.hub(), &w.ids.r(2)).await.unwrap(), vr::AddOutcome::Changed);
    assert_eq!(vr::add_any_role(&w.t, &w.g(), &w.ids.r(3)).await.unwrap(), vr::AddOutcome::Changed);

    let js = run_node("rust/fixtures/gen/feat-tempvoice-js.mjs", &["read", &w.g(), &w.ids.hub(), &ch]);
    assert!(js.get("error").is_none(), "{js}");
    let rust_hub = serde_json::to_value(tv::find_hub_by_channel(&w.t.pool, &w.g(), &w.ids.hub()).await.unwrap().unwrap()).unwrap();
    assert_eq!(js["hub"], rust_hub, "the JS reads the hub exactly as Rust has it");
    assert_eq!(js["hubs"], json!([{ "id": hub.id, "channels": 1 }]));
    let rust_row = serde_json::to_value(w.row(&ch).await.unwrap()).unwrap();
    let mut js_row = js["row"].clone();
    js_row.as_object_mut().unwrap().remove("hub");
    assert_eq!(js_row, rust_row);
    assert_eq!(js["row"]["bannedUserIds"], json!([w.ids.u(2)]));
    assert_eq!(js["job"], json!({ "type": "tempvoice.cleanup", "payload": { "channelId": ch }, "dedupeKey": tv::cleanup_key(&ch), "maxAttempts": 5, "guildId": w.g() }));
    assert_eq!(js["stamps"].as_array().unwrap().len(), 1, "the JS reads the Rust rename stamps");
    assert_eq!(js["links"], json!([{ "channelId": w.ids.hub(), "roleId": w.ids.r(2) }]));
    assert_eq!(js["any"], json!([w.ids.r(3)]));
    assert_eq!(js["managed"], json!([w.ids.r(2), w.ids.r(3)]));
    assert_eq!(js["isTemp"], json!(true));
    assert_eq!(js["count"], json!(1));
    let _: Value = js;
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn creation_failures_leave_no_row_and_the_module_toggle_is_honoured() {
    let Some(w) = world(19, |g, _| g).await else { return };
    let u1 = w.ids.u(1);
    w.create_hub(vec![("createTextChannel", json!(true))]).await;
    // Discord refuses the voice channel: nothing is saved and the member stays in the hub.
    w.rest.fail_op("create_channel", 50013);
    w.mv(&u1, Some(&w.ids.hub())).await;
    assert!(w.row_of_owner(&u1).await.is_none());
    assert_eq!(voice_of(&w.t, &w.g(), &u1), Some(w.ids.hub()));
    w.rest.clear_fail("create_channel");
    // While the module is off a hub join does nothing.
    w.t.settings().update_settings(&w.g(), bot_core::settings::SettingsPatch::new().set("tempVoiceEnabled", false)).await.unwrap();
    w.rest.clear();
    w.mv(&w.ids.u(2), Some(&w.ids.hub())).await;
    assert!(w.rest.calls_of("create_channel").is_empty());
    assert!(w.row_of_owner(&w.ids.u(2)).await.is_none());
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn the_voice_roles_hook_follows_voice_changes_while_the_module_is_on() {
    let Some(w) = world(20, |g, _| g).await else { return };
    let u1 = w.ids.u(1);
    assert_eq!(vr::add_link(&w.t, &w.g(), &w.ids.cat(), &w.ids.r(2)).await.unwrap(), vr::AddOutcome::Changed);
    // Module off (the default): nothing changes.
    w.mv(&u1, Some(&w.ids.ch(3))).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    assert!(w.rest.calls_of("add_role").is_empty());
    w.t.settings().update_settings(&w.g(), bot_core::settings::SettingsPatch::new().set("voiceRolesEnabled", true)).await.unwrap();
    w.mv(&u1, Some(&w.ids.ch(3))).await;
    w.mv(&u1, Some(&w.ids.hub())).await;
    let adds = w.rest.calls_of("add_role");
    assert_eq!(adds.len(), 1, "{:?}", w.rest.ops());
    assert_eq!(adds[0]["role"], json!(w.ids.r(2)));
    w.mv(&u1, None).await;
    assert_eq!(w.rest.calls_of("remove_role").len(), 1);
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn reconcile_deletes_a_hub_only_after_a_confirmed_10003() {
    let Some(w) = world(21, |g, _| g).await else { return };
    let g = gid(&w.g());
    w.create_hub(vec![]).await;
    let hub_ch = channel(&w.t, &w.g(), &w.ids.hub()).unwrap();
    let mut ev: serenity::all::ChannelDeleteEvent = serde_json::from_value(serde_json::to_value(&hub_ch).unwrap()).unwrap();
    w.t.discord().unwrap().cache.update(&mut ev);
    w.rest.fail_op("fetch_channel", 500);
    tv::reconcile_guild(&w.t, g, None).await;
    assert!(w.hub_row().await.is_some(), "a 500 is not a deletion");
    w.rest.clear_fail("fetch_channel");
    tv::reconcile_guild(&w.t, g, None).await;
    assert!(w.hub_row().await.is_none(), "a confirmed 10003 with no open channels deletes the hub");
    w.t.reset_guilds(&[&w.g()]).await;
}

#[tokio::test]
async fn the_panel_is_posted_when_the_gateway_echo_lags_and_healing_and_retries_work() {
    let Some(w) = world(22, |g, _| g).await else { return };
    let u1 = w.ids.u(1);
    w.create_hub(vec![("createTextChannel", json!(true))]).await;
    w.rest.channel_echo.store(false, std::sync::atomic::Ordering::Relaxed);
    w.mv(&u1, Some(&w.ids.hub())).await;
    let row = w.row_of_owner(&u1).await.expect("row");
    assert_eq!(w.rest.calls_of("send_message").len(), 1, "{:?}", w.rest.ops());
    assert_eq!(w.rest.calls_of("pin_message").len(), 1);
    w.rest.channel_echo.store(true, std::sync::atomic::Ordering::Relaxed);

    // The text channel vanished (fetch answers 10003): the next sync forgets it.
    let text = row.text_channel_id.clone().unwrap();
    let t_ch = channel(&w.t, &w.g(), &text).unwrap();
    let mut ev: serenity::all::ChannelDeleteEvent = serde_json::from_value(serde_json::to_value(&t_ch).unwrap()).unwrap();
    w.t.discord().unwrap().cache.update(&mut ev);
    tv::sync_text_overwrites(&w.t, gid(&w.g()), &row.channel_id).await.unwrap();
    let r = w.row(&row.channel_id).await.unwrap();
    assert!(r.text_channel_id.is_none() && r.control_message_id.is_none());

    // keep alive 0 and Discord refuses the delete: the row stays and a retry job is scheduled.
    w.rest.fail_op("delete_channel", 50013);
    w.mv(&u1, None).await;
    assert!(w.row(&row.channel_id).await.is_some());
    let job = w.t.scheduler().get_job(&tv::cleanup_key(&row.channel_id)).await.expect("retry job");
    assert_eq!(job.payload, json!({ "channelId": row.channel_id }));
    w.rest.clear_fail("delete_channel");
    w.t.reset_guilds(&[&w.g()]).await;
}
