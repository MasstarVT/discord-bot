//! Database tests (`TEST_DATABASE_URL`; each returns early without it): the services against the real tables, the
//! hooks, jobs, components and commands through the router with an offline fake guild and a recording stand-in for
//! Discord REST, the raw SQL, and both directions of the JS round trip (`rust/fixtures/gen/feat-community-a-js.mjs`).

mod common;

use std::sync::Arc;

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, database_url, run_node};
use chrono::{DateTime, TimeZone, Utc};
use common::{FakeIo, bot_message, fields_of};
use feat_community_a::bump_service as bump;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Permissions as P};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_community_a::register(&mut reg);
    reg
}

const OWNER: &str = "600000000000000099";
const ADMIN: &str = "600000000000000098";
const MOD: &str = "600000000000000097";
const BOT_ROLE: &str = "820000000000000001";
const ADMIN_ROLE: &str = "820000000000000002";
const MOD_ROLE: &str = "820000000000000003";
const PING_ROLE: &str = "820000000000000004";
const QUIET_ROLE: &str = "820000000000000005";
const DISBOARD: &str = "302050872383242240";

fn bot_perms() -> P {
    P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::ATTACH_FILES | P::CREATE_PUBLIC_THREADS | P::SEND_MESSAGES_IN_THREADS | P::READ_MESSAGE_HISTORY
}

/// The standard fake guild: the bot (posting rights), an admin, a moderator role, text channels `...01` (open),
/// `...02` (open), `...03` (the bot cannot send), `...04` (private: @everyone cannot view).
fn world(g: &str) -> FakeGuild {
    let ch = |n: u8| format!("8{}{:02}", &g[1..16], n);
    FakeGuild::new(g, OWNER)
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role(BOT_ROLE, 3, bot_perms())
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role(ADMIN_ROLE, 2, P::MANAGE_GUILD)
        .member(ADMIN, &[ADMIN_ROLE])
        .role(MOD_ROLE, 1, P::empty())
        .member(MOD, &[MOD_ROLE])
        .role_with(PING_ROLE, 1, P::empty(), |r| r["mentionable"] = json!(true))
        .role(QUIET_ROLE, 1, P::empty())
        .channel(&ch(1), 0, "one", &[])
        .channel(&ch(2), 0, "two", &[])
        .channel(&ch(3), 0, "locked", &[(BOT_ROLE, false, P::empty(), P::SEND_MESSAGES)])
        .channel(&ch(4), 0, "private", &[(g, false, P::empty(), P::VIEW_CHANNEL), (BOT_ROLE, false, P::VIEW_CHANNEL, P::empty()), (MOD_ROLE, false, P::VIEW_CHANNEL, P::empty())])
        .channel(&ch(5), 0, "no-threads", &[(BOT_ROLE, false, P::empty(), P::CREATE_PUBLIC_THREADS)])
        .channel(&ch(6), 0, "no-files", &[(BOT_ROLE, false, P::empty(), P::ATTACH_FILES)])
        // A public thread under `locked` (the bot cannot Send Messages there, but can Send Messages in Threads).
        .thread(&ch(7), 11, &ch(3))
}

/// The id of channel `n` of [`world`].
fn ch(g: &str, n: u8) -> String {
    format!("8{}{:02}", &g[1..16], n)
}

struct Env {
    t: DbTestApp,
    g: &'static str,
    io: Arc<FakeIo>,
    /// Discord REST behind `app.http()` (channel fetches for uncached channels); the posts go through `io`.
    rest: FakeRest,
}

impl Env {
    async fn new(g: &'static str) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([g]);
        t.reset_guilds(&[g]).await;
        let rest = FakeRest::new();
        t.set_discord(rest.discord(&[world(g).json()]));
        t.install(registry());
        let io = FakeIo::install(&t);
        Some(Self { t, g, io, rest })
    }
    fn gid(&self) -> GuildId {
        GuildId::new(self.g.parse().unwrap())
    }
    fn ch(&self, n: u8) -> String {
        ch(self.g, n)
    }
    async fn set(&self, patch: SettingsPatch) {
        self.t.settings().update_settings(self.g, patch).await.expect("settings");
    }
    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
    fn slash(&self, name: &str, sub: &str, user: &str) -> FakeInteraction {
        let f = FakeInteraction::slash(name).guild(self.g).user(user).subcommand(sub).channel(&self.ch(1));
        match user {
            ADMIN => f.roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD),
            MOD => f.roles(&[MOD_ROLE]),
            _ => f,
        }
    }
    async fn route(&self, i: serenity::all::Interaction) {
        self.t.responses.clear();
        bot_core::router::route_interaction(&self.t, i).await;
    }
    fn last(&self) -> bot_core::interaction::RecordedCall {
        self.t.responses.calls().last().cloned().expect("an answer")
    }
}

#[path = "db/suggestions.rs"]
mod suggestions;

#[path = "db/confessions.rs"]
mod confessions;

fn ms(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).unwrap()
}

// ══ Bumps ═════════════════════════════════════════════════════════════════════

async fn bumps_on(e: &Env) {
    e.set(
        SettingsPatch::new()
            .set("bumpsEnabled", true)
            .set("bumpChannelId", Value::Null)
            .set("bumpRoleId", Value::Null)
            .set("bumpReminderMessage", Value::Null)
            .set("bumpThanks", true)
            .set("bumpThanksMessage", Value::Null)
            .set("bumpRewardCoins", 0)
            .set("economyEnabled", false),
    )
    .await;
}

async fn job_row(e: &Env) -> Option<bot_db::ScheduledJob> {
    e.t.scheduler().get_job(&bump::reminder_dedupe_key(e.g, "disboard")).await
}

#[tokio::test]
async fn record_bump_writes_the_state_schedules_the_reminder_and_counts() {
    let Some(e) = Env::new("931000000000000001").await else { return };
    let at = ms(1_767_323_045_678);
    let out = bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some("600000000000000001"), at).await.unwrap();
    assert!(out.recorded);
    assert_eq!(out.count, Some(1));
    assert_eq!(out.next_bump_at, Some(ms(1_767_323_045_678 + 7_200_000)));
    let s = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap();
    assert_eq!((s.last_bump_at, s.next_bump_at, s.channel_id.clone(), s.last_bumper_id.clone(), s.reminded_at), (at, ms(1_767_323_045_678 + 7_200_000), e.ch(1), Some("600000000000000001".into()), None));
    let job = job_row(&e).await.expect("the reminder job");
    assert_eq!(job.r#type, "bump.remind");
    assert_eq!(job.payload, json!({ "site": "disboard" }));
    assert_eq!(job.run_at, ms(1_767_323_045_678 + 7_200_000));

    // The same bump again, or an older one, changes nothing.
    assert!(!bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some("600000000000000001"), at).await.unwrap().recorded);
    assert!(!bump::record_bump(&e.t, e.g, "disboard", &e.ch(2), Some("600000000000000002"), ms(1_767_323_000_000)).await.unwrap().recorded);
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000001").await.unwrap(), 1);
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000002").await.unwrap(), 0);

    // A newer bump replaces the state, clears remindedAt and raises the count.
    sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = "lastBumpAt" WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    let later = ms(1_767_323_045_678 + 7_300_000);
    let out = bump::record_bump(&e.t, e.g, "disboard", &e.ch(2), Some("600000000000000001"), later).await.unwrap();
    assert_eq!((out.recorded, out.count), (true, Some(2)));
    let s = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap();
    assert_eq!((s.channel_id.clone(), s.reminded_at), (e.ch(2), None));
    assert_eq!(job_row(&e).await.unwrap().run_at, ms(1_767_323_045_678 + 7_300_000 + 7_200_000), "the reminder moved");

    // No bumper: the state and the reminder, but no count.
    let out = bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), None, ms(1_767_323_045_678 + 20_000_000)).await.unwrap();
    assert_eq!((out.recorded, out.count), (true, None));
    assert_eq!(bump::top_bumpers(e.t.db(), e.g, 10).await.unwrap().len(), 1);

    // An unknown site is an error.
    assert!(bump::record_bump(&e.t, e.g, "topgg", &e.ch(1), None, at).await.is_err());
    e.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_bumps_of_one_guild_run_one_at_a_time() {
    let Some(e) = Env::new("931000000000000002").await else { return };
    let base = 1_767_323_045_678;
    let futs: Vec<_> = (0..6).map(|i| bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some("600000000000000001"), ms(base + i * 1000))).collect();
    let mut recorded = 0;
    for f in futs {
        if f.await.unwrap().recorded {
            recorded += 1;
        }
    }
    assert_eq!(recorded, 6, "queued in call order, each newer than the last");
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000001").await.unwrap(), 6);
    let s = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap();
    assert_eq!(s.last_bump_at, ms(base + 5000));
    e.done().await;
}

fn bump_embed() -> Value {
    json!([{ "description": "Bump done! :thumbsup:" }])
}

async fn dispatch(e: &Env, message: serenity::all::Message, edited: bool) {
    let channel_id = message.channel_id;
    e.t.run_hooks(BotMessage { guild_id: e.gid(), message, channel_id, channel: None, edited, was_loading: None }).await;
}

#[tokio::test]
async fn a_bump_is_recorded_and_the_bumper_thanked_and_only_they_can_be_pinged() {
    let Some(e) = Env::new("931000000000000003").await else { return };
    bumps_on(&e).await;
    let m = bot_message("700000000000000001", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z");
    dispatch(&e, m.clone(), false).await;
    let s = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().expect("recorded");
    assert_eq!(s.last_bump_at, ms(1_767_323_045_678));
    assert!(job_row(&e).await.is_some());
    let sent = e.io.of("send");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].channel, e.ch(1));
    assert_eq!(sent[0].content(), "📣 Thanks for bumping, <@600000000000000001>! The next bump is possible <t:1767330245:R>.");
    assert_eq!(sent[0].body["allowed_mentions"], json!({ "parse": [], "users": ["600000000000000001"], "roles": [] }));

    // The same message again (an edit, a replay) is ignored.
    dispatch(&e, m, true).await;
    assert_eq!(e.io.of("send").len(), 1);

    // A custom thank-you gets {count} and {site}; its role and everyone mentions still cannot ping.
    e.set(SettingsPatch::new().set("bumpThanksMessage", "@everyone <@&820000000000000004> {user} bumped {site} {count} times")).await;
    let m = bot_message("700000000000000002", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T06:00:00.000Z");
    dispatch(&e, m, false).await;
    let sent = e.io.of("send");
    assert_eq!(sent[1].content(), "@everyone <@&820000000000000004> <@600000000000000001> bumped Disboard 2 times");
    assert_eq!(sent[1].body["allowed_mentions"], json!({ "parse": [], "users": ["600000000000000001"], "roles": [] }));
    e.done().await;
}

#[tokio::test]
async fn a_bump_in_a_thread_is_thanked_with_send_messages_in_threads() {
    let Some(e) = Env::new("931000000000000015").await else { return };
    bumps_on(&e).await;
    dispatch(&e, bot_message("700000000000000041", e.g, &e.ch(7), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), false).await;
    let sent = e.io.of("send");
    assert_eq!(sent.len(), 1, "thanked although the parent denies Send Messages");
    assert_eq!(sent[0].channel, e.ch(7));
    assert_eq!(bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap().channel_id, e.ch(7));
    e.done().await;
}

#[tokio::test]
async fn failures_other_bots_and_unknown_bumpers() {
    let Some(e) = Env::new("931000000000000004").await else { return };
    bumps_on(&e).await;
    let wait = json!([{ "description": "Please wait another 87 minutes until the server can be bumped" }]);
    dispatch(&e, bot_message("700000000000000011", e.g, &e.ch(1), DISBOARD, wait, Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), false).await;
    dispatch(&e, bot_message("700000000000000012", e.g, &e.ch(1), "999999999999999999", bump_embed(), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), false).await;
    assert!(bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().is_none());
    assert!(e.io.calls().is_empty());

    // A deferred answer counts once it is edited into "Bump done".
    dispatch(&e, bot_message("700000000000000013", e.g, &e.ch(1), DISBOARD, json!([]), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), false).await;
    assert!(bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().is_none());
    dispatch(&e, bot_message("700000000000000013", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), true).await;
    assert!(bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().is_some());

    // No known bumper: the reminder is set, but no thank-you and no count.
    dispatch(&e, bot_message("700000000000000014", e.g, &e.ch(1), DISBOARD, bump_embed(), None, "2026-01-02T07:04:05.678Z"), false).await;
    let s = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap();
    assert_eq!(s.last_bumper_id, None);
    assert_eq!(e.io.of("send").len(), 1, "only the first bump was thanked");
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000001").await.unwrap(), 1);
    e.done().await;
}

#[tokio::test]
async fn thanks_off_no_permission_and_module_off() {
    let Some(e) = Env::new("931000000000000005").await else { return };
    bumps_on(&e).await;
    e.set(SettingsPatch::new().set("bumpThanks", false)).await;
    dispatch(&e, bot_message("700000000000000021", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), false).await;
    assert!(bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().is_some());
    assert!(e.io.calls().is_empty());

    // Thanks on, but the bot cannot send in that channel: still recorded.
    e.set(SettingsPatch::new().set("bumpThanks", true)).await;
    dispatch(&e, bot_message("700000000000000022", e.g, &e.ch(3), DISBOARD, bump_embed(), Some(("600000000000000002", false)), "2026-01-02T06:04:05.678Z"), false).await;
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000002").await.unwrap(), 1);
    assert!(e.io.calls().is_empty());

    // A failing send does not stop the count.
    e.io.fail_with("send", Some(500));
    dispatch(&e, bot_message("700000000000000023", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000003", false)), "2026-01-02T09:04:05.678Z"), false).await;
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000003").await.unwrap(), 1);
    e.io.heal();

    // The module off: the hook is gated, nothing is recorded.
    e.set(SettingsPatch::new().set("bumpsEnabled", false)).await;
    dispatch(&e, bot_message("700000000000000024", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000004", false)), "2026-01-02T12:04:05.678Z"), false).await;
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000004").await.unwrap(), 0);
    e.done().await;
}

struct Coins(std::sync::Mutex<Vec<(String, i64, String)>>);

#[async_trait::async_trait]
impl RewardProvider for Coins {
    async fn grant(&self, _app: &App, args: RewardArgs) -> BotResult<RewardResult> {
        self.0.lock().unwrap().push((args.user_id.clone(), args.amount, args.reason.clone()));
        Ok(RewardResult::ok(Some(10)))
    }
    async fn take(&self, _app: &App, _args: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(None))
    }
}

#[tokio::test]
async fn bump_reward_coins_are_granted() {
    let Some(e) = Env::new("931000000000000006").await else { return };
    bumps_on(&e).await;
    let coins = Arc::new(Coins(Default::default()));
    e.t.rewards().register_provider(RewardKind::Coins, coins.clone(), Some("test")).unwrap();
    e.set(SettingsPatch::new().set("bumpRewardCoins", 25).set("economyEnabled", true)).await;
    dispatch(&e, bot_message("700000000000000031", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T03:04:05.678Z"), false).await;
    assert_eq!(coins.0.lock().unwrap().clone(), vec![("600000000000000001".to_string(), 25, "Bumped the server".to_string())]);
    e.set(SettingsPatch::new().set("bumpRewardCoins", 0)).await;
    dispatch(&e, bot_message("700000000000000032", e.g, &e.ch(1), DISBOARD, bump_embed(), Some(("600000000000000001", false)), "2026-01-02T06:04:05.678Z"), false).await;
    assert_eq!(coins.0.lock().unwrap().len(), 1, "0 grants nothing");
    e.done().await;
}

fn remind_job(g: &str, payload: Value) -> Job {
    Job { id: "job".into(), guild_id: g.into(), job_type: "bump.remind".into(), payload, run_at: Utc::now(), attempts: 1, max_attempts: 5, dedupe_key: None, version: 0 }
}

async fn remind(e: &Env) -> BotResult<JobOutcome> {
    let ctx = JobCtx { app: e.t.app.clone(), guild_id: e.g.into(), late_by_ms: 0, attempt: 1 };
    bump::handle_remind_job(remind_job(e.g, json!({ "site": "disboard" })), ctx).await
}

async fn due_state(e: &Env, channel: &str) {
    let at = Utc::now() - chrono::Duration::hours(3);
    bump::record_bump(&e.t, e.g, "disboard", channel, Some("600000000000000001"), at).await.unwrap();
}

async fn reminded(e: &Env) -> bool {
    bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().is_some_and(|s| s.reminded_at.is_some())
}

#[tokio::test]
async fn the_reminder_posts_once_in_the_right_channel() {
    let Some(e) = Env::new("931000000000000007").await else { return };
    bumps_on(&e).await;
    due_state(&e, &e.ch(1)).await;
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
    let sent = e.io.of("send");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].channel, e.ch(1));
    assert_eq!(sent[0].content(), bump::DEFAULT_REMINDER_MESSAGE);
    assert_eq!(sent[0].body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    assert!(reminded(&e).await);
    // Running again reminds once (the claim).
    remind(&e).await.unwrap();
    assert_eq!(e.io.of("send").len(), 1);

    // Two at once also remind once.
    due_state(&e, &e.ch(1)).await;
    sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL, "lastBumpAt" = "lastBumpAt" + interval '1 second' WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    e.io.clear();
    let (a, b) = tokio::join!(remind(&e), remind(&e));
    a.unwrap();
    b.unwrap();
    assert_eq!(e.io.of("send").len(), 1);

    // bumpChannelId wins over the channel of the last bump; a dead one falls back.
    sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    e.set(SettingsPatch::new().set("bumpChannelId", e.ch(2))).await;
    e.io.clear();
    remind(&e).await.unwrap();
    assert_eq!(e.io.of("send")[0].channel, e.ch(2));
    sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    e.set(SettingsPatch::new().set("bumpChannelId", "899999999999999999")).await;
    e.io.clear();
    remind(&e).await.unwrap();
    assert_eq!(e.io.of("send")[0].channel, e.ch(1));
    e.done().await;
}

#[tokio::test]
async fn the_reminder_pings_only_a_pingable_role_and_falls_back_to_the_default_text() {
    let Some(e) = Env::new("931000000000000008").await else { return };
    bumps_on(&e).await;
    e.set(SettingsPatch::new().set("bumpRoleId", PING_ROLE).set("bumpReminderMessage", "Bump {site} now!")).await;
    due_state(&e, &e.ch(1)).await;
    remind(&e).await.unwrap();
    let s = &e.io.of("send")[0];
    assert_eq!(s.content(), format!("<@&{PING_ROLE}> Bump Disboard now!"));
    assert_eq!(s.body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [PING_ROLE] }));

    // An unmentionable role is not pinged (the bot has no Mention Everyone); a blank text falls back to the default.
    e.set(SettingsPatch::new().set("bumpRoleId", QUIET_ROLE).set("bumpReminderMessage", "   ")).await;
    sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    e.io.clear();
    remind(&e).await.unwrap();
    let s = &e.io.of("send")[0];
    assert_eq!(s.content(), bump::DEFAULT_REMINDER_MESSAGE);
    assert_eq!(s.body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    e.done().await;
}

#[tokio::test]
async fn the_reminder_job_edge_cases() {
    let Some(e) = Env::new("931000000000000009").await else { return };
    bumps_on(&e).await;
    // No state: nothing.
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
    // Unknown site: permanent.
    let ctx = JobCtx { app: e.t.app.clone(), guild_id: e.g.into(), late_by_ms: 0, attempt: 1 };
    let err = bump::handle_remind_job(remind_job(e.g, json!({ "site": "nope" })), ctx.clone()).await.unwrap_err();
    assert_eq!(err.downcast_ref::<PermanentJobError>().map(|p| p.message.as_str()), Some("bump.remind: unknown site \"nope\""));
    let err = bump::handle_remind_job(remind_job(e.g, json!({})), ctx).await.unwrap_err();
    assert_eq!(err.downcast_ref::<PermanentJobError>().map(|p| p.message.as_str()), Some("bump.remind: unknown site undefined"));

    // The next bump is still ahead: reschedule for then.
    let soon = Utc::now() - chrono::Duration::minutes(30);
    bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), None, soon).await.unwrap();
    let next = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap().next_bump_at;
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Reschedule { next_run_at: next, payload: None });
    assert!(e.io.calls().is_empty());

    // Module off: nothing.
    sqlx::query(r#"DELETE FROM "BumpState" WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    due_state(&e, &e.ch(1)).await;
    e.set(SettingsPatch::new().set("bumpsEnabled", false)).await;
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
    assert!(!reminded(&e).await);
    e.set(SettingsPatch::new().set("bumpsEnabled", true)).await;

    // A transient send error releases the claim and fails, so the retry reminds.
    e.io.fail_with("send", Some(500));
    assert!(remind(&e).await.is_err());
    assert!(!reminded(&e).await);
    // A permanent one (missing permissions) ends quietly and releases the claim.
    e.io.fail_with("send", Some(50013));
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
    assert!(!reminded(&e).await);
    e.io.heal();
    // No usable channel: ends quietly, claim released.
    sqlx::query(r#"UPDATE "BumpState" SET "channelId" = $2 WHERE "guildId" = $1"#).bind(e.g).bind(e.ch(3)).execute(e.t.db()).await.unwrap();
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
    assert!(!reminded(&e).await);
    assert!(e.io.of("send").is_empty());
    e.done().await;
}

/// A text channel of guild `g` as `GET /channels/{id}` answers it (open to everyone).
fn channel_json(g: &str, id: &str) -> Value {
    json!({ "id": id, "type": 0, "guild_id": g, "name": "fetched", "position": 9, "permission_overwrites": [], "nsfw": false })
}

#[tokio::test]
async fn the_reminder_fetches_an_uncached_channel_and_retries_on_a_transient_fetch_error() {
    let Some(e) = Env::new("931000000000000041").await else { return };
    bumps_on(&e).await;
    due_state(&e, &e.ch(1)).await;
    // bumpChannelId is not in the cache: guild.channels.fetch finds it, and the reminder goes there.
    let fetched = "831000000000000099";
    e.set(SettingsPatch::new().set("bumpChannelId", fetched)).await;
    e.rest.on("GET", &format!("/channels/{fetched}"), 200, channel_json(e.g, fetched));
    assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
    assert_eq!(e.rest.calls_to("GET", &format!("/channels/{fetched}")).len(), 1);
    assert_eq!(e.io.of("send").iter().map(|c| c.channel.clone()).collect::<Vec<_>>(), vec![fetched.to_string()]);
    assert!(reminded(&e).await);

    // A fetch that fails for a reason a retry may fix (not unknown channel / missing access): the job fails and the
    // claim is released, so the retry reminds.
    sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    let flaky = "831000000000000098";
    e.set(SettingsPatch::new().set("bumpChannelId", flaky)).await;
    e.rest.fail("GET", &format!("/channels/{flaky}"), 500, 0, "Internal Server Error");
    e.io.clear();
    assert!(remind(&e).await.is_err());
    assert!(!reminded(&e).await);
    assert!(e.io.of("send").is_empty());

    // Unknown Channel (10003) or Missing Access (50001) skip to the channel of the last bump.
    for code in [10003, 50001] {
        e.rest.fail("GET", &format!("/channels/{flaky}"), 404, code, "gone");
        e.io.clear();
        assert_eq!(remind(&e).await.unwrap(), JobOutcome::Done);
        assert_eq!(e.io.of("send")[0].channel, e.ch(1), "code {code}");
        sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    }
    e.done().await;
}

#[tokio::test]
async fn bump_cleanup_hooks_unset_only_their_own_channel_and_role() {
    let Some(e) = Env::new("931000000000000010").await else { return };
    bumps_on(&e).await;
    e.set(SettingsPatch::new().set("bumpChannelId", e.ch(1)).set("bumpRoleId", PING_ROLE)).await;
    let channel = |id: &str| -> serenity::all::GuildChannel {
        bot_core::interaction_helpers::cached_channel(&e.t, e.gid(), ChannelId::new(id.parse().unwrap())).expect("cached")
    };
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: channel(&e.ch(2)) }).await;
    assert_eq!(e.t.settings().get_settings(e.g).await.unwrap().bump_channel_id, Some(e.ch(1)));
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: channel(&e.ch(1)) }).await;
    assert_eq!(e.t.settings().get_settings(e.g).await.unwrap().bump_channel_id, None);
    e.t.run_hooks(RoleDelete { guild_id: e.gid(), role_id: serenity::all::RoleId::new(QUIET_ROLE.parse().unwrap()), role: None }).await;
    assert_eq!(e.t.settings().get_settings(e.g).await.unwrap().bump_role_id.as_deref(), Some(PING_ROLE));
    e.t.run_hooks(RoleDelete { guild_id: e.gid(), role_id: serenity::all::RoleId::new(PING_ROLE.parse().unwrap()), role: None }).await;
    assert_eq!(e.t.settings().get_settings(e.g).await.unwrap().bump_role_id, None);
    e.done().await;
}

#[tokio::test]
async fn top_bumpers_order_and_reset_counts() {
    let Some(e) = Env::new("931000000000000011").await else { return };
    e.t.settings().ensure_guild_settings(e.g).await.unwrap();
    for (u, c, at) in [("A", 3, 5), ("B", 5, 9), ("C", 3, 1), ("D", 0, 0)] {
        sqlx::query(r#"INSERT INTO "BumpCount" ("guildId", "userId", "count", "lastBumpAt") VALUES ($1, $2, $3, $4)"#)
            .bind(e.g)
            .bind(u)
            .bind(c)
            .bind(ms(at * 1000).naive_utc())
            .execute(e.t.db())
            .await
            .unwrap();
    }
    let top: Vec<String> = bump::top_bumpers(e.t.db(), e.g, 10).await.unwrap().into_iter().map(|r| r.user_id).collect();
    assert_eq!(top, vec!["B", "C", "A"], "most first, the earlier last bump wins a tie, 0 is left out");
    assert_eq!(bump::reset_counts(e.t.db(), e.g, Some("A")).await.unwrap(), 1);
    assert_eq!(bump::reset_counts(e.t.db(), e.g, Some("A")).await.unwrap(), 0);
    assert_eq!(bump::reset_counts(e.t.db(), e.g, None).await.unwrap(), 3);
    e.done().await;
}

#[tokio::test]
async fn bumps_status_and_leaderboard() {
    let Some(e) = Env::new("931000000000000012").await else { return };
    bumps_on(&e).await;
    e.route(e.slash("bumps", "status", "600000000000000001").build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!e.t.responses.calls()[0].ephemeral(), "public");
    let last = e.last();
    assert_eq!(last.embed_title(), Some("📣 Bump status"));
    assert_eq!(fields_of(&last.body)[0].1, "No bump seen yet. Use `/bump` with Disboard; I'll count it and remind you in time.");

    e.route(e.slash("bumps", "leaderboard", "600000000000000001").build()).await;
    assert_eq!(e.last().embed_description(), Some("Nobody has bumped yet. Be the first with `/bump`!"));

    for i in 0..12 {
        bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some(&format!("6100000000000000{i:02}")), Utc::now() - chrono::Duration::hours(30 - i)).await.unwrap();
    }
    e.route(e.slash("bumps", "status", "600000000000000001").build()).await;
    assert!(fields_of(&e.last().body)[0].1.ends_with("Next bump: **now**, the server can be bumped!"));
    e.route(e.slash("bumps", "leaderboard", "600000000000000001").build()).await;
    let d = e.last().embed_description().unwrap_or("").to_string();
    assert!(d.starts_with("🥇 <@610000000000000000> — 1 bump\n🥈"), "{d}");
    assert_eq!(d.lines().count(), 10);
    bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some("600000000000000001"), Utc::now() - chrono::Duration::hours(1)).await.unwrap();
    e.route(e.slash("bumps", "leaderboard", "600000000000000001").build()).await;
    assert_eq!(e.last().body["embeds"][0]["footer"]["text"], "You: 1 bump");
    e.route(e.slash("bumps", "status", "600000000000000001").build()).await;
    assert!(fields_of(&e.last().body)[0].1.contains("by <@600000000000000001>\nNext bump: <t:"));
    e.done().await;
}

#[tokio::test]
async fn bumps_setup_shows_saves_and_refuses() {
    let Some(e) = Env::new("931000000000000013").await else { return };
    bumps_on(&e).await;
    e.route(e.slash("bumps", "setup", ADMIN).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(e.last().embed_title(), Some("📣 Bump Reminders"));
    assert_eq!(
        e.last().embed_description(),
        Some("Module: ✅ On\nReminder channel: the channel of the last bump\nRole to ping: none\nThank the bumper: Yes\nCoins per bump: none")
    );

    // A member cannot.
    e.route(e.slash("bumps", "setup", "600000000000000001").build()).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);

    let set = e
        .slash("bumps", "setup", ADMIN)
        .option("channel", bot_core::command::option_type::CHANNEL, json!(e.ch(2)))
        .option("role", bot_core::command::option_type::ROLE, json!(QUIET_ROLE))
        .option("thanks", bot_core::command::option_type::BOOLEAN, json!(false))
        .option("coins", bot_core::command::option_type::INTEGER, json!(1500))
        .build();
    e.route(set).await;
    let last = e.last();
    assert_eq!(last.embed_title(), Some("✅ Bump Reminders Updated"));
    assert_eq!(
        last.embed_description(),
        Some(format!("Module: ✅ On\nReminder channel: <#{}>\nRole to ping: <@&{QUIET_ROLE}>\nThank the bumper: No\nCoins per bump: 1,500", e.ch(2)).as_str())
    );
    let f = fields_of(&last.body);
    assert_eq!(f[0].0, "Role");
    assert!(f[0].1.starts_with(&format!("⚠️ I can't ping <@&{QUIET_ROLE}>")));
    assert_eq!(f[1], ("Coins".into(), "⚠️ The Economy module is off, so no coins are given. Turn it on with `/module enable economy`.".into()));
    let s = e.t.settings().get_settings(e.g).await.unwrap();
    assert_eq!((s.bump_channel_id.clone(), s.bump_role_id.clone(), s.bump_thanks, s.bump_reward_coins), (Some(e.ch(2)), Some(QUIET_ROLE.into()), false, 1500));

    e.route(e.slash("bumps", "setup", ADMIN).option("channel", bot_core::command::option_type::CHANNEL, json!(e.ch(3))).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Cannot Use That Channel"));
    assert_eq!(e.last().embed_description(), Some(format!("I need **Send Messages** in <#{}>. Pick another channel, or fix my permissions there.", e.ch(3)).as_str()));
    e.route(e.slash("bumps", "setup", ADMIN).option("role", bot_core::command::option_type::ROLE, json!(e.g)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Cannot Ping @everyone"));
    assert_eq!(e.t.settings().get_settings(e.g).await.unwrap().bump_channel_id, Some(e.ch(2)), "nothing saved");
    e.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bumps_reset_one_member_or_everyone_after_a_confirm() {
    let Some(e) = Env::new("931000000000000014").await else { return };
    bumps_on(&e).await;
    for u in ["600000000000000001", "600000000000000002"] {
        bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some(u), Utc::now() - chrono::Duration::hours(5) + chrono::Duration::seconds(u[17..].parse::<i64>().unwrap())).await.unwrap();
    }
    e.route(e.slash("bumps", "reset", ADMIN).option("user", bot_core::command::option_type::USER, json!("600000000000000001")).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Bump Count Cleared"));
    assert_eq!(e.last().embed_description(), Some("The bump count of <@600000000000000001> was cleared."));
    e.route(e.slash("bumps", "reset", ADMIN).option("user", bot_core::command::option_type::USER, json!("600000000000000001")).build()).await;
    assert_eq!(e.last().embed_description(), Some("<@600000000000000001> had no bumps counted."));

    // Everyone: a confirm prompt first; Cancel changes nothing, Clear all empties the guild.
    for (pick, left) in [(1usize, 1usize), (0, 0)] {
        e.t.responses.clear();
        let t2 = e.t.app.clone();
        let i = e.slash("bumps", "reset", ADMIN).build();
        let run = tokio::spawn(async move { bot_core::router::route_interaction(&t2, i).await });
        for _ in 0..200 {
            if !e.t.collectors().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let prompt = e.t.responses.calls().into_iter().find(|c| c.body["components"].is_array()).expect("the prompt");
        assert_eq!(prompt.embed_title(), Some("⚠️  Clear all bump counts?"));
        let id = prompt.body["components"][0]["components"][pick]["custom_id"].as_str().unwrap().to_string();
        bot_core::router::route_interaction(&e.t, FakeInteraction::button(&id).guild(e.g).user(ADMIN).message_id("1000000000000000100").build()).await;
        run.await.unwrap();
        assert_eq!(bump::top_bumpers(e.t.db(), e.g, 10).await.unwrap().len(), left);
    }
    let last = e.last();
    assert_eq!(last.embed_description(), Some("Cleared the counts of 1 member."));
    e.done().await;
}

#[tokio::test]
async fn bump_raw_sql_runs() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    for (name, sql, binds) in bump::raw_sql() {
        let mut tx = pool.begin().await.unwrap();
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for b in binds {
            q = q.bind(b.as_str().unwrap().to_string());
        }
        // The guild row does not exist: the FK fails, which still proves the statement parses and plans.
        let r = q.execute(&mut *tx).await;
        if let Err(e) = &r {
            assert!(e.to_string().contains("foreign key"), "{name}: {e}");
        }
        tx.rollback().await.unwrap();
    }
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_records_a_bump_and_rust_reads_it_and_runs_its_reminder() {
    let Some(e) = Env::new("931000000000000020").await else { return };
    bumps_on(&e).await;
    let at = bot_db::format_js_date(&(Utc::now() - chrono::Duration::hours(3)));
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["bump-record", e.g, &e.ch(1), "600000000000000001", &at]);
    assert_eq!(js["out"]["recorded"], true, "{js}");
    let s = bump::get_bump_state(e.t.db(), e.g, "disboard").await.unwrap().unwrap();
    assert_eq!(bot_db::format_js_date(&s.last_bump_at), at);
    assert_eq!(json!(bot_db::format_js_date(&s.next_bump_at)), js["out"]["nextBumpAt"]);
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000001").await.unwrap(), 1);
    // The job the JS scheduled runs under Rust.
    let row = job_row(&e).await.expect("the JS job");
    let job = Job::from_row(&row);
    assert_eq!(job.kind(), Some(JobType::BumpRemind));
    let ctx = JobCtx { app: e.t.app.clone(), guild_id: e.g.into(), late_by_ms: 0, attempt: 1 };
    assert_eq!(bump::handle_remind_job(job, ctx).await.unwrap(), JobOutcome::Done);
    assert_eq!(e.io.of("send")[0].content(), bump::DEFAULT_REMINDER_MESSAGE);
    assert!(reminded(&e).await);
    // Rust records the same bump again: nothing changes; a newer one counts on top of the JS count.
    assert!(!bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some("600000000000000001"), s.last_bump_at).await.unwrap().recorded);
    e.done().await;
}

#[tokio::test]
async fn rust_records_bumps_and_js_reads_them() {
    let Some(e) = Env::new("931000000000000021").await else { return };
    let at = ms(1_767_323_045_678);
    bump::record_bump(&e.t, e.g, "disboard", &e.ch(1), Some("600000000000000001"), at).await.unwrap();
    bump::record_bump(&e.t, e.g, "disboard", &e.ch(2), Some("600000000000000002"), ms(1_767_323_045_678 + 60_000)).await.unwrap();
    bump::record_bump(&e.t, e.g, "disboard", &e.ch(2), Some("600000000000000002"), ms(1_767_323_045_678 + 120_000)).await.unwrap();
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["bump-read", e.g, "600000000000000001"]);
    assert!(js.get("error").is_none(), "{js}");
    let ours = serde_json::to_value(bump::get_bump_states(e.t.db(), e.g).await.unwrap()).unwrap();
    assert_eq!(ours, js["states"]);
    let top = serde_json::to_value(bump::top_bumpers(e.t.db(), e.g, 10).await.unwrap()).unwrap();
    assert_eq!(top, js["top"]);
    assert_eq!(js["own"], 1);
    assert_eq!(js["job"]["payload"], json!({ "site": "disboard" }));
    assert_eq!(js["job"]["type"], "bump.remind");
    assert_eq!(js["job"]["runAt"], "2026-01-02T05:06:05.678Z");
    assert_eq!(js["job"]["maxAttempts"], 5, "the scheduler's default, as the JS writes it");
    // A JS reset reads the Rust rows.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["bump-reset", e.g, "600000000000000002"]);
    assert_eq!(js["removed"], 1);
    assert_eq!(bump::bump_count_of(e.t.db(), e.g, "600000000000000002").await.unwrap(), 0);
    e.done().await;
}
