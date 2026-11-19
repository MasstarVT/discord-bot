//! Database and Redis tests of feat-search (run with `eval "$(node rust/mockdb/mockdb.mjs db feat_search)"`): the AFK
//! handler and `/afk` against the real `AfkStatus` table (ported from `test/afk.test.js`), the raw SQL, and both
//! cross-language directions for the AFK rows and the search cache (`rust/fixtures/gen/feat-search-js.mjs`).

mod common;

use std::sync::Arc;

use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, offline_discord_as, redis_url, run_node};
use common::*;
use feat_search::afk_service::{self as svc, afk};
use feat_search::ops;
use feat_search::plugins::afk::{AfkMessage, handle_message, may_concern_afk};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, MessageId, Permissions as P, User};

const JS: &str = "rust/fixtures/gen/feat-search-js.mjs";
const BOTR: &str = "850000000000000010";
const LOW: &str = "850000000000000001";
const MOD: &str = "850000000000000020";
const C: &str = "850000000000000100";
const C_MUTE: &str = "850000000000000101";
const CAT: &str = "850000000000000200";
const C_IN_CAT: &str = "850000000000000102";
const BOB: &str = "650000000000000001";
const AMY: &str = "650000000000000002";
const CAT_USER: &str = "650000000000000003";
const MODU: &str = "650000000000000009";

fn guild_json(g: &str) -> Value {
    let mut j = FakeGuild::new(g, "650000000000000099")
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES | P::READ_MESSAGE_HISTORY)
        .role(LOW, 1, P::empty())
        .role(BOTR, 10, P::MANAGE_NICKNAMES)
        .role(MOD, 20, P::ADMINISTRATOR)
        .member(TEST_BOT_ID, &[BOTR])
        .member(BOB, &[LOW])
        .member(AMY, &[LOW])
        .member(CAT_USER, &[LOW])
        .member(MODU, &[MOD])
        .channel(C, 0, "general", &[])
        .channel(C_MUTE, 0, "muted", &[(BOTR, false, P::empty(), P::SEND_MESSAGES)])
        .channel(CAT, 4, "a-category", &[])
        .channel(C_IN_CAT, 0, "in-category", &[])
        .json();
    if let Some(chs) = j["channels"].as_array_mut() {
        for c in chs {
            if c["id"] == C_IN_CAT {
                c["parent_id"] = json!(CAT);
            }
        }
    }
    j
}

struct Afk {
    t: DbTestApp,
    d: Arc<FakeDiscord>,
    g: &'static str,
    next: std::sync::atomic::AtomicU64,
}

async fn afk_app(g: &'static str, patch: SettingsPatch) -> Option<Afk> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let d = FakeDiscord::new(&[guild_json(g)]);
    ops::set_ops(&t, Arc::new(FakeOps(d.clone())));
    t.set_discord(offline_discord_as(&[guild_json(g)]));
    t.settings().update_settings(g, SettingsPatch::new().set("afkEnabled", true).set("afkNicknamePrefix", true)).await.expect("settings");
    if !patch.is_empty() {
        t.settings().update_settings(g, patch).await.expect("settings");
    }
    Some(Afk { t, d, g, next: std::sync::atomic::AtomicU64::new(700_000_000_000_000_001) })
}

fn user(id: &str, global: Option<&str>) -> User {
    serde_json::from_value(json!({ "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "global_name": global })).expect("user")
}

impl Afk {
    fn gid(&self) -> GuildId {
        GuildId::new(self.g.parse().unwrap())
    }

    fn msg(&self, channel: &str, author: &str, nick: Option<&str>, mentions: &[(&str, Option<&str>)]) -> AfkMessage {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        AfkMessage {
            guild_id: self.gid(),
            channel_id: ChannelId::new(channel.parse().unwrap()),
            message_id: MessageId::new(id),
            author: user(author, None),
            member: Some(member(self.g, author, &[LOW], nick)),
            mentions: mentions.iter().map(|(u, n)| (user(u, None), n.map(str::to_string))).collect(),
            parent_id: None,
        }
    }

    async fn handle(&self, m: &AfkMessage, delete_ms: u64) {
        let s = self.t.settings().get_settings_safe(self.g).await;
        handle_message(&self.t, s.as_deref(), m, delete_ms).await;
    }

    async fn seed(&self, user: &str, minutes_ago: i64, extra: &[(&str, Value)]) {
        if self.row(user).await.is_none() {
            svc::set_afk(&self.t, self.g, user, None, false, None).await.expect("set");
        }
        let sql = r#"UPDATE "AfkStatus" SET "since" = "since" - make_interval(mins => $3::int) WHERE "guildId" = $1 AND "userId" = $2"#;
        bot_db::sqlx::query(sql).bind(self.g).bind(user).bind(minutes_ago as i32).execute(&self.t.pool).await.expect("age");
        for (col, v) in extra {
            let sql = format!(r#"UPDATE "AfkStatus" SET "{col}" = $3 WHERE "guildId" = $1 AND "userId" = $2"#);
            let q = bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(sql)).bind(self.g).bind(user);
            let q = match v {
                Value::Bool(b) => q.bind(*b),
                Value::Number(n) => q.bind(n.as_i64().unwrap_or(0) as i32),
                Value::String(s) => q.bind(s.clone()),
                other => q.bind(other.clone()),
            };
            q.execute(&self.t.pool).await.expect("seed column");
        }
    }

    async fn row(&self, user: &str) -> Option<bot_db::AfkStatus> {
        svc::load_status(&self.t.pool, self.g, user).await.expect("load")
    }

    fn replies(&self) -> Vec<String> {
        self.d.calls().into_iter().filter_map(|c| if let Call::Reply { content, .. } = c { Some(content) } else { None }).collect()
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

// ── State and the id memo ─────────────────────────────────────────────────────

#[tokio::test]
async fn set_afk_clear_afk_and_the_memoised_ids() {
    let Some(a) = afk_app("950000000000000001", SettingsPatch::new()).await else { return };
    let g = a.g;
    assert!(svc::peek_afk_ids(&a.t, g).is_none(), "unknown until loaded");
    assert!(svc::load_afk_ids(&a.t, g).await.is_empty());
    let row = svc::set_afk(&a.t, g, BOB, Some("lunch"), true, Some("Uncle One")).await.expect("set");
    assert_eq!((row.reason.as_deref(), row.previous_nick.as_deref(), row.nick_changed), (Some("lunch"), Some("Uncle One"), true));
    assert!(svc::peek_afk_ids(&a.t, g).expect("memo").contains(BOB), "the memo follows the write");

    // A second set changes the reason only: the start time, the pings, the nickname flag and the saved nickname stay.
    a.seed(BOB, 3, &[("pingCount", json!(4))]).await;
    let since = a.row(BOB).await.expect("row").since;
    svc::set_afk(&a.t, g, BOB, Some("dinner"), false, None).await.expect("set");
    let kept = a.row(BOB).await.expect("row");
    assert_eq!((kept.reason.as_deref(), kept.since, kept.ping_count, kept.nick_changed, kept.previous_nick.as_deref()), (Some("dinner"), since, 4, true, Some("Uncle One")));

    let cleared = svc::clear_afk(&a.t, g, BOB).await.expect("clear").expect("row");
    assert_eq!(cleared.reason.as_deref(), Some("dinner"));
    assert!(!svc::peek_afk_ids(&a.t, g).expect("memo").contains(BOB));
    assert!(svc::clear_afk(&a.t, g, BOB).await.expect("clear").is_none(), "only the first clear gets the row");
    a.done().await;
}

#[tokio::test]
async fn the_hook_filter_lets_through_only_an_afk_author_or_a_mention_of_one() {
    let Some(a) = afk_app("950000000000000002", SettingsPatch::new()).await else { return };
    let g = a.g;
    assert!(may_concern_afk(&a.t, g, BOB, &[]), "not loaded yet: the handler decides");
    svc::load_afk_ids(&a.t, g).await;
    assert!(!may_concern_afk(&a.t, g, BOB, &[]), "nobody is AFK");
    svc::set_afk(&a.t, g, AMY, None, false, None).await.expect("set");
    assert!(may_concern_afk(&a.t, g, AMY, &[]));
    assert!(may_concern_afk(&a.t, g, BOB, &[AMY.to_string()]));
    assert!(!may_concern_afk(&a.t, g, BOB, &[CAT_USER.to_string()]));
    a.done().await;
}

#[tokio::test]
async fn a_failed_load_reads_as_nobody_afk_for_a_moment_and_concurrent_loads_share_it() {
    // Offline: the database is unreachable.
    let t = TestApp::new();
    let (x, y) = tokio::join!(svc::load_afk_ids(&t, "950000000000000003"), svc::load_afk_ids(&t, "950000000000000003"));
    assert!(x.is_empty() && Arc::ptr_eq(&x, &y), "one shared load");
    assert!(svc::peek_afk_ids(&t, "950000000000000003").expect("remembered").is_empty());
    t.clock.advance(afk::IDS_FAILED_TTL_MS + 1);
    assert!(svc::peek_afk_ids(&t, "950000000000000003").is_none(), "retried after the short TTL");
}

// ── The message handler ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_afk_author_writes_and_gets_a_self_deleting_welcome_back() {
    let Some(a) = afk_app("950000000000000010", SettingsPatch::new()).await else { return };
    a.d.edit(a.g, |g| g.members.get_mut(&serenity::all::UserId::new(BOB.parse().unwrap())).unwrap().nick = Some("[AFK] Bobster".into()));
    let pings = json!([{ "channelId": C, "messageId": "M1", "authorId": "X", "at": 1 }, { "channelId": "C11", "messageId": "M2", "authorId": "Y", "at": 2 }]);
    a.seed(BOB, 75, &[("nickChanged", json!(true)), ("previousNick", json!("Bobster")), ("pings", pings), ("pingCount", json!(2))]).await;
    let m = a.msg(C, BOB, Some("[AFK] Bobster"), &[]);
    a.handle(&m, 15).await;
    assert!(a.row(BOB).await.is_none());
    assert!(!svc::peek_afk_ids(&a.t, a.g).expect("memo").contains(BOB));
    assert_eq!(a.d.nick_of(a.g, BOB).as_deref(), Some("Bobster"));
    let replies = a.replies();
    assert_eq!(replies.len(), 1);
    let g = a.g;
    assert_eq!(
        replies[0],
        format!(
            "👋 Welcome back, **Bobster**! You were AFK for **1h 15m**.\nYou were mentioned **2** times: [1](https://discord.com/channels/{g}/{C}/M1) · [2](https://discord.com/channels/{g}/C11/M2)"
        )
    );
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert!(a.d.calls().iter().any(|c| matches!(c, Call::Delete { .. })), "the welcome deletes itself");
    a.done().await;
}

#[tokio::test]
async fn a_message_within_ten_seconds_of_afk_set_does_not_end_it() {
    let Some(a) = afk_app("950000000000000011", SettingsPatch::new()).await else { return };
    svc::set_afk(&a.t, a.g, BOB, None, false, None).await.expect("set");
    a.handle(&a.msg(C, BOB, None, &[]), 0).await;
    assert!(a.row(BOB).await.is_some());
    assert!(a.replies().is_empty());
    a.done().await;
}

#[tokio::test]
async fn ignored_channels_and_categories_neither_end_an_afk_nor_answer_mentions() {
    let Some(a) = afk_app("950000000000000012", SettingsPatch::new().set("afkIgnoredChannelIds", json!([C, CAT]))).await else { return };
    a.seed(BOB, 5, &[]).await;
    a.seed(AMY, 5, &[]).await;
    a.handle(&a.msg(C, BOB, None, &[]), 0).await;
    a.handle(&a.msg(C_IN_CAT, CAT_USER, None, &[(AMY, None)]), 0).await;
    assert!(a.row(BOB).await.is_some());
    assert_eq!(a.row(AMY).await.expect("row").ping_count, 0);
    assert!(a.d.calls().is_empty());
    a.done().await;
}

#[tokio::test]
async fn a_mention_gets_one_reply_with_the_reason_records_the_ping_and_is_announced_once_a_minute() {
    let Some(a) = afk_app("950000000000000013", SettingsPatch::new()).await else { return };
    svc::set_afk(&a.t, a.g, AMY, Some("at the <@1> gym"), false, None).await.expect("set");
    a.seed(AMY, 5, &[]).await;
    let m = a.msg(C, BOB, None, &[(AMY, Some("Amy"))]);
    a.handle(&m, 0).await;
    let replies = a.replies();
    assert_eq!(replies.len(), 1);
    let since = a.row(AMY).await.expect("row").since.timestamp();
    assert_eq!(replies[0], format!("💤 **Amy** is AFK: at the <\u{200B}@1> gym (since <t:{since}:R>)"));
    let row = a.row(AMY).await.expect("row");
    assert_eq!(row.ping_count, 1);
    assert_eq!(row.pings[0]["channelId"], C);
    assert_eq!(row.pings[0]["messageId"], m.message_id.to_string());
    assert_eq!(row.pings[0]["authorId"], BOB);
    // Again in the same channel within a minute: recorded, not announced.
    a.handle(&a.msg(C, CAT_USER, None, &[(AMY, None)]), 0).await;
    assert_eq!(a.replies().len(), 1);
    assert_eq!(a.row(AMY).await.expect("row").ping_count, 2);
    a.done().await;
}

#[tokio::test]
async fn only_the_last_ten_pings_are_kept_and_the_count_covers_all() {
    let Some(a) = afk_app("950000000000000014", SettingsPatch::new()).await else { return };
    a.seed(AMY, 5, &[]).await;
    for _ in 0..12 {
        a.handle(&a.msg(C, BOB, None, &[(AMY, None)]), 0).await;
    }
    let row = a.row(AMY).await.expect("row");
    assert_eq!(row.ping_count, 12);
    assert_eq!(svc::read_pings(&row.pings).len(), bot_config::limits::AFK_PINGS_KEPT);
    a.done().await;
}

#[tokio::test]
async fn self_mentions_are_not_announced_and_at_most_five_are() {
    let Some(a) = afk_app("950000000000000015", SettingsPatch::new()).await else { return };
    let many: Vec<String> = (0..7).map(|i| format!("65000000000000010{i}")).collect();
    for u in &many {
        a.seed(u, 5, &[]).await;
    }
    a.seed(BOB, 0, &[]).await; // just set: writing does not end it
    let mut mentions: Vec<(&str, Option<&str>)> = vec![(BOB, None)];
    mentions.extend(many.iter().map(|u| (u.as_str(), None)));
    a.handle(&a.msg(C, BOB, None, &mentions), 0).await;
    let replies = a.replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].lines().count(), afk::MAX_NOTICES);
    assert!(!replies[0].contains(&format!("user{BOB}")));
    a.done().await;
}

#[tokio::test]
async fn an_afk_author_who_mentions_another_afk_member_returns_and_announces() {
    let Some(a) = afk_app("950000000000000016", SettingsPatch::new()).await else { return };
    a.seed(BOB, 5, &[]).await;
    a.seed(AMY, 5, &[]).await;
    a.handle(&a.msg(C, BOB, None, &[(AMY, None)]), 0).await;
    assert!(a.row(BOB).await.is_none());
    let replies = a.replies();
    assert_eq!(replies.len(), 2);
    assert!(replies[0].starts_with("👋 Welcome back") && replies[1].starts_with("💤 **userAMY**".replace("AMY", AMY).as_str()));
    a.done().await;
}

#[tokio::test]
async fn a_renamed_member_is_left_alone_and_nothing_is_touched_without_a_prefix() {
    let Some(a) = afk_app("950000000000000017", SettingsPatch::new()).await else { return };
    a.seed(BOB, 5, &[("nickChanged", json!(true)), ("previousNick", json!("Bobster"))]).await;
    a.handle(&a.msg(C, BOB, Some("Robert"), &[]), 0).await;
    a.seed(AMY, 5, &[]).await;
    a.handle(&a.msg(C, AMY, Some("[AFK] Amy"), &[]), 0).await;
    assert!(!a.d.calls().iter().any(|c| matches!(c, Call::Nick { .. })));
    a.done().await;
}

#[tokio::test]
async fn without_send_messages_the_ping_is_recorded_but_nothing_sent_and_failures_do_not_throw() {
    let Some(a) = afk_app("950000000000000018", SettingsPatch::new()).await else { return };
    a.seed(AMY, 5, &[]).await;
    a.handle(&a.msg(C_MUTE, BOB, None, &[(AMY, None)]), 0).await;
    assert_eq!(a.row(AMY).await.expect("row").ping_count, 1);
    assert!(a.replies().is_empty());
    a.d.fail_next("reply", 10008);
    a.handle(&a.msg(C, BOB, None, &[(AMY, None)]), 0).await;
    assert_eq!(a.row(AMY).await.expect("row").ping_count, 2);
    a.done().await;
}

// ── /afk ──────────────────────────────────────────────────────────────────────

fn afk_cmd(g: &str, sub: &str, user: &str, nick: Option<&str>) -> FakeInteraction {
    let mut i = FakeInteraction::slash("afk").guild(g).user(user).roles(&[LOW]).channel(C).subcommand(sub);
    i.json_mut()["member"]["nick"] = json!(nick);
    i
}

async fn answer(a: &Afk, i: serenity::all::Interaction) -> bot_core::interaction::RecordedCall {
    a.t.responses.clear();
    route_interaction(&a.t, i).await;
    let calls = a.t.responses.calls();
    assert_eq!(calls[0].method, "deferReply", "{calls:?}");
    assert!(calls[0].ephemeral(), "replies are private");
    let last = calls.last().cloned().expect("answer");
    assert_eq!(last.body["allowed_mentions"]["parse"], json!([]));
    last
}

#[tokio::test]
async fn afk_set_marks_the_member_adds_the_prefix_and_clear_takes_it_off() {
    let Some(a) = afk_app("950000000000000020", SettingsPatch::new()).await else { return };
    a.t.install(registry());
    a.d.edit(a.g, |g| g.members.get_mut(&serenity::all::UserId::new(BOB.parse().unwrap())).unwrap().nick = Some("Bobster".into()));
    let set = afk_cmd(a.g, "set", BOB, Some("Bobster")).option("reason", option_type::STRING, json!("  at\nthe   gym ")).build();
    let r = answer(&a, set).await;
    assert_eq!(r.embed_title(), Some("✅  💤 AFK"));
    assert_eq!(r.embed_description(), Some("You are now AFK: at the gym\nAnyone who mentions you is told. Your next message ends it."));
    let row = a.row(BOB).await.expect("row");
    assert_eq!((row.reason.as_deref(), row.nick_changed, row.previous_nick.as_deref()), (Some("at the gym"), true, Some("Bobster")));
    assert_eq!(a.d.nick_of(a.g, BOB).as_deref(), Some("[AFK] Bobster"));

    let r = answer(&a, afk_cmd(a.g, "clear", BOB, Some("[AFK] Bobster")).build()).await;
    assert_eq!(r.embed_title(), Some("✅  Welcome back"));
    assert_eq!(r.embed_description(), Some("You were AFK for **0s**."));
    assert_eq!(a.d.nick_of(a.g, BOB).as_deref(), Some("Bobster"));
    let r = answer(&a, afk_cmd(a.g, "clear", BOB, None).build()).await;
    assert_eq!((r.embed_title(), r.embed_description()), (Some("ℹ️  Not AFK"), Some("You are not AFK.")));
    a.done().await;
}

#[tokio::test]
async fn afk_set_without_the_setting_or_the_rights_still_works_and_says_why() {
    let Some(a) = afk_app("950000000000000021", SettingsPatch::new().set("afkNicknamePrefix", false)).await else { return };
    a.t.install(registry());
    let r = answer(&a, afk_cmd(a.g, "set", BOB, None).build()).await;
    assert_eq!(r.embed_description(), Some("You are now AFK.\nAnyone who mentions you is told. Your next message ends it."));
    assert!(a.d.calls().is_empty());
    a.t.settings().update_settings(a.g, SettingsPatch::new().set("afkNicknamePrefix", true)).await.expect("settings");
    // The moderator's role is above the bot's: it cannot rename them.
    let mut i = afk_cmd(a.g, "set", MODU, None);
    i.json_mut()["member"]["roles"] = json!([MOD]);
    let r = answer(&a, i.build()).await;
    assert!(r.embed_description().unwrap_or("").ends_with("-# I couldn't add [AFK] to your nickname: I need Manage Nicknames and a role above yours."));
    a.d.fail_next("nick", 500);
    let r = answer(&a, afk_cmd(a.g, "set", AMY, None).build()).await;
    assert!(r.embed_description().unwrap_or("").ends_with("-# I couldn't change your nickname just now."));
    a.done().await;
}

#[tokio::test]
async fn afk_remove_and_list_for_moderators() {
    let Some(a) = afk_app("950000000000000022", SettingsPatch::new()).await else { return };
    a.t.install(registry());
    a.d.edit(a.g, |g| g.members.get_mut(&serenity::all::UserId::new(AMY.parse().unwrap())).unwrap().nick = Some("[AFK] Amy".into()));
    a.seed(AMY, 30, &[("nickChanged", json!(true)), ("previousNick", json!("Amy"))]).await;
    svc::set_afk(&a.t, a.g, BOB, Some("@everyone lunch"), false, None).await.expect("set");
    a.seed(BOB, 10, &[]).await;
    let modi = |sub: &str| {
        let mut i = afk_cmd(a.g, sub, MODU, None).permissions(P::ADMINISTRATOR);
        i.json_mut()["member"]["roles"] = json!([MOD]);
        i
    };
    let r = answer(&a, modi("list").build()).await;
    assert_eq!(r.embed_title(), Some("ℹ️  💤 AFK members (2)"));
    let amy_since = a.row(AMY).await.expect("row").since.timestamp();
    let bob_since = a.row(BOB).await.expect("row").since.timestamp();
    assert_eq!(r.embed_description(), Some(format!("<@{AMY}> (since <t:{amy_since}:R>)\n<@{BOB}>: @\u{200B}everyone lunch (since <t:{bob_since}:R>)").as_str()));

    let r = answer(&a, modi("remove").option("user", option_type::USER, json!(AMY)).build()).await;
    assert_eq!((r.embed_title(), r.embed_description()), (Some("✅  AFK removed"), Some(format!("<@{AMY}> is no longer AFK.").as_str())));
    assert_eq!(a.d.nick_of(a.g, AMY).as_deref(), Some("Amy"), "the prefix we added comes off");
    let r = answer(&a, modi("remove").option("user", option_type::USER, json!(AMY)).build()).await;
    assert_eq!(r.embed_description(), Some(format!("user{AMY} is not AFK.").as_str()));

    // Members may not use them.
    a.t.responses.clear();
    route_interaction(&a.t, afk_cmd(a.g, "list", BOB, None).build()).await;
    assert_ne!(a.t.responses.calls().first().map(|c| c.method), Some("deferReply"));
    a.done().await;
}

#[tokio::test]
async fn a_nickname_too_long_for_the_prefix_comes_back_whole_and_no_nickname_stays_none() {
    let Some(a) = afk_app("950000000000000023", SettingsPatch::new()).await else { return };
    a.t.install(registry());
    let long = "L".repeat(32);
    a.d.edit(a.g, |g| g.members.get_mut(&serenity::all::UserId::new(BOB.parse().unwrap())).unwrap().nick = Some(long.clone()));
    answer(&a, afk_cmd(a.g, "set", BOB, Some(&long)).build()).await;
    let prefixed = a.d.nick_of(a.g, BOB).expect("nick");
    assert_eq!(bot_util::js::utf16_len(&prefixed), 32);
    a.seed(BOB, 5, &[]).await;
    a.handle(&a.msg(C, BOB, Some(&prefixed), &[]), 0).await;
    assert_eq!(a.d.nick_of(a.g, BOB), Some(long));

    // No nickname before: none after; a second set keeps the saved (absent) nickname.
    answer(&a, afk_cmd(a.g, "set", AMY, None).build()).await;
    assert_eq!(a.d.nick_of(a.g, AMY).as_deref(), Some("[AFK] member"));
    answer(&a, afk_cmd(a.g, "set", AMY, Some("[AFK] member")).option("reason", option_type::STRING, json!("again")).build()).await;
    let row = a.row(AMY).await.expect("row");
    assert_eq!((row.nick_changed, row.previous_nick.as_deref(), row.reason.as_deref()), (true, None, Some("again")));
    answer(&a, afk_cmd(a.g, "clear", AMY, Some("[AFK] member")).build()).await;
    assert_eq!(a.d.nick_of(a.g, AMY), None);
    a.done().await;
}

#[tokio::test]
async fn afk_set_that_cannot_save_puts_the_nickname_back() {
    // Offline: the database is unreachable, so the row cannot be written.
    let g = "950000000000000024";
    let t = TestApp::new().with_guilds([g]);
    let d = FakeDiscord::new(&[guild_json(g)]);
    ops::set_ops(&t, Arc::new(FakeOps(d.clone())));
    t.install(registry());
    t.settings().update_settings(g, SettingsPatch::new().set("afkEnabled", true).set("afkNicknamePrefix", true)).await.expect("settings");
    route_interaction(&t, afk_cmd(g, "set", BOB, Some("Bobster")).build()).await;
    let nicks: Vec<Option<String>> = d.calls().into_iter().filter_map(|c| if let Call::Nick { nick, .. } = c { Some(nick) } else { None }).collect();
    assert_eq!(nicks, vec![Some("[AFK] Bobster".into()), Some("Bobster".into())]);
    let last = t.responses.calls().last().cloned().expect("answer");
    assert_eq!(last.embed_title(), Some("⚠️  Something went wrong"));
}

// ── Raw SQL against the schema ────────────────────────────────────────────────

#[tokio::test]
async fn every_raw_statement_runs_against_the_schema() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.expect("tx");
    for (name, sql) in svc::RAW_SQL {
        let q = bot_db::sqlx::query(sql).bind("950000000000000099");
        let q = match name {
            "loadStatuses" => q.bind(vec!["1".to_string()]),
            "recordPing" => q.bind("1").bind(json!([])),
            _ => q,
        };
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    tx.rollback().await.expect("rollback");
}

// ── Cross-language: AFK rows ─────────────────────────────────────────────────

#[tokio::test]
async fn js_writes_afk_rows_and_rust_reads_them() {
    let Some(url) = database_url() else { return };
    let g = "950000000000000030";
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let js = run_node(JS, &["afk-set", g, BOB, r#"{"reason":"lunch","nickChanged":true,"previousNick":"Bobster"}"#]);
    assert!(js.get("error").is_none(), "{js}");
    let js = run_node(JS, &["afk-ping", g, BOB, C, "700000000000000001", AMY]);
    let row = svc::load_status(&t.pool, g, BOB).await.expect("load").expect("row");
    assert_eq!(serde_json::to_value(&row).expect("json"), js, "the same row through Prisma and Rust");
    assert_eq!(svc::read_pings(&row.pings)[0].author_id.as_deref(), Some(AMY));
    assert!(svc::load_afk_ids(&t, g).await.contains(BOB));
    let (rows, total) = svc::list_statuses(&t.pool, g).await.expect("list");
    assert_eq!((rows.len(), total), (1, 1));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn rust_writes_afk_rows_and_js_reads_them() {
    let Some(url) = database_url() else { return };
    let g = "950000000000000031";
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    svc::set_afk(&t, g, BOB, Some("gym"), true, Some("Bobster")).await.expect("set");
    svc::set_afk(&t, g, AMY, None, false, Some("ignored")).await.expect("set");
    svc::record_ping(&t, g, BOB, C, "700000000000000002", AMY).await;
    svc::record_ping(&t, g, BOB, C, "700000000000000003", CAT_USER).await;
    let ours = serde_json::to_value(svc::load_status(&t.pool, g, BOB).await.expect("load").expect("row")).expect("json");
    let back = run_node(JS, &["afk-load", g, BOB]);
    assert_eq!(back, ours);
    assert_eq!(back["pingCount"], 2);
    assert_eq!(back["pings"][1]["authorId"], CAT_USER);
    assert_eq!(run_node(JS, &["afk-load", g, AMY])["previousNick"], Value::Null, "a previous nickname is only saved with a prefix");
    assert_eq!(run_node(JS, &["afk-ids", g]), json!([BOB, AMY].iter().map(|s| s.to_string()).collect::<std::collections::BTreeSet<_>>()));
    let list = run_node(JS, &["afk-list", g]);
    assert_eq!(list["total"], 2);
    let (rows, _) = svc::list_statuses(&t.pool, g).await.expect("list");
    assert_eq!(list["rows"], serde_json::to_value(&rows).expect("json"), "same order (longest AFK first) and rows");
    svc::clear_afk(&t, g, BOB).await.expect("clear");
    assert_eq!(run_node(JS, &["afk-load", g, BOB]), Value::Null);
    t.reset_guilds(&[g]).await;
}

// ── Cross-language: the search cache ─────────────────────────────────────────

mod cache {
    use super::*;
    use feat_search::search_service::{self as s, Api, MediaType, PokemonQuery, SearchCode};

    async fn redis_app() -> Option<DbTestApp> {
        let db = database_url()?;
        let redis = redis_url()?;
        Some(DbTestApp::with_redis(&db, &redis).await)
    }

    fn unique(tag: &str) -> String {
        format!("{tag} {}", bot_db::new_cuid())
    }

    #[tokio::test]
    async fn anilist_entries_written_by_one_side_are_hits_for_the_other() {
        let Some(t) = redis_app().await else { return };
        let media = json!([common::search_flows::frieren(), common::search_flows::adult()]);
        let canned = json!({ "status": 200, "headers": {}, "body": { "data": { "Page": { "media": media } } } }).to_string();

        // JS writes, Rust reads.
        let q = unique("frieren js");
        let js = run_node(JS, &["anilist", &q, "0", &canned]);
        assert_eq!((js["cached"].clone(), js["calls"].clone()), (json!(false), json!(1)), "{js}");
        let fetch = common::search_flows::CannedFetch::new(|_, _| panic!("a hit never fetches"));
        s::set_fetch(&t, Arc::new(common::search_flows::Shared(fetch)));
        let hit = s::search_anilist(&t, MediaType::Anime, &q, false, None, None).await.expect("hit");
        assert!(hit.cached);
        assert_eq!(serde_json::to_value(&hit.results).expect("json"), js["results"]);

        // Rust writes, JS reads.
        let q = unique("frieren rust");
        let ok = common::search_flows::CannedFetch::new(|_, _| common::search_flows::ani_body(vec![common::search_flows::frieren()]));
        s::set_fetch(&t, Arc::new(common::search_flows::Shared(ok)));
        let ours = s::search_anilist(&t, MediaType::Anime, &q, true, None, None).await.expect("ok");
        assert!(!ours.cached);
        let back = run_node(JS, &["anilist", &q, "1", &canned]);
        assert_eq!((back["cached"].clone(), back["calls"].clone()), (json!(true), json!(0)), "{back}");
        assert_eq!(back["results"], serde_json::to_value(&ours.results).expect("json"));
        // The envelope reads back identically on the Rust side too.
        let raw = t.cache().get(&s::cache_key(Api::AniList, &format!("anime|nsfw|{}", s::key_form(&q)))).await.expect("cached");
        assert_eq!(raw["v"], 1);

        // JS pauses AniList after a 429; Rust honours the shared pause without fetching.
        let q = unique("paused");
        let limited = json!({ "status": 429, "headers": { "retry-after": "30" }, "body": null }).to_string();
        let js = run_node(JS, &["anilist", &q, "0", &limited]);
        assert_eq!(js["code"], "RATE_LIMITED");
        let fresh = DbTestApp::with_redis(&database_url().unwrap_or_default(), &redis_url().unwrap_or_default()).await;
        s::set_fetch(&fresh, Arc::new(common::search_flows::Shared(common::search_flows::CannedFetch::new(|_, _| panic!("paused")))));
        let err = s::search_anilist(&fresh, MediaType::Anime, &unique("other"), false, None, None).await.expect_err("paused");
        assert_eq!(err.code, SearchCode::RateLimited);
        assert_eq!(err.retry_at, js["retryAt"].as_i64());
        fresh.cache().del_one(s::ANILIST_PAUSE_KEY).await;
    }

    /// The cooldown and budget keys: the JS writes them, Rust finds them under the names it builds; Rust's cooldown
    /// stops a JS lookup.
    #[tokio::test]
    async fn cooldown_and_budget_keys_are_the_same_on_both_sides() {
        let Some(t) = redis_app().await else { return };
        let empty = json!({ "status": 200, "headers": {}, "body": { "data": { "Page": { "media": [] } } } }).to_string();
        let (user, guild) = (format!("u{}", bot_db::new_cuid()), format!("g{}", bot_db::new_cuid()));
        let before = chrono::Utc::now().timestamp_millis() / 60_000;
        let js = run_node(JS, &["anilist", &unique("keys"), "0", &empty, &user, &guild]);
        let after = chrono::Utc::now().timestamp_millis() / 60_000;
        assert_eq!(js["calls"], 1, "{js}");
        assert_eq!(t.cache().get(&format!("search:cd:{user}")).await, Some(json!(1)), "the cooldown key");
        let mut found = false;
        for bucket in before..=after {
            let user_slice = t.cache().get(&format!("search:budget:anilist:u:{user}:{bucket}")).await;
            let guild_slice = t.cache().get(&format!("search:budget:anilist:g:{guild}:{bucket}")).await;
            let wide = t.cache().get(&format!("search:budget:anilist:{bucket}")).await;
            if user_slice == Some(json!(1)) && guild_slice == Some(json!(1)) {
                assert!(wide.and_then(|v| v.as_i64()).is_some_and(|n| n >= 1), "the bot-wide count");
                found = true;
            }
        }
        assert!(found, "the JS budget keys are the ones Rust builds");

        // Rust's cooldown stops a JS lookup by the same member before any request.
        let other = format!("u{}", bot_db::new_cuid());
        let ok = common::search_flows::CannedFetch::new(|_, _| common::search_flows::ani_body(vec![]));
        s::set_fetch(&t, Arc::new(common::search_flows::Shared(ok)));
        s::search_anilist(&t, MediaType::Anime, &unique("rust first"), false, Some(&other), None).await.expect("ok");
        let back = run_node(JS, &["anilist", &unique("js second"), "0", &empty, &other, ""]);
        assert_eq!((back["code"].clone(), back["calls"].clone()), (json!("COOLDOWN"), json!(0)), "{back}");
    }

    #[tokio::test]
    async fn urban_and_pokemon_entries_cross_both_ways() {
        let Some(t) = redis_app().await else { return };
        let term = unique("yeet");
        let list = json!({ "list": [common::search_flows::yeet()] }).to_string();
        let js = run_node(JS, &["urban", &term, &list]);
        s::set_fetch(&t, Arc::new(common::search_flows::Shared(common::search_flows::CannedFetch::new(|_, _| panic!("a hit never fetches")))));
        let hit = s::define_urban(&t, &term, None, None).await.expect("hit");
        assert!(hit.cached);
        assert_eq!(serde_json::to_value(&hit.results).expect("json"), js["results"]);

        // A Pokémon looked up in Rust is a hit in JS.
        let name = format!("pikachu-{}", bot_db::new_cuid().to_lowercase());
        let routes = vec![("/pokemon-species/pikachu".to_string(), common::search_flows::pikachu_species()), (format!("/pokemon/{name}"), common::search_flows::pikachu())];
        let r2 = routes.clone();
        let fetch = common::search_flows::CannedFetch::new(move |_, url| {
            let path = url.replace(s::POKEAPI_BASE, "");
            match r2.iter().find(|(p, _)| *p == path) {
                Some((_, v)) => common::search_flows::Canned::Json(v.clone()),
                None => common::search_flows::Canned::Err(bot_util::safe_fetch::SafeFetchError::with_status(bot_util::safe_fetch::SafeFetchCode::HttpError, 404)),
            }
        });
        s::set_fetch(&t, Arc::new(common::search_flows::Shared(fetch)));
        let ours = s::get_pokemon(&t, &PokemonQuery::Name(name.clone()), None, None).await.expect("ok");
        assert_eq!(ours.results.len(), 1);
        let back = run_node(JS, &["pokemon", &name, &json!({ "routes": {} }).to_string()]);
        assert_eq!((back["cached"].clone(), back["calls"].clone()), (json!(true), json!(0)), "{back}");
        assert_eq!(back["results"], serde_json::to_value(&ours.results).expect("json"));
    }

    #[tokio::test]
    async fn the_pokemon_index_and_the_pause_are_shared() {
        let Some(t) = redis_app().await else { return };
        // Rust writes the index (the refresh), JS reads it without the network.
        let body = json!({ "results": [{ "name": "bulbasaur", "url": "https://pokeapi.co/api/v2/pokemon-species/1/" }, { "name": "mew", "url": "https://pokeapi.co/api/v2/pokemon-species/151/" }] });
        s::set_fetch(&t, Arc::new(common::search_flows::Shared(common::search_flows::CannedFetch::routes(vec![("/pokemon-species?limit=2000", body)]))));
        t.cache().del_one(s::POKEAPI_INDEX_KEY).await;
        assert_eq!(s::get_pokemon_index(&t).await, None);
        for _ in 0..200 {
            if t.cache().get(s::POKEAPI_INDEX_KEY).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let js = run_node(JS, &["index"]);
        assert_eq!(js["entries"], json!([["bulbasaur", 1], ["mew", 151]]));
        assert_eq!(js["calls"], 0);

    }
}
