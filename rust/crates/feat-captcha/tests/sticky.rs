//! Port of `test/sticky.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: the debounce scheduler (paused tokio time), rendering and the texts against
//! `golden/feat-captcha/golden.json`, the hook filters. With `TEST_DATABASE_URL`: the re-send flow against the real
//! `StickyMessage` table and a fake Discord REST, the admin operations, the hooks, the form, `/sticky`, and both
//! cross-language directions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, load_golden, redis_url, run_node};
use feat_captcha::plugins::sticky as plugin;
use feat_captcha::sticky_service::{self as svc, Debouncer, ResendStatus, RunFn, SetArgs, SetResult};
use serde_json::{Value, json};
use serenity::all::Permissions as P;

fn golden() -> Value {
    load_golden("feat-captcha/golden.json")
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_captcha::register(&mut reg);
    reg
}

// ── Scheduler (paused time) ───────────────────────────────────────────────────

fn counting_run(log: Arc<Mutex<Vec<String>>>, hold_ms: u64, fail: bool) -> RunFn<u32> {
    Arc::new(move |id: String, arg: u32| {
        let log = log.clone();
        Box::pin(async move {
            log.lock().map(|mut l| l.push(format!("{id}:{arg}"))).ok();
            tokio::time::sleep(Duration::from_millis(hold_ms)).await;
            if fail { Err(anyhow::anyhow!("boom")) } else { Ok(()) }
        })
    })
}

async fn advance(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[tokio::test(start_paused = true)]
async fn scheduler_runs_once_after_the_channel_went_quiet() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let s = Debouncer::new(3_000, 15_000, counting_run(log.clone(), 0, false));
    s.touch("C1", 1);
    advance(2_000).await;
    s.touch("C1", 2);
    advance(2_000).await;
    assert!(log.lock().map(|l| l.is_empty()).unwrap_or(false), "still talking");
    advance(1_100).await;
    assert_eq!(*log.lock().expect("log"), vec!["C1:2".to_string()], "one run, the latest arg");
    assert!(!s.has("C1"));
    assert_eq!(s.size(), 0);
}

#[tokio::test(start_paused = true)]
async fn scheduler_serves_a_busy_channel_after_the_maximum_wait() {
    let at = Arc::new(Mutex::new(Vec::new()));
    let at2 = at.clone();
    let stamped: RunFn<u32> = Arc::new(move |_id, _arg| {
        let at = at2.clone();
        Box::pin(async move {
            at.lock().map(|mut a| a.push(tokio::time::Instant::now())).ok();
            Ok(())
        })
    });
    let s = Debouncer::new(40, 100, stamped);
    let start = tokio::time::Instant::now();
    // ~135 ms of chat, never quiet for 40 ms.
    for i in 0..9 {
        s.touch("C", i);
        advance(15).await;
    }
    let runs = at.lock().expect("at").clone();
    assert_eq!(runs.len(), 1, "the first run came while the chat went on");
    let ms = (runs[0] - start).as_millis();
    assert!((90..135).contains(&ms), "ran {ms} ms in");
    advance(80).await;
    assert_eq!(at.lock().expect("at").len(), 2, "the rest of the chat gets its own run");
    assert_eq!(s.size(), 0);
}

#[tokio::test(start_paused = true)]
async fn scheduler_channels_are_independent_a_touch_during_a_run_reruns_and_cancel_and_failures() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let s = Debouncer::new(3_000, 15_000, counting_run(log.clone(), 2_000, false));
    s.touch("A", 1);
    s.touch("B", 1);
    advance(3_100).await;
    assert_eq!(log.lock().expect("log").len(), 2);
    s.touch("A", 2); // during A's run: dirty
    advance(2_000).await;
    assert!(s.has("A"));
    advance(3_100).await;
    assert_eq!(log.lock().expect("log").iter().filter(|l| l.starts_with("A:")).count(), 2, "rerun after the run");
    advance(3_000).await;
    s.touch("C", 1);
    assert!(s.cancel("C"));
    assert!(!s.cancel("C"));
    advance(5_000).await;
    assert!(!log.lock().expect("log").iter().any(|l| l.starts_with("C:")));

    let failing = Arc::new(Mutex::new(Vec::new()));
    let f = Debouncer::new(3_000, 15_000, counting_run(failing.clone(), 0, true));
    // A failing run is logged ("[sticky] re-send in X failed: boom") and never wedges the channel.
    f.touch("X", 1);
    advance(3_100).await;
    assert_eq!(failing.lock().expect("log").len(), 1);
    assert_eq!(f.size(), 0);
    f.touch("X", 2);
    advance(3_100).await;
    assert_eq!(failing.lock().expect("log").len(), 2);
    f.touch("Y", 1);
    f.clear();
    assert_eq!(f.size(), 0);
}

// ── Rendering and texts (checked against the JS) ──────────────────────────────

fn ctx() -> bot_util::placeholder_parser::RenderContext {
    use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild};
    bot_util::placeholder_parser::RenderContext {
        guild: Some(PlaceholderGuild { id: "900000000000000002".into(), name: Some("Sticky **Guild**".into()), member_count: Some(5.0), ..Default::default() }),
        channel: Some(PlaceholderChannel { id: "800000000000000002".into(), name: Some("general".into()), kind: Some(0) }),
        ..Default::default()
    }
}

fn strip(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(es) = v.get_mut("embeds").and_then(Value::as_array_mut) {
        for e in es {
            if let Some(o) = e.as_object_mut() {
                o.remove("timestamp");
                o.remove("type");
            }
        }
    }
    v
}

#[test]
fn embed_part_and_render_sticky_agree_with_the_js() {
    let g = golden();
    for case in g["embedPart"].as_array().into_iter().flatten() {
        assert_eq!(svc::embed_part(&case[0]).unwrap_or(Value::Null), case[1], "{case}");
    }
    for case in g["renderSticky"].as_array().into_iter().flatten() {
        let row = &case[0];
        let settings = (!case[1].is_null()).then(|| bot_db::GuildSettings { embed_color: case[1]["embedColor"].as_i64().map(|c| c as i32), ..Default::default() });
        let r = svc::render_sticky(row["content"].as_str(), Some(&row["embed"]).filter(|e| !e.is_null()), &ctx(), settings.as_ref());
        let js = &case[2];
        match r {
            None => assert!(js.is_null(), "{case}"),
            Some(r) => {
                let ours = r.to_json();
                assert_eq!(ours.get("content").cloned().unwrap_or(Value::Null), js.get("content").cloned().unwrap_or(Value::Null), "{case}");
                assert_eq!(strip(&ours)["embeds"], strip(js)["embeds"], "{case}");
                let urls = |v: &Value| v["components"].to_string();
                assert_eq!(urls(&ours), urls(js), "{case}");
                assert_eq!(js["allowedMentions"], json!({ "parse": [] }));
            }
        }
    }
    assert_eq!(json!([svc::sticky_needs(true), svc::sticky_needs(false)]), g["stickyNeeds"]);
}

fn status_of(s: &str) -> ResendStatus {
    match s {
        "sent" => ResendStatus::Sent,
        "current" => ResendStatus::Current,
        "off" => ResendStatus::Off,
        "perms" => ResendStatus::Perms,
        "gone" => ResendStatus::Gone,
        _ => ResendStatus::Failed,
    }
}

#[test]
fn texts_and_the_form_agree_with_the_js() {
    let g = golden();
    for case in g["setResult"].as_array().into_iter().flatten() {
        let r = SetResult {
            created: case[0]["created"].as_bool().unwrap_or(false),
            has_embed: case[0]["hasEmbed"].as_bool().unwrap_or(false),
            status: status_of(case[0]["status"].as_str().unwrap_or_default()),
        };
        let e = serde_json::to_value(plugin::set_result_embed("800000000000000002", &r, case[1].as_str().unwrap_or_default())).unwrap_or(Value::Null);
        assert_eq!(e["title"], case[2]["title"], "{case}");
        assert_eq!(e["description"], case[2]["description"], "{case}");
        assert_eq!(e["color"], case[2]["color"], "{case}");
    }
    for case in g["variableNotes"].as_array().into_iter().flatten() {
        let p = &case[0];
        let embed = p.get("embed").filter(|e| !e.is_null());
        assert_eq!(plugin::variable_notes(p["content"].as_str(), embed, &ctx()), case[1].as_str().unwrap_or_default(), "{case}");
    }
    let modals = [
        plugin::build_sticky_modal("800000000000000002", Some("Current text")),
        plugin::build_sticky_modal("800000000000000003", None),
        plugin::build_sticky_modal("800000000000000004", Some(&"x".repeat(2100))),
    ];
    for (m, js) in modals.iter().zip(g["stickyModal"].as_array().into_iter().flatten()) {
        assert_eq!(serde_json::to_value(m).unwrap_or(Value::Null), *js);
    }
    assert!(plugin::has_user_token("{USER.Mention}"));
    assert!(!plugin::has_user_token("{user.}"));
    assert_eq!(feat_captcha::commands::sticky::preview(Some("  a `b`\n\n c\\ ")), "a b c");
    assert_eq!(feat_captcha::commands::sticky::preview(Some(&"y".repeat(60))).chars().count(), 50);
}

#[test]
fn plugin_and_command_declarations() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["captcha", "sticky"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "sticky");
    let at = |id: &str| bot_config::hook_index("messageCreate", id);
    assert_eq!(at("sticky"), Some(13), "last of the 14 messageCreate hooks");
    assert!(bot_config::hook_index("ownMessage", "sticky").is_some());
}

#[tokio::test]
async fn the_filters_follow_the_memo() {
    let g = "955030000000000999";
    let t = TestApp::new().with_guilds([g]);
    assert!(plugin::may_stick(&t, g, "1"), "no memo: the handler loads");
    assert!(plugin::may_have_sticky(&t, g, "1"));
    t.settings().update_settings(g, SettingsPatch::new().set("stickyEnabled", false)).await.expect("memory");
    assert!(!plugin::may_stick(&t, g, "1"), "module off");
    let mut msg = bot_core::testing::fake_message("955030000000000001", g, "955030000000000002", TEST_BOT_ID, "x");
    msg.flags = Some(serenity::all::MessageFlags::EPHEMERAL);
    t.settings().update_settings(g, SettingsPatch::new().set("stickyEnabled", true)).await.expect("memory");
    assert!(!plugin::may_stick_own(&t, g, &msg), "ephemeral answers never move the sticky");
    msg.flags = None;
    assert!(plugin::may_stick_own(&t, g, &msg));
}

// ── Database ──────────────────────────────────────────────────────────────────

const ROLE_BOT: &str = "955030000000000100";
const ROLE_ADMIN: &str = "955030000000000101";
const ADMIN: &str = "955030111111100009";
/// A channel id of this test guild (StickyMessage.channelId is unique across guilds, and tests run in parallel).
fn chan(g: &str, n: u32) -> &'static str {
    Box::leak(format!("9550310000000{}{n:03}", &g[16..]).into_boxed_str())
}

fn world(g: &str) -> FakeGuild {
    let (ch, ch2, ch_locked) = (chan(g, 0), chan(g, 1), chan(g, 2));
    FakeGuild::new(g, "955030111111100099")
        .everyone(P::VIEW_CHANNEL)
        .role(ROLE_BOT, 10, P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS)
        .role(ROLE_ADMIN, 5, P::MANAGE_GUILD)
        .member(TEST_BOT_ID, &[ROLE_BOT])
        .member(ADMIN, &[ROLE_ADMIN])
        .channel(ch, 0, "general", &[])
        .channel(ch2, 0, "other", &[])
        .channel(ch_locked, 0, "locked", &[(ROLE_BOT, false, P::empty(), P::SEND_MESSAGES)])
}

struct Db {
    t: DbTestApp,
    rest: FakeRest,
    g: &'static str,
}

impl Db {
    async fn new(g: &'static str, edit: impl FnOnce(FakeGuild) -> FakeGuild) -> Option<Self> {
        let url = database_url()?;
        let t = match redis_url() {
            Some(r) => DbTestApp::with_redis(&url, &r).await,
            None => DbTestApp::new(&url).await,
        }
        .with_guilds([g]);
        t.reset_guilds(&[g]).await;
        t.settings().invalidate_settings(g).await;
        let rest = FakeRest::new();
        t.set_discord(rest.discord(&[edit(world(g)).json()]));
        t.install(registry());
        t.settings().update_settings(g, SettingsPatch::new().set("stickyEnabled", true)).await.expect("settings");
        Some(Self { t, rest, g })
    }

    fn args(&self, channel: &str, content: Option<&str>) -> SetArgs {
        SetArgs { guild_id: self.g.into(), channel_id: channel.into(), user_id: ADMIN.into(), content: content.map(str::to_string), embed: None, keep_embed: false }
    }

    async fn row(&self, channel: &str) -> Option<bot_db::StickyMessage> {
        svc::get_sticky(self.t.db(), self.g, channel).await.expect("row")
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

#[tokio::test]
async fn db_set_posts_at_once_and_a_replacement_deletes_the_previous_message_first() {
    let Some(d) = Db::new("955030000000000001", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    let r = svc::set_sticky(&d.t, d.args(ch, Some("Read the rules in {server}"))).await.expect("set");
    assert_eq!(r, SetResult { created: true, has_embed: false, status: ResendStatus::Sent });
    let posts = d.rest.calls_to("POST", "/channels/*/messages");
    assert_eq!(posts.len(), 1);
    assert!(posts[0].body["content"].as_str().unwrap_or_default().starts_with("Read the rules in "));
    assert_eq!(posts[0].body["allowed_mentions"]["parse"], json!([]), "never pings");
    let first = d.row(ch).await.expect("row").last_message_id.expect("saved");

    d.rest.clear_calls();
    let embed = svc::embed_part(&json!({ "embed": { "title": "Rules" }, "buttons": [] }));
    let r = svc::set_sticky(&d.t, SetArgs { embed, ..d.args(ch, Some("New")) }).await.expect("set");
    assert_eq!(r, SetResult { created: false, has_embed: true, status: ResendStatus::Sent });
    let calls = d.rest.calls();
    let order: Vec<String> = calls.iter().map(|c| format!("{} {}", c.method, c.path)).collect();
    assert_eq!(order, vec![format!("DELETE /channels/{ch}/messages/{first}"), format!("POST /channels/{ch}/messages")]);
    // Our own delete costs no query afterwards.
    assert!(svc::consume_self_deleted(&d.t, &first));

    // The text form keeps the embed; a plain set replaces both.
    svc::set_sticky(&d.t, SetArgs { keep_embed: true, ..d.args(ch, Some("Edited")) }).await.expect("set");
    let row = d.row(ch).await.expect("row");
    assert_eq!((row.content.as_deref(), row.embed.as_ref().map(|e| e["embed"]["title"].clone())), (Some("Edited"), Some(json!("Rules"))));
    svc::set_sticky(&d.t, d.args(ch, Some("Plain"))).await.expect("set");
    let null: Option<bool> = bot_db::sqlx::query_scalar(r#"SELECT "embed" IS NULL FROM "StickyMessage" WHERE "channelId" = $1"#).bind(ch).fetch_one(d.t.db()).await.expect("q");
    assert_eq!(null, Some(true), "SQL NULL, the JS Prisma.DbNull");
    d.done().await;
}

#[tokio::test]
async fn db_set_refuses_empty_too_long_unusable_and_over_the_cap() {
    let Some(d) = Db::new("955030000000000002", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    let err = |e: anyhow::Error| e.downcast::<svc::StickyError>().expect("a sticky error");
    let e = err(svc::set_sticky(&d.t, d.args(ch, Some("   "))).await.expect_err("empty"));
    assert_eq!((e.code, e.title.as_str()), ("INVALID", "Nothing To Stick"));
    assert_eq!(e.message, "The message is empty. Add some text or an embed.");
    let long = "é".repeat(2001);
    let e = err(svc::set_sticky(&d.t, d.args(ch, Some(&long))).await.expect_err("long"));
    assert_eq!(e.message, "The text is 2001 characters; the limit is 2000.");
    let e = err(svc::set_sticky(&d.t, d.args(ch_locked, Some("x"))).await.expect_err("perms"));
    assert_eq!(e.title, "Cannot Use That Channel");
    assert_eq!(e.message, format!("I need **Send Messages** in <#{ch_locked}>. Fix my permissions there, or pick another channel."));
    d.t.settings().ensure_guild_settings(d.g).await.expect("ensure");
    for i in 0..25 {
        bot_db::sqlx::query(r#"INSERT INTO "StickyMessage" ("id","guildId","channelId","content","createdById","updatedAt") VALUES ($1,$2,$3,'x','A',(now() AT TIME ZONE 'UTC'))"#)
            .bind(bot_db::new_cuid())
            .bind(d.g)
            .bind(format!("9550300000001{i:05}"))
            .execute(d.t.db())
            .await
            .expect("insert");
    }
    let e = err(svc::set_sticky(&d.t, d.args(ch, Some("x"))).await.expect_err("cap"));
    assert_eq!(e.code, "LIMIT");
    assert_eq!(e.message, "A server can have at most **25** sticky messages. Remove one with `/sticky remove` first.");
    d.done().await;
}

#[tokio::test]
async fn db_resend_paths_paused_off_current_gone_and_a_failing_post() {
    let Some(d) = Db::new("955030000000000003", |w| w.edit_channel(chan("955030000000000003", 1), |c| c["last_message_id"] = json!("955030000000009999"))).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    svc::set_sticky(&d.t, d.args(ch, Some("x"))).await.expect("set");
    // Already the last message: skipped unless forced.
    svc::set_sticky(&d.t, d.args(ch2, Some("y"))).await.expect("set");
    bot_db::sqlx::query(r#"UPDATE "StickyMessage" SET "lastMessageId" = '955030000000009999' WHERE "channelId" = $1"#).bind(ch2).execute(d.t.db()).await.expect("q");
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch2, false).await.expect("resend"), ResendStatus::Current);
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch2, true).await.expect("resend"), ResendStatus::Sent);
    assert_eq!(svc::resend_sticky(&d.t, d.g, "955030000000000299", false).await.expect("resend"), ResendStatus::Missing);
    // Paused: the message is deleted and nothing is posted.
    let r = svc::set_sticky_enabled(&d.t, d.g, ch, false).await.expect("pause");
    assert_eq!((r.found, r.changed, r.status), (true, true, Some(ResendStatus::Paused)));
    assert_eq!(d.row(ch).await.expect("row").last_message_id, None);
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, false).await.expect("resend"), ResendStatus::Paused);
    let r = svc::set_sticky_enabled(&d.t, d.g, ch, true).await.expect("resume");
    assert_eq!((r.changed, r.status), (true, Some(ResendStatus::Sent)));
    // Module off.
    d.t.settings().update_settings(d.g, SettingsPatch::new().set("stickyEnabled", false)).await.expect("s");
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, true).await.expect("resend"), ResendStatus::Off);
    d.t.settings().update_settings(d.g, SettingsPatch::new().set("stickyEnabled", true)).await.expect("s");
    // A post Discord refuses forgets the old message id.
    let last = d.row(ch).await.expect("row").last_message_id;
    assert!(last.is_some());
    d.rest.fail("POST", &format!("/channels/{ch}/messages"), 403, 50013, "Missing Permissions");
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, true).await.expect("resend"), ResendStatus::Perms);
    assert_eq!(d.row(ch).await.expect("row").last_message_id, None);
    d.rest.fail("POST", &format!("/channels/{ch}/messages"), 404, 10003, "Unknown Channel");
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, true).await.expect("resend"), ResendStatus::Gone);
    d.rest.fail("POST", &format!("/channels/{ch}/messages"), 500, 0, "Internal");
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, true).await.expect("resend"), ResendStatus::Failed);
    d.done().await;
}

#[tokio::test]
async fn db_channel_cache_hooks_and_cleanup() {
    let Some(d) = Db::new("955030000000000004", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    assert_eq!(svc::get_channels(&d.t, d.g).await.map(|c| c.len()), Some(0), "an empty answer is remembered");
    assert!(svc::peek_channels(&d.t, d.g).is_some());
    svc::set_sticky(&d.t, d.args(ch, Some("x"))).await.expect("set");
    assert!(svc::peek_channels(&d.t, d.g).is_none(), "a change invalidates");
    let chans = svc::get_channels(&d.t, d.g).await.expect("loaded");
    assert_eq!(chans.get(ch), Some(&true));
    assert!(!plugin::may_stick(&d.t, d.g, ch2), "a channel without a sticky costs nothing");
    assert!(plugin::may_stick(&d.t, d.g, ch));

    // A member message schedules a re-send (the hook returns at once).
    let gid = serenity::all::GuildId::new(d.g.parse().unwrap_or(1));
    let cid = serenity::all::ChannelId::new(ch.parse().unwrap_or(1));
    let msg = bot_core::testing::fake_message("955030000000000501", d.g, ch, ADMIN, "hello");
    d.t.run_hooks(MessageCreate { guild_id: gid, message: msg, member: None, channel_id: cid, channel: None }).await;
    assert!(svc::has_pending(&d.t, ch));

    // Someone else deletes the sticky message: its id is forgotten. Our own deletes are skipped.
    let last = d.row(ch).await.expect("row").last_message_id.expect("posted");
    let del = |id: &str| MessageDelete {
        guild_id: gid,
        message_id: serenity::all::MessageId::new(id.parse().unwrap_or(1)),
        channel_id: cid,
        message: None,
        partial: true,
        channel: None,
    };
    d.t.run_hooks(del("955030000000000777")).await;
    assert_eq!(d.row(ch).await.expect("row").last_message_id.as_deref(), Some(last.as_str()));
    d.t.run_hooks(del(&last)).await;
    assert_eq!(d.row(ch).await.expect("row").last_message_id, None);

    // The channel is deleted: the row, the pending re-send and the memo go.
    let channel = bot_core::interaction_helpers::cached_channel(&d.t, gid, cid).expect("cached");
    d.t.run_hooks(ChannelDelete { guild_id: gid, channel }).await;
    assert!(d.row(ch).await.is_none());
    assert!(!svc::has_pending(&d.t, ch));
    assert!(svc::peek_channels(&d.t, d.g).is_none());
    assert_eq!(svc::drop_channel(&d.t, d.g, ch).await.expect("drop"), 0);
    d.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_a_noted_message_reposts_after_the_quiet_period() {
    let Some(d) = Db::new("955030000000000005", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    svc::set_sticky(&d.t, d.args(ch, Some("x"))).await.expect("set");
    let first = d.row(ch).await.expect("row").last_message_id.expect("posted");
    d.rest.clear_calls();
    svc::note_message(&d.t, d.g, ch);
    for _ in 0..80 {
        if !svc::has_pending(&d.t, ch) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!svc::has_pending(&d.t, ch));
    let calls: Vec<String> = d.rest.calls().iter().map(|c| format!("{} {}", c.method, c.path)).collect();
    assert_eq!(calls, vec![format!("DELETE /channels/{ch}/messages/{first}"), format!("POST /channels/{ch}/messages")]);
    assert_ne!(d.row(ch).await.expect("row").last_message_id, Some(first));
    d.done().await;
}

fn admin_slash(g: &str, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("sticky").guild(g).user(ADMIN).roles(&[ROLE_ADMIN]).permissions(P::MANAGE_GUILD).subcommand(sub)
}

#[tokio::test]
async fn db_the_sticky_command_and_the_form() {
    let Some(d) = Db::new("955030000000000006", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    use bot_core::command::option_type as ot;
    let route = |f: FakeInteraction| {
        d.t.responses.clear();
        bot_core::router::route_interaction(&d.t, f.build())
    };
    let last = || d.t.responses.calls().last().cloned().expect("answer");

    // No text: the form first (no defer), prefilled.
    route(admin_slash(d.g, "set").option("channel", ot::CHANNEL, json!(ch))).await;
    assert_eq!(d.t.responses.names(), vec!["showModal"]);
    assert_eq!(last().body["custom_id"], format!("sticky-modal:{ch}"));
    route(admin_slash(d.g, "set").option("channel", ot::CHANNEL, json!(ch_locked))).await;
    assert_eq!(last().embed_title(), Some("❌  Cannot Use That Channel"));

    route(admin_slash(d.g, "set").option("channel", ot::CHANNEL, json!(ch)).option("text", ot::STRING, json!("Line one\\nLine {user}"))).await;
    assert_eq!(d.t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(last().embed_title(), Some("✅  Sticky Message Set"));
    assert_eq!(
        last().embed_description(),
        Some(format!("The sticky message in <#{ch}> is set.\nIt is posted now and moves back down once chat has been quiet for a few seconds.\n⚠️ A sticky message has no member, so user variables like `{{user}}` are posted as written. Server and channel variables work.").as_str())
    );
    assert_eq!(d.row(ch).await.expect("row").content.as_deref(), Some("Line one\nLine {user}"));
    route(admin_slash(d.g, "set").option("channel", ot::CHANNEL, json!(ch)).option("embed", ot::STRING, json!("nope"))).await;
    assert_eq!(last().embed_description(), Some("There is no saved embed named `nope`. Create it with `/embed create`."));

    // The form: Admin only; edits the text and keeps the embed.
    let form = |user: &str, roles: &[&str], text: &str| {
        FakeInteraction::modal(&format!("sticky-modal:{ch}"), &[("sticky-content", text)]).guild(d.g).user(user).roles(roles).build()
    };
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, form("955030111111100001", &[], "x")).await;
    assert_eq!(last().embed_title(), Some("❌  Insufficient Permissions"));
    assert!(last().ephemeral());
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, form(ADMIN, &[ROLE_ADMIN], "From the form")).await;
    assert_eq!(last().embed_title(), Some("✅  Sticky Message Updated"));
    assert_eq!(d.row(ch).await.expect("row").content.as_deref(), Some("From the form"));

    route(admin_slash(d.g, "list")).await;
    assert_eq!(last().embed_title(), Some("📌 Sticky Messages"));
    assert_eq!(last().embed_description(), Some(format!("✅ <#{ch}> `From the form`").as_str()));
    assert_eq!(last().body["embeds"][0]["footer"]["text"], "1 of 25 used");

    route(admin_slash(d.g, "pause").option("channel", ot::CHANNEL, json!(ch))).await;
    assert_eq!(last().embed_description(), Some(format!("<#{ch}> stays quiet until you run `/sticky resume`. The current sticky message was deleted.").as_str()));
    route(admin_slash(d.g, "pause").option("channel", ot::CHANNEL, json!(ch))).await;
    assert!(last().embed_description().unwrap_or_default().ends_with("It was already paused."));
    route(admin_slash(d.g, "resume").option("channel", ot::CHANNEL, json!(ch))).await;
    assert_eq!(last().embed_description(), Some(format!("<#{ch}> is sticky again. It is posted again and will move back down once chat has been quiet.").as_str()));
    route(admin_slash(d.g, "remove").option("channel", ot::CHANNEL, json!(ch2))).await;
    assert_eq!(last().embed_title(), Some("❌  No Sticky Message"));
    route(admin_slash(d.g, "remove").option("channel", ot::CHANNEL, json!(ch))).await;
    assert_eq!(last().embed_title(), Some("✅  Sticky Message Removed"));

    d.t.settings().update_settings(d.g, SettingsPatch::new().set("stickyEnabled", false)).await.expect("s");
    route(admin_slash(d.g, "list")).await;
    assert_eq!(last().embed_description(), Some("No sticky messages yet. Create one with `/sticky set`.\n\n⚠️ The **Sticky Messages** module is off. Turn it on with `/module enable sticky`."));

    let ac = FakeInteraction::autocomplete("sticky").guild(d.g).user(ADMIN).roles(&[ROLE_ADMIN]).permissions(P::MANAGE_GUILD).subcommand("set").focused("embed", "");
    route(ac).await;
    assert_eq!(d.t.responses.names(), vec!["respond"]);
    d.done().await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_read_each_others_stickies() {
    let Some(d) = Db::new("955030000000000007", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = (ch2, ch_locked);
    // JS setSticky writes; Rust reads the row, the list and the channel map.
    let js = run_node(
        "rust/fixtures/gen/feat-captcha-js.mjs",
        &["sticky-set", d.g, ch2, r#"{"content":"From JS {server}","embed":{"embed":{"title":"T"},"buttons":[]}}"#],
    );
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["result"], json!({ "created": true, "hasEmbed": true, "status": "sent" }));
    let row = d.row(ch2).await.expect("JS row");
    assert_eq!(serde_json::to_value(&row).unwrap_or(Value::Null), js["row"], "the row reads the same");
    assert_eq!(svc::get_channels(&d.t, d.g).await.and_then(|c| c.get(ch2).copied()), Some(true));
    let r = svc::render_sticky(row.content.as_deref(), row.embed.as_ref(), &Default::default(), None).expect("renders");
    assert_eq!(r.embeds.len(), 1);

    // Rust writes; the JS service reads the same rows.
    svc::set_sticky(&d.t, d.args(ch, Some("From Rust"))).await.expect("set");
    svc::set_sticky_enabled(&d.t, d.g, ch, false).await.expect("pause");
    let js = run_node("rust/fixtures/gen/feat-captcha-js.mjs", &["sticky-get", d.g, ch]);
    assert!(js.get("error").is_none(), "{js}");
    let ours = serde_json::to_value(d.row(ch).await.expect("row")).unwrap_or(Value::Null);
    assert_eq!(js["row"], ours);
    assert_eq!(js["count"], 2);
    assert_eq!(js["channels"], json!({ ch2: true, ch: false }));
    let list: Vec<Value> = svc::list_stickies(d.t.db(), d.g).await.expect("list").iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect();
    assert_eq!(js["list"], Value::Array(list), "same rows, same order");
    d.done().await;
}

#[tokio::test]
async fn db_delete_failures_still_post_a_saved_embed_shows_and_a_row_removed_in_flight_is_cleaned_up() {
    let Some(d) = Db::new("955030000000000008", |g| g).await else { return };
    let (ch, ch2, ch_locked) = (chan(d.g, 0), chan(d.g, 1), chan(d.g, 2));
    let _ = ch_locked;
    use bot_core::command::option_type as ot;
    svc::set_sticky(&d.t, d.args(ch, Some("x"))).await.expect("set");
    // A delete that fails for another reason than "already gone" does not stop the post.
    d.rest.fail("DELETE", "/channels/*/messages/*", 403, 50013, "Missing Permissions");
    d.rest.clear_calls();
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, true).await.expect("resend"), ResendStatus::Sent);
    assert_eq!(d.rest.calls_to("POST", "/channels/*/messages").len(), 1);
    d.rest.clear_scripts();

    // /sticky set embed:<saved embed> posts the saved embed's embed.
    feat_embeds::embed_service::create_saved_embed(&d.t, d.g, "rules", &json!({ "content": "From the embed", "embed": { "title": "Rules", "description": "Be nice" }, "buttons": [] }), ADMIN)
        .await
        .expect("saved embed");
    d.t.responses.clear();
    let f = FakeInteraction::slash("sticky").guild(d.g).user(ADMIN).roles(&[ROLE_ADMIN]).permissions(P::MANAGE_GUILD).subcommand("set");
    bot_core::router::route_interaction(&d.t, f.option("channel", ot::CHANNEL, json!(ch2)).option("embed", ot::STRING, json!("rules")).build()).await;
    let last = d.t.responses.calls().last().cloned().expect("answer");
    assert_eq!(last.embed_description().map(|t| t.starts_with(&format!("The sticky message in <#{ch2}> is set (with an embed)."))), Some(true), "{last:?}");
    let row = d.row(ch2).await.expect("row");
    assert_eq!(row.content.as_deref(), Some("From the embed"), "the saved embed's text when no text is given");
    assert_eq!(row.embed.as_ref().map(|e| e["embed"]["title"].clone()), Some(json!("Rules")));

    // Removed while the message was on its way: the new message is deleted again.
    let Some(url) = database_url() else { return };
    let channel = ch.to_string();
    d.rest.on_with("POST", &format!("/channels/{ch}/messages"), move |_| {
        let (url, channel2) = (url.clone(), channel.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(async {
                let pool = bot_core::testing::connect_test_pool(&url).await;
                bot_db::sqlx::query(r#"DELETE FROM "StickyMessage" WHERE "channelId" = $1"#).bind(&channel2).execute(&pool).await.expect("delete");
            });
        })
        .join()
        .ok();
        let mut m = serde_json::to_value(bot_core::testing::fake_message("955030000000004444", "955030000000000008", &channel, TEST_BOT_ID, "x")).unwrap_or(Value::Null);
        m["channel_id"] = json!(channel);
        (200, m)
    });
    d.rest.clear_calls();
    assert_eq!(svc::resend_sticky(&d.t, d.g, ch, true).await.expect("resend"), ResendStatus::Missing);
    let deletes = d.rest.calls_to("DELETE", "/channels/*/messages/*");
    assert!(deletes.iter().any(|c| c.path == format!("/channels/{ch}/messages/955030000000004444")), "{deletes:?}");
    d.done().await;
}

/// `guild.channels.cache.get(option.id)`: discord.js put the resolved option channel into the cache, so an uncached
/// channel picked in `/sticky set` is found (fetched over REST here) instead of "Channel not found.".
#[tokio::test]
async fn db_sticky_set_on_a_channel_missing_from_the_cache_opens_the_form() {
    let Some(d) = Db::new("955030000000000009", |g| g).await else { return };
    let late = "955030000000000990";
    d.rest.on("GET", &format!("/channels/{late}"), 200, json!({ "id": late, "type": 0, "guild_id": d.g, "name": "late", "position": 9, "permission_overwrites": [], "nsfw": false, "parent_id": null }));
    use bot_core::command::option_type as ot;
    d.t.responses.clear();
    bot_core::router::route_interaction(&d.t, admin_slash(d.g, "set").option("channel", ot::CHANNEL, json!(late)).build()).await;
    assert_eq!(d.t.responses.names(), vec!["showModal"], "{:?}", d.t.responses.calls());
    d.done().await;
}
