//! The gateway events (`src/events/*.js`) offline: the pre-filters, the payload fields and the synthetic points
//! (ClientReady per shard, guildCreate / guildAvailable, guildDelete cleanup, userUpdate, emoji diffs), driven
//! through `gateway::process` with an offline serenity cache.

use std::sync::{Arc, Mutex};

use bot_core::gateway::{self, Before, Envelope, GatewayState, MessageRoute, route_message};
use bot_core::prelude::*;
use bot_core::testing::{FakeGuild, TEST_BOT_ID, TestApp, fake_message};
use serde_json::{Value, json};
use serenity::all::{Event, MessageType, Permissions};

const G: &str = "900000000000000200";
const CH: &str = "700000000000000200";

type Log = Arc<Mutex<Vec<String>>>;

fn app() -> (TestApp, Log) {
    let guild = FakeGuild::new(G, "600000000000000001")
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .member(TEST_BOT_ID, &[])
        .member("600000000000000001", &[])
        .member("600000000000000002", &[])
        .channel(CH, 0, "general", &[]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild]);
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    record(&t, &log);
    (t, log)
}

macro_rules! rec {
    ($t:expr, $log:expr, $ty:ty, $fmt:expr) => {{
        let l = $log.clone();
        let f: fn(&$ty) -> String = $fmt;
        let _ = $t.hooks().register_hook(
            Hook::<$ty>::new("logging", move |ctx: Arc<HookCtx<$ty>>| {
                let l = l.clone();
                async move {
                    l.lock().unwrap().push(f(&ctx.data));
                    Ok(())
                }
            })
            .ungated(),
            "logging",
            None,
        );
    }};
}

fn record(t: &TestApp, log: &Log) {
    rec!(t, log, MessageCreate, |d| format!("messageCreate {} member={}", d.message.content, d.member.is_some()));
    rec!(t, log, BotMessage, |d| format!("botMessage {} edited={} wasLoading={:?}", d.message.content, d.edited, d.was_loading));
    rec!(t, log, OwnMessage, |d| format!("ownMessage {}", d.message.content));
    rec!(t, log, DirectMessage, |d| format!("directMessage {}", d.message.content));
    rec!(t, log, MessageUpdate, |d| format!("messageUpdate {} oldKnown={}", d.message.content, d.old_known));
    rec!(t, log, MessageDelete, |d| format!("messageDelete partial={} content={:?}", d.partial, d.message.as_ref().map(|m| m.content.clone())));
    rec!(t, log, GuildReady, |d| format!("guildReady {} {}", d.guild_id, d.reason.as_str()));
    rec!(t, log, Ready, |_| "ready".to_string());
    rec!(t, log, GuildDelete, |d| format!("guildDelete {}", d.guild_id));
    rec!(t, log, UserUpdate, |d| format!("userUpdate {} {} -> {}", d.guild_id, d.old_user.name, d.user.name));
    rec!(t, log, GuildMemberUpdate, |d| format!("guildMemberUpdate oldKnown={}", d.old_known));
    rec!(t, log, GuildMemberAdd, |d| format!("guildMemberAdd {}", d.user.id));
    rec!(t, log, EmojiCreate, |d| format!("emojiCreate {}", d.emoji.name));
    rec!(t, log, EmojiUpdate, |d| format!("emojiUpdate {} -> {}", d.old_emoji.name, d.emoji.name));
    rec!(t, log, EmojiDelete, |d| format!("emojiDelete {}", d.emoji.name));
    rec!(t, log, MessageReactionAdd, |d| format!("reactionAdd {} {}", d.user.id, d.emoji_key));
    rec!(t, log, VoiceStateUpdate, |d| format!("voice {}", d.change.as_str()));
}

fn log_of(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
}

async fn feed(t: &TestApp, gw: &Arc<GatewayState>, event: Event, before: Before) {
    gateway::process(t, gw, Envelope { shard: 0, event, before }).await;
    settle().await;
}

fn event(kind: &str, data: Value) -> Event {
    let frame = json!({ "op": 0, "t": kind, "s": 1, "d": data });
    match serde_json::from_value::<serenity::model::event::GatewayEvent>(frame) {
        Ok(serenity::model::event::GatewayEvent::Dispatch(_, e)) => e,
        Ok(other) => panic!("{kind}: not a dispatch: {other:?}"),
        Err(e) => panic!("{kind}: {e}"),
    }
}

fn msg_json(id: &str, author: &str, content: &str, extra: Value) -> Value {
    let mut m = serde_json::to_value(fake_message(id, G, CH, author, content)).expect("message json");
    if let (Some(o), Some(x)) = (m.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    m
}

#[test]
fn message_create_routes_like_the_js() {
    use MessageRoute::*;
    let k = MessageType::Regular;
    assert_eq!(route_message(false, false, false, true, k), MessageCreate);
    assert_eq!(route_message(false, false, false, true, MessageType::InlineReply), MessageCreate);
    assert_eq!(route_message(false, false, false, true, MessageType::MemberJoin), Ignored, "system messages never enter the pipeline");
    assert_eq!(route_message(true, false, false, true, MessageType::ChatInputCommand), BotMessage, "bots go to botMessage before the type filter");
    assert_eq!(route_message(false, true, false, true, k), BotMessage, "webhooks too");
    assert_eq!(route_message(true, false, true, true, k), OwnMessage);
    assert_eq!(route_message(true, false, false, false, k), Ignored);
    assert_eq!(route_message(false, false, false, false, k), DirectMessage);
    assert_eq!(route_message(false, false, false, false, MessageType::MemberJoin), Ignored);
    assert!(gateway::recently_edited(Some(1_000), 10_000));
    assert!(!gateway::recently_edited(Some(1_000), 20_000));
    assert!(!gateway::recently_edited(None, 0));
}

#[tokio::test]
async fn message_events_reach_the_right_points() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    feed(&t, &gw, event("MESSAGE_CREATE", msg_json("1", "600000000000000001", "hello", json!({}))), Before::default()).await;
    let bot = json!({ "id": "600000000000000009", "username": "otherbot", "discriminator": "0", "avatar": null, "bot": true });
    feed(&t, &gw, event("MESSAGE_CREATE", msg_json("2", "600000000000000009", "beep", json!({ "author": bot, "type": 20 }))), Before::default()).await;
    let me = json!({ "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true });
    feed(&t, &gw, event("MESSAGE_CREATE", msg_json("3", TEST_BOT_ID, "mine", json!({ "author": me }))), Before::default()).await;
    feed(&t, &gw, event("MESSAGE_CREATE", msg_json("4", "600000000000000001", "joined", json!({ "type": 7 }))), Before::default()).await;
    let mut dm = msg_json("5", "600000000000000001", "dm text", json!({}));
    if let Some(o) = dm.as_object_mut() {
        o.remove("guild_id");
        o.remove("member");
    }
    feed(&t, &gw, event("MESSAGE_CREATE", dm), Before::default()).await;
    assert_eq!(
        log_of(&log),
        vec!["messageCreate hello member=true", "botMessage beep edited=false wasLoading=None", "ownMessage mine", "directMessage dm text"]
    );
}

#[tokio::test]
async fn message_update_and_delete_use_the_snapshot() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let old = fake_message("10", G, CH, "600000000000000001", "before");
    // Not cached anywhere and not freshly edited: no edit (an embed unfurl).
    feed(&t, &gw, event("MESSAGE_UPDATE", json!({ "id": "10", "channel_id": CH, "guild_id": G, "content": "after" })), Before::default()).await;
    assert!(log_of(&log).is_empty());
    // The old message was cached but the new one is not, and no REST is reachable: dropped like a failed fetch.
    feed(&t, &gw, event("MESSAGE_UPDATE", json!({ "id": "10", "channel_id": CH, "guild_id": G, "content": "after" })), Before { message: Some(old.clone()), ..Default::default() }).await;
    assert!(log_of(&log).is_empty());
    // A delete of a cached message carries it; an uncached one is partial.
    feed(&t, &gw, event("MESSAGE_DELETE", json!({ "id": "10", "channel_id": CH, "guild_id": G })), Before { message: Some(old), ..Default::default() }).await;
    feed(&t, &gw, event("MESSAGE_DELETE", json!({ "id": "11", "channel_id": CH, "guild_id": G })), Before::default()).await;
    feed(&t, &gw, event("MESSAGE_DELETE", json!({ "id": "12", "channel_id": CH })), Before::default()).await;
    assert_eq!(log_of(&log), vec!["messageDelete partial=false content=Some(\"before\")", "messageDelete partial=true content=None"]);
}

#[tokio::test]
async fn startup_runs_ready_then_guild_ready_once_the_ready_guilds_arrived() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let ready = json!({
        "v": 10, "user": { "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true, "verified": true, "mfa_enabled": false, "flags": 0 },
        "guilds": [{ "id": G, "unavailable": true }], "session_id": "s", "resume_gateway_url": "wss://x.invalid", "shard": [0, 1],
        "application": { "id": "100000000000000001", "flags": 0 }
    });
    feed(&t, &gw, event("READY", ready), Before::default()).await;
    assert!(!gw.shard_ready(0), "waits for its READY guilds");
    let guild = FakeGuild::new(G, "600000000000000001").member(TEST_BOT_ID, &[]).json();
    feed(&t, &gw, event("GUILD_CREATE", guild.clone()), Before { guild_unavailable: true, guild_known: true, ..Default::default() }).await;
    assert!(gw.shard_ready(0));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(log_of(&log), vec!["ready".to_string(), format!("guildReady {G} startup")]);
    assert!(t.scheduler().is_started(), "the scheduler starts on ClientReady");
    t.scheduler().stop(100).await;

    // After startup: a guild that was unavailable comes back (guildAvailable: skipped inside its 10 minute cooldown).
    log.lock().unwrap().clear();
    feed(&t, &gw, event("GUILD_CREATE", guild.clone()), Before { guild_unavailable: true, guild_known: true, ..Default::default() }).await;
    assert!(log_of(&log).is_empty(), "startup work ran recently");
    // A new guild is a join.
    let other = "900000000000000201";
    t.set_guilds(bot_core::discord::StaticGuilds::available([G, other]));
    feed(&t, &gw, event("GUILD_CREATE", FakeGuild::new(other, "600000000000000001").json()), Before::default()).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(log_of(&log), vec![format!("guildReady {other} join")]);
    assert!(t.settings_store.row(other).is_some(), "the settings row is ensured before the guildReady hooks");
}

#[tokio::test]
async fn guild_delete_clears_the_cache_and_recurring_jobs_but_not_on_an_outage() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    for k in [format!("guild:{G}:settings"), format!("xp:cooldown:{G}:1"), format!("voice:start:{G}:1"), format!("tag:cd:{G}:x"), "guild:1:settings".to_string()] {
        t.cache().set(&k, &1, 300).await;
    }
    let run_at = chrono::Utc::now() + chrono::Duration::hours(1);
    t.scheduler().schedule_job(ScheduleJob::new(G, JobType::StatsRefresh, run_at).dedupe_key(format!("stats.refresh:{G}"))).await.expect("scheduled");
    t.scheduler().schedule_job(ScheduleJob::new(G, JobType::ModUnban, run_at).dedupe_key(format!("mod.unban:{G}:1"))).await.expect("scheduled");

    feed(&t, &gw, event("GUILD_DELETE", json!({ "id": G, "unavailable": true })), Before::default()).await;
    assert!(log_of(&log).is_empty(), "an outage is not a departure");
    assert_eq!(t.cache().get(&format!("guild:{G}:settings")).await, Some(json!(1)));

    feed(&t, &gw, event("GUILD_DELETE", json!({ "id": G })), Before { guild_known: true, ..Default::default() }).await;
    assert_eq!(log_of(&log), vec![format!("guildDelete {G}")]);
    for k in [format!("guild:{G}:settings"), format!("xp:cooldown:{G}:1"), format!("voice:start:{G}:1")] {
        assert_eq!(t.cache().get(&k).await, None, "{k} purged");
    }
    assert_eq!(t.cache().get(&format!("tag:cd:{G}:x")).await, Some(json!(1)), "only the three patterns, as the JS");
    assert_eq!(t.cache().get("guild:1:settings").await, Some(json!(1)));
    assert!(t.scheduler().get_job(&format!("stats.refresh:{G}")).await.is_none(), "recurring jobs are cancelled");
    assert!(t.scheduler().get_job(&format!("mod.unban:{G}:1")).await.is_some(), "one-off jobs are kept");
}

#[tokio::test]
async fn user_update_is_synthesised_from_member_updates() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let cached = t.discord().and_then(|d| d.cache.user(serenity::all::UserId::new(600000000000000002)).map(|u| u.clone())).expect("cached user");
    let update = json!({
        "guild_id": G, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "pending": false, "flags": 0,
        "user": { "id": "600000000000000002", "username": "renamed", "discriminator": "0", "avatar": null }
    });
    feed(&t, &gw, event("GUILD_MEMBER_UPDATE", update.clone()), Before { user: Some(cached.clone()), member: None, ..Default::default() }).await;
    let l = log_of(&log);
    assert!(l.contains(&"guildMemberUpdate oldKnown=false".to_string()), "{l:?}");
    assert!(l.contains(&format!("userUpdate {G} user600000000000000002 -> renamed")), "{l:?}");
    log.lock().unwrap().clear();
    let mut same = cached.clone();
    same.name = "renamed".into();
    feed(&t, &gw, event("GUILD_MEMBER_UPDATE", update), Before { user: Some(same), ..Default::default() }).await;
    assert!(!log_of(&log).iter().any(|l| l.starts_with("userUpdate")), "no change, no userUpdate");
}

#[tokio::test]
async fn emoji_updates_are_diffed_into_create_update_delete() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let emoji = |id: &str, name: &str| json!({ "id": id, "name": name, "roles": [], "require_colons": true, "managed": false, "animated": false, "available": true });
    let old: std::collections::HashMap<serenity::all::EmojiId, serenity::all::Emoji> = [emoji("1", "kept"), emoji("2", "renamed_old"), emoji("3", "gone")]
        .into_iter()
        .map(|e| serde_json::from_value::<serenity::all::Emoji>(e).expect("emoji"))
        .map(|e| (e.id, e))
        .collect();
    let ev = event("GUILD_EMOJIS_UPDATE", json!({ "guild_id": G, "emojis": [emoji("1", "kept"), emoji("2", "renamed_new"), emoji("4", "fresh")] }));
    feed(&t, &gw, ev, Before { emojis: Some(old), ..Default::default() }).await;
    assert_eq!(log_of(&log), vec!["emojiUpdate renamed_old -> renamed_new", "emojiCreate fresh", "emojiDelete gone"]);
}

#[tokio::test]
async fn reactions_skip_the_bot_and_carry_the_emoji_key() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let member = |id: &str, bot: bool| json!({ "user": { "id": id, "username": "u", "discriminator": "0", "avatar": null, "bot": bot }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 });
    let r = |user: &str, bot: bool, emoji: Value| {
        json!({ "user_id": user, "channel_id": CH, "message_id": "50", "guild_id": G, "member": member(user, bot), "emoji": emoji, "burst": false, "type": 0 })
    };
    feed(&t, &gw, event("MESSAGE_REACTION_ADD", r("600000000000000001", false, json!({ "id": null, "name": "⭐" }))), Before::default()).await;
    feed(&t, &gw, event("MESSAGE_REACTION_ADD", r(TEST_BOT_ID, true, json!({ "id": null, "name": "⭐" }))), Before::default()).await;
    feed(&t, &gw, event("MESSAGE_REACTION_ADD", r("600000000000000009", true, json!({ "id": null, "name": "⭐" }))), Before::default()).await;
    feed(&t, &gw, event("MESSAGE_REACTION_ADD", r("600000000000000002", false, json!({ "id": "123", "name": "pepe" }))), Before::default()).await;
    assert_eq!(log_of(&log), vec!["reactionAdd 600000000000000001 ⭐", "reactionAdd 600000000000000002 123"]);
}

#[tokio::test]
async fn voice_states_classify_and_skip_bots() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let state = |user: &str, channel: Option<&str>, bot: bool| {
        json!({ "guild_id": G, "channel_id": channel, "user_id": user, "session_id": "s", "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
            "self_video": false, "suppress": false, "request_to_speak_timestamp": null,
            "member": { "user": { "id": user, "username": "u", "discriminator": "0", "avatar": null, "bot": bot }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 } })
    };
    let vs = |v: Value| serde_json::from_value::<serenity::all::VoiceState>(v).expect("voice state");
    feed(&t, &gw, event("VOICE_STATE_UPDATE", state("600000000000000001", Some("1"), false)), Before::default()).await;
    feed(&t, &gw, event("VOICE_STATE_UPDATE", state("600000000000000001", Some("2"), false)), Before { voice: Some(vs(state("600000000000000001", Some("1"), false))), ..Default::default() }).await;
    feed(&t, &gw, event("VOICE_STATE_UPDATE", state("600000000000000001", None, false)), Before { voice: Some(vs(state("600000000000000001", Some("2"), false))), ..Default::default() }).await;
    feed(&t, &gw, event("VOICE_STATE_UPDATE", state("600000000000000009", Some("1"), true)), Before::default()).await;
    assert_eq!(log_of(&log), vec!["voice join", "voice move", "voice leave"]);
}

#[tokio::test]
async fn hooks_are_entered_in_gateway_order() {
    let (t, _log) = app();
    let gw = GatewayState::new(1);
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lock = bot_core::sync::KeyedMutex::new();
    let (o, l) = (order.clone(), lock.clone());
    let _ = t.hooks().register_hook(
        Hook::<MessageCreate>::new("counting", move |ctx: Arc<HookCtx<MessageCreate>>| {
            let o = o.clone();
            let content = ctx.data.message.content.clone();
            // Takes its place in the lock synchronously, like the counting plugin.
            let fut = l.run(G, move || async move {
                tokio::time::sleep(std::time::Duration::from_millis(if content == "1" { 30 } else { 0 })).await;
                o.lock().unwrap().push(content);
            });
            async move {
                fut.await;
                Ok(())
            }
        })
        .ungated(),
        "counting",
        None,
    );
    for (i, c) in ["1", "2", "3", "4"].iter().enumerate() {
        let ev = event("MESSAGE_CREATE", msg_json(&format!("{}", 100 + i), "600000000000000001", c, json!({})));
        gateway::process(&t, &gw, Envelope { shard: 0, event: ev, before: Before::default() }).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(*order.lock().unwrap(), vec!["1", "2", "3", "4"]);
}

#[tokio::test]
async fn the_snapshot_reads_the_cache_before_the_update() {
    let (t, _) = app();
    let d = t.discord().expect("offline discord");
    let ev = event("CHANNEL_UPDATE", json!({ "id": CH, "type": 0, "guild_id": G, "name": "renamed", "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": null }));
    let before = gateway::snapshot(&d.cache, &ev, chrono::Utc::now().timestamp_millis());
    assert_eq!(before.channel.map(|c| c.name), Some("general".to_string()));
    let ev = event("GUILD_DELETE", json!({ "id": G }));
    let before = gateway::snapshot(&d.cache, &ev, 0);
    assert!(before.guild.is_some() && before.guild_known && !before.guild_unavailable);
}

#[tokio::test]
async fn shard_arithmetic_for_one_process() {
    let gw = GatewayState::new(4);
    let g = serenity::all::GuildId::new(900000000000000200);
    assert_eq!(gw.shard_of(g), ((900000000000000200u64 >> 22) % 4) as u32);
    let d = bot_core::discord::DiscordHandle::new(
        Arc::new(serenity::all::Http::new("x")),
        Arc::new(serenity::all::Cache::new()),
        Some(vec![0, 1, 2, 3]),
        4,
    );
    let t = TestApp::new();
    t.set_discord(d);
    assert!(matches!(bot_core::scheduler::Scheduler::shard_scope(&t), bot_core::scheduler::Owned::Shards { count: 4, .. }), "every shard of the process is claimed");
    assert!(t.scheduler().owner_of(&t).starts_with("0,1,2,3:"));
    assert_eq!(bot_core::client::intents(false) | serenity::all::GatewayIntents::GUILD_PRESENCES, bot_core::client::intents(true));
    assert_eq!(bot_core::client::cache_settings().max_messages, 200);
}

/// discord.js actions that emit nothing: GuildDeleteAction for a guild it never cached, MessageDeleteBulkAction and
/// InviteCreate/InviteDeleteAction for a channel it has not cached.
#[tokio::test]
async fn events_for_uncached_guilds_and_channels_are_not_dispatched() {
    let (t, log) = app();
    let l = log.clone();
    let _ = t.hooks().register_hook(
        Hook::<MessageDeleteBulk>::new("logging", move |ctx: Arc<HookCtx<MessageDeleteBulk>>| {
            let l = l.clone();
            async move {
                l.lock().unwrap().push(format!("bulk {}", ctx.data.channel_id));
                Ok(())
            }
        })
        .ungated(),
        "logging",
        None,
    );
    for (kind, label) in [("INVITE_CREATE", "inviteCreate"), ("INVITE_DELETE", "inviteDelete")] {
        let l = log.clone();
        if kind == "INVITE_CREATE" {
            let _ = t.hooks().register_hook(
                Hook::<InviteCreate>::new("logging", move |ctx: Arc<HookCtx<InviteCreate>>| {
                    let l = l.clone();
                    async move {
                        l.lock().unwrap().push(format!("{label} {}", ctx.data.invite.channel_id));
                        Ok(())
                    }
                })
                .ungated(),
                "logging",
                None,
            );
        } else {
            let _ = t.hooks().register_hook(
                Hook::<InviteDelete>::new("logging", move |ctx: Arc<HookCtx<InviteDelete>>| {
                    let l = l.clone();
                    async move {
                        l.lock().unwrap().push(format!("{label} {}", ctx.data.invite.channel_id));
                        Ok(())
                    }
                })
                .ungated(),
                "logging",
                None,
            );
        }
    }
    let gw = GatewayState::new(1);
    const OTHER: &str = "700000000000000299";

    // guildDelete for a guild that was never in the cache.
    feed(&t, &gw, event("GUILD_DELETE", json!({ "id": "900000000000000299" })), Before::default()).await;
    // Bulk deletes and invites in an uncached channel.
    feed(&t, &gw, event("MESSAGE_DELETE_BULK", json!({ "ids": ["1", "2"], "channel_id": OTHER, "guild_id": G })), Before::default()).await;
    let invite = |ch: &str| json!({ "channel_id": ch, "guild_id": G, "code": "abc", "created_at": "2026-01-01T00:00:00.000Z", "max_age": 0, "max_uses": 0, "temporary": false, "uses": 0 });
    feed(&t, &gw, event("INVITE_CREATE", invite(OTHER)), Before::default()).await;
    feed(&t, &gw, event("INVITE_DELETE", json!({ "channel_id": OTHER, "guild_id": G, "code": "abc" })), Before::default()).await;
    assert!(log_of(&log).is_empty(), "{:?}", log_of(&log));

    // The same events in a cached channel are dispatched.
    feed(&t, &gw, event("MESSAGE_DELETE_BULK", json!({ "ids": ["1", "2"], "channel_id": CH, "guild_id": G })), Before::default()).await;
    feed(&t, &gw, event("INVITE_CREATE", invite(CH)), Before::default()).await;
    feed(&t, &gw, event("INVITE_DELETE", json!({ "channel_id": CH, "guild_id": G, "code": "abc" })), Before::default()).await;
    assert_eq!(log_of(&log), vec![format!("bulk {CH}"), format!("inviteCreate {CH}"), format!("inviteDelete {CH}")]);
}

/// PresenceUpdateAction emits only when `!newPresence.equals(oldPresence)` (status, client statuses, activities).
#[test]
fn presence_equality_follows_discord_js() {
    let p = |status: &str, activities: Value, client: Value| -> serenity::all::Presence {
        serde_json::from_value(json!({ "user": { "id": "600000000000000002" }, "guild_id": G, "status": status, "activities": activities, "client_status": client }))
            .expect("presence")
    };
    let game = json!([{ "name": "Chess", "type": 0, "state": "Turn 3", "created_at": 1 }]);
    let game_later = json!([{ "name": "Chess", "type": 0, "state": "Turn 3", "created_at": 999 }]);
    let game_moved = json!([{ "name": "Chess", "type": 0, "state": "Turn 4", "created_at": 1 }]);
    let desk = json!({ "desktop": "online" });
    assert!(gateway::presence_equals(&p("online", game.clone(), desk.clone()), &p("online", game_later, desk.clone())), "timestamps are not compared");
    assert!(!gateway::presence_equals(&p("online", game.clone(), desk.clone()), &p("online", game_moved, desk.clone())));
    assert!(!gateway::presence_equals(&p("online", game.clone(), desk.clone()), &p("idle", game.clone(), desk.clone())));
    assert!(!gateway::presence_equals(&p("online", game.clone(), desk.clone()), &p("online", game.clone(), json!({ "mobile": "online" }))));
    assert!(!gateway::presence_equals(&p("online", game.clone(), desk.clone()), &p("online", json!([]), desk)));
}

/// GuildMemberUpdateAction emits guildMemberUpdate only when `!member.equals(old)`: a rename of the user alone is a
/// userUpdate; a nickname or role change is a guildMemberUpdate.
#[tokio::test]
async fn a_member_update_that_changes_nothing_discord_js_compares_is_not_dispatched() {
    let (t, log) = app();
    let gw = GatewayState::new(1);
    let uid = serenity::all::UserId::new(600000000000000002);
    let cached_user = t.discord().and_then(|d| d.cache.user(uid).map(|u| u.clone())).expect("cached user");
    let mut old: serenity::all::Member = serde_json::from_value(json!({
        "guild_id": G, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "pending": false, "flags": 0,
        "user": { "id": "600000000000000002", "username": "before", "discriminator": "0", "avatar": null }
    }))
    .expect("member");
    old.nick = None;
    let update = |name: &str, nick: Value| {
        json!({
            "guild_id": G, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "pending": false, "flags": 0,
            "nick": nick, "user": { "id": "600000000000000002", "username": name, "discriminator": "0", "avatar": null }
        })
    };
    feed(&t, &gw, event("GUILD_MEMBER_UPDATE", update("renamed", Value::Null)), Before { user: Some(cached_user.clone()), member: Some(old.clone()), ..Default::default() }).await;
    let l = log_of(&log);
    assert!(!l.iter().any(|x| x.starts_with("guildMemberUpdate")), "{l:?}");
    assert!(l.iter().any(|x| x.starts_with("userUpdate")), "{l:?}");
    log.lock().unwrap().clear();
    feed(&t, &gw, event("GUILD_MEMBER_UPDATE", update("renamed", json!("Nick"))), Before { member: Some(old), ..Default::default() }).await;
    assert_eq!(log_of(&log), vec!["guildMemberUpdate oldKnown=true".to_string()]);
}
