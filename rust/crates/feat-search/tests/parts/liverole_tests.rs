//! The live-role engine, hooks and `/liverole` (ported from `test/liverole.test.js`), against a fake Discord.

use std::sync::Arc;

use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, offline_discord_as};
use feat_search::ops;
use feat_search::plugins::liverole::{self as live, AnnounceOutcome, Mode, Outcome, Source, SyncFailure, SyncResult};
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions as P, RoleId, UserId};

use crate::common::*;

pub const G: &str = "940000000000000001";
pub const OWNER: &str = "640000000000000099";
pub const BOTR: &str = "840000000000000010";
pub const LIVE: &str = "840000000000000002";
pub const REQ: &str = "840000000000000001";
pub const DANGER: &str = "840000000000000003";
pub const HIGH: &str = "840000000000000020";
pub const PING: &str = "840000000000000004";
pub const QUIET: &str = "840000000000000005";
pub const ADMINR: &str = "840000000000000030";
pub const S1: &str = "640000000000000001";
pub const S2: &str = "640000000000000002";
pub const H1: &str = "640000000000000003";
pub const BOTM: &str = "640000000000000004";
pub const ADMIN: &str = "640000000000000009";
pub const ANN: &str = "840000000000000100";
pub const LOCKED: &str = "840000000000000101";

fn gid() -> GuildId {
    GuildId::new(G.parse().unwrap())
}

fn uid(s: &str) -> UserId {
    UserId::new(s.parse().unwrap())
}

/// The guild: the bot's role above the live role, two streamers (one holding the role), a holder who is not live, a
/// bot holding the role, and an admin.
pub fn guild_json() -> Value {
    let fg = FakeGuild::new(G, OWNER)
        .role(REQ, 1, P::empty())
        .role(LIVE, 2, P::empty())
        .role(DANGER, 3, P::ADMINISTRATOR)
        .role_with(PING, 4, P::empty(), |r| r["mentionable"] = json!(true))
        .role(QUIET, 6, P::empty())
        .role(BOTR, 10, P::MANAGE_ROLES | P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS)
        .role(HIGH, 20, P::empty())
        .role(ADMINR, 30, P::MANAGE_GUILD)
        .member(TEST_BOT_ID, &[BOTR])
        .member(S1, &[])
        .member(S2, &[LIVE])
        .member(H1, &[LIVE])
        .member(ADMIN, &[ADMINR])
        .channel(ANN, 0, "live-now", &[])
        .channel(LOCKED, 0, "locked", &[(BOTR, false, P::empty(), P::SEND_MESSAGES)]);
    let mut g = fg.json();
    g["members"].as_array_mut().unwrap().push(member_json(BOTM, &[LIVE], None, true));
    g["member_count"] = json!(6);
    let stream = streaming();
    g["presences"] = json!([
        { "user": { "id": S1 }, "status": "online", "activities": stream, "client_status": {} },
        { "user": { "id": S2 }, "status": "online", "activities": stream, "client_status": {} },
        { "user": { "id": H1 }, "status": "online", "activities": [], "client_status": {} },
    ]);
    g
}

pub struct Live {
    pub t: TestApp,
    pub d: Arc<FakeDiscord>,
}

pub async fn live_app(patch: SettingsPatch) -> Live {
    live_app_on(TestApp::new(), patch).await
}

/// [`live_app`] on a given test app (one whose job store fails, say).
pub async fn live_app_on(t: TestApp, patch: SettingsPatch) -> Live {
    let t = t.with_guilds([G]);
    let d = FakeDiscord::new(&[guild_json()]);
    ops::set_ops(&t, Arc::new(FakeOps(d.clone())));
    ops::set_presences_enabled(&t, true);
    t.set_discord(offline_discord_as(&[guild_json()]));
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("liveRoleEnabled", true).set("liveRoleId", LIVE)).await.expect("settings");
    if !patch.is_empty() {
        t.settings().update_settings(G, patch).await.expect("settings");
    }
    Live { t, d }
}

impl Live {
    pub async fn settings(&self) -> Settings {
        self.t.settings().get_settings(G).await.expect("settings")
    }

    pub async fn reconcile(&self, user: &str, mode: Mode, source: Source) -> Outcome {
        let s = self.settings().await;
        live::reconcile_member(&self.t, gid(), uid(user), mode, s, source).await
    }

    pub fn role_calls(&self) -> Vec<Call> {
        self.d.calls().into_iter().filter(|c| matches!(c, Call::AddRole { .. } | Call::RemoveRole { .. })).collect()
    }
}

fn add(user: &str, reason: &str) -> Call {
    Call::AddRole { user: user.into(), role: LIVE.into(), reason: reason.into() }
}

fn remove(user: &str, reason: &str) -> Call {
    Call::RemoveRole { user: user.into(), role: LIVE.into(), reason: reason.into() }
}

// ── reconcileMember ──────────────────────────────────────────────────────────

#[tokio::test]
async fn reconcile_adds_to_a_streamer_by_id_and_is_idempotent() {
    let l = live_app(SettingsPatch::new()).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::Added);
    assert_eq!(l.role_calls(), vec![add(S1, live::reasons::PRESENCE_ADD)]);
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::None);
    assert_eq!(l.role_calls().len(), 1);
}

#[tokio::test]
async fn reconcile_removes_from_a_holder_who_is_not_streaming_and_uses_the_right_reasons() {
    let l = live_app(SettingsPatch::new()).await;
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Presence).await, Outcome::Pending, "a presence stop waits for its re-check");
    assert!(l.role_calls().is_empty());
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Recheck).await, Outcome::Removed);
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Recheck).await, Outcome::None);
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Presence).await, Outcome::None);
    assert_eq!(l.role_calls(), vec![remove(H1, live::reasons::PRESENCE_REMOVE)]);
    l.d.clear();
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Sync).await, Outcome::Added);
    assert_eq!(l.reconcile(S2, Mode::Clear, Source::Sync).await, Outcome::Removed);
    assert_eq!(l.role_calls(), vec![add(S1, live::reasons::SYNC_ADD), remove(S2, live::reasons::CLEAR)]);
    l.d.set_presence(G, S1, json!([]));
    l.d.clear();
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Sync).await, Outcome::Removed);
    assert_eq!(l.role_calls(), vec![remove(S1, live::reasons::SYNC_REMOVE)]);
}

#[tokio::test]
async fn a_dangerous_role_is_never_added_but_can_be_removed() {
    let l = live_app(SettingsPatch::new().set("liveRoleId", DANGER)).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::Blocked);
    assert!(l.role_calls().is_empty());
    l.d.edit(G, |g| g.members.get_mut(&uid(H1)).unwrap().roles.push(RoleId::new(DANGER.parse().unwrap())));
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Recheck).await, Outcome::Removed);
}

#[tokio::test]
async fn a_live_role_above_the_bot_blocks_both_directions_without_a_rest_call() {
    let l = live_app(SettingsPatch::new().set("liveRoleId", HIGH)).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::Blocked);
    l.d.edit(G, |g| g.members.get_mut(&uid(H1)).unwrap().roles.push(RoleId::new(HIGH.parse().unwrap())));
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Recheck).await, Outcome::Blocked);
    assert!(l.role_calls().is_empty());
}

#[tokio::test]
async fn discord_errors_are_classified_and_never_thrown() {
    let l = live_app(SettingsPatch::new()).await;
    for (code, want) in [(10007, Outcome::Gone), (10011, Outcome::RoleMissing), (50013, Outcome::Failed), (50001, Outcome::Failed), (500, Outcome::Failed)] {
        l.d.fail_next("add", code);
        assert_eq!(l.reconcile(S1, Mode::Sync, Source::Sync).await, want, "code {code}");
    }
}

#[tokio::test]
async fn bots_missing_members_and_missing_roles() {
    let l = live_app(SettingsPatch::new()).await;
    l.d.set_presence(G, BOTM, json!([]));
    assert_eq!(l.reconcile(BOTM, Mode::Sync, Source::Sync).await, Outcome::None, "a bot is never touched");
    assert_eq!(l.reconcile("640000000000000077", Mode::Sync, Source::Sync).await, Outcome::Gone);
    assert!(l.d.calls().contains(&Call::FetchMember { user: "640000000000000077".into() }));
    let l = live_app(SettingsPatch::new().set("liveRoleId", "840000000000000777")).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Sync).await, Outcome::RoleMissing);
}

#[tokio::test]
async fn an_uncached_member_is_fetched_before_any_role_logic() {
    let l = live_app(SettingsPatch::new()).await;
    let remote = "640000000000000055";
    l.d.remote_members.lock().unwrap().insert(uid(remote), member(G, remote, &[], None));
    l.d.set_presence(G, remote, streaming());
    assert_eq!(l.reconcile(remote, Mode::Sync, Source::Sync).await, Outcome::Added);
    assert_eq!(l.d.calls()[0], Call::FetchMember { user: remote.into() });
}

#[tokio::test]
async fn a_partial_member_from_a_presence_is_fetched_before_any_role_logic() {
    // The cache holds H1 as a partial member (built from a presence: no roles). The real member holds the live role and
    // is not streaming, so it is removed. Reading the partial member would see no role and do nothing.
    let l = live_app(SettingsPatch::new()).await;
    l.d.edit(G, |g| g.members.insert(uid(H1), partial_member(G, H1)));
    l.d.remote_members.lock().unwrap().insert(uid(H1), member(G, H1, &[LIVE], None));
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Recheck).await, Outcome::Removed);
    assert_eq!(l.d.calls()[0], Call::FetchMember { user: H1.into() });
    assert_eq!(l.role_calls(), vec![remove(H1, live::reasons::PRESENCE_REMOVE)]);
}

#[tokio::test]
async fn a_sync_checks_partial_streamers_one_by_one_and_never_counts_them_as_eligible() {
    // S1 streams but is cached only as a partial member; with a required role, the real member has it.
    let l = live_app(SettingsPatch::new().set("liveRoleRequiredRoleId", REQ)).await;
    l.d.edit(G, |g| g.members.insert(uid(S1), partial_member(G, S1)));
    l.d.remote_members.lock().unwrap().insert(uid(S1), member(G, S1, &[REQ], None));
    let r = live::sync_guild(&l.t, gid(), Mode::Sync, "test").await;
    assert!(matches!(r, SyncResult::Done(_)), "{r:?}");
    assert!(l.role_calls().contains(&add(S1, live::reasons::SYNC_ADD)), "{:?}", l.role_calls());
}

#[tokio::test]
async fn the_required_role_gates_adds_and_a_stop_removes_regardless() {
    let l = live_app(SettingsPatch::new().set("liveRoleRequiredRoleId", REQ)).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::None, "streaming without the required role");
    assert_eq!(l.reconcile(S2, Mode::Sync, Source::Presence).await, Outcome::Removed, "holds it but does not qualify");
    l.d.edit(G, |g| g.members.get_mut(&uid(S1)).unwrap().roles.push(RoleId::new(REQ.parse().unwrap())));
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::Added);
    l.d.set_presence(G, S1, json!([]));
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::Pending, "it still qualifies, so the stop waits");
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Recheck).await, Outcome::Removed);
    // A deleted required role fails closed.
    let l = live_app(SettingsPatch::new().set("liveRoleRequiredRoleId", "840000000000000888")).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::None);
}

#[tokio::test]
async fn an_unavailable_guild_is_left_alone() {
    let l = live_app(SettingsPatch::new()).await;
    l.d.guilds.lock().unwrap().clear();
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Presence).await, Outcome::None);
    assert_eq!(live::sync_guild(&l.t, gid(), Mode::Sync, "test").await, SyncResult::Failed { code: SyncFailure::Error, reason: None });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flapping_presence_is_coalesced_and_converges() {
    let l = live_app(SettingsPatch::new()).await;
    *l.d.edit_delay_ms.lock().unwrap() = 40;
    let s = l.settings().await;
    let mut futs = Vec::new();
    for i in 0..6 {
        if i % 2 == 1 {
            l.d.set_presence(G, S1, json!([]));
        } else {
            l.d.set_presence(G, S1, streaming());
        }
        futs.push(live::reconcile_member(&l.t, gid(), uid(S1), Mode::Sync, s.clone(), Source::Presence));
    }
    // The last presence is "not streaming" (i = 5).
    let outs = futures::future::join_all(futs).await;
    assert!(l.role_calls().len() <= 2, "at most one running and one waiting: {:?}", l.role_calls());
    assert!(outs.iter().filter(|o| **o == Outcome::None).count() >= 4);
    l.reconcile(S1, Mode::Sync, Source::Recheck).await; // a pending stop would be confirmed here
    assert!(!l.d.roles_of(G, S1).contains(&LIVE.to_string()), "converged on the newest presence");
}

#[tokio::test]
async fn sync_sourced_calls_are_never_coalesced() {
    let l = live_app(SettingsPatch::new()).await;
    let s = l.settings().await;
    let (a, b) = tokio::join!(
        live::reconcile_member(&l.t, gid(), uid(S1), Mode::Sync, s.clone(), Source::Sync),
        live::reconcile_member(&l.t, gid(), uid(S1), Mode::Sync, s.clone(), Source::Sync),
    );
    assert_eq!((a, b), (Outcome::Added, Outcome::None), "both ran, in order");
}

#[tokio::test]
async fn presence_decisions_made_on_stale_settings_are_dropped() {
    let l = live_app(SettingsPatch::new()).await;
    let stale = l.settings().await;
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleEnabled", false)).await.expect("settings");
    assert_eq!(live::reconcile_member(&l.t, gid(), uid(S1), Mode::Sync, stale.clone(), Source::Presence).await, Outcome::None);
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleEnabled", true).set("liveRoleId", PING)).await.expect("settings");
    assert_eq!(live::reconcile_member(&l.t, gid(), uid(S1), Mode::Sync, stale, Source::Presence).await, Outcome::None);
    assert!(l.role_calls().is_empty());
}

// ── Whole-guild ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn count_losing_holders_and_load_members() {
    let l = live_app(SettingsPatch::new()).await;
    let s = l.settings().await;
    let n = l.d.edit(G, |g| live::count_losing_holders(g, RoleId::new(LIVE.parse().unwrap()), &s));
    assert_eq!(n, 1, "H1 (not streaming); the bot and the streamer are not counted");
    // Complete cache: no fetch.
    assert!(live::load_members(&l.t, gid()).await);
    assert!(l.d.calls().is_empty());
    // An incomplete cache: one full fetch per guild per interval.
    l.d.edit(G, |g| g.member_count = 10);
    assert!(live::load_members(&l.t, gid()).await);
    assert!(!live::load_members(&l.t, gid()).await, "throttled");
    assert_eq!(l.d.calls(), vec![Call::FetchAll { guild: G.into() }]);
    // Too large: the cache only.
    l.d.edit(G, |g| g.member_count = 30_000);
    let l2 = Live { t: TestApp::new().with_guilds([G]), d: l.d.clone() };
    ops::set_ops(&l2.t, Arc::new(FakeOps(l.d.clone())));
    assert!(!live::load_members(&l2.t, gid()).await);
}

#[tokio::test]
async fn sync_adds_first_removes_non_streamers_and_reports() {
    let l = live_app(SettingsPatch::new()).await;
    let r = live::sync_guild(&l.t, gid(), Mode::Sync, "test").await;
    let SyncResult::Done(d) = r else { panic!("{r:?}") };
    assert_eq!((d.added, d.removed, d.failed, d.deferred, d.complete), (1, 1, 0, 0, true));
    assert_eq!(l.role_calls(), vec![add(S1, live::reasons::SYNC_ADD), remove(H1, live::reasons::SYNC_REMOVE)]);
    assert!(l.d.roles_of(G, BOTM).contains(&LIVE.to_string()), "bots keep it");
    // Clear removes it from every non-bot holder, streaming ones too.
    l.d.clear();
    let SyncResult::Done(c) = live::sync_guild(&l.t, gid(), Mode::Clear, "disable").await else { panic!() };
    assert_eq!((c.mode, c.removed), (Mode::Clear, 2));
    assert_eq!(live::format_sync_result(Some(&SyncResult::Done(c))), "Removed the live role from **2** member(s).");
}

#[tokio::test]
async fn sync_caps_the_edits_and_honours_the_required_role() {
    let l = live_app(SettingsPatch::new()).await;
    l.d.edit(G, |g| {
        for i in 0..60u64 {
            let id = 650_000_000_000_000_000 + i;
            let m = member(G, &id.to_string(), &[], None);
            g.members.insert(UserId::new(id), m);
        }
        g.member_count = g.members.len() as u64;
    });
    for i in 0..60u64 {
        l.d.set_presence(G, &(650_000_000_000_000_000 + i).to_string(), streaming());
    }
    let SyncResult::Done(d) = live::sync_guild(&l.t, gid(), Mode::Sync, "test").await else { panic!() };
    assert_eq!(d.added, 50);
    assert_eq!(d.deferred, 61 + 1 - 50, "61 streamers to add and one holder to remove, 50 per sync");
    assert!(l.role_calls().iter().all(|c| matches!(c, Call::AddRole { .. })), "adds first");

    let l = live_app(SettingsPatch::new().set("liveRoleRequiredRoleId", REQ)).await;
    let SyncResult::Done(d) = live::sync_guild(&l.t, gid(), Mode::Sync, "test").await else { panic!() };
    assert_eq!((d.added, d.removed), (0, 2), "nobody qualifies: S2 and H1 lose it");
}

#[tokio::test]
async fn sync_result_codes_and_dangerous_roles_block_adds_only() {
    let l = live_app(SettingsPatch::new().set("liveRoleId", "840000000000000777")).await;
    assert_eq!(live::sync_guild(&l.t, gid(), Mode::Sync, "t").await, SyncResult::Failed { code: SyncFailure::RoleMissing, reason: None });
    let l = live_app(SettingsPatch::new().set("liveRoleId", HIGH)).await;
    assert_eq!(
        live::sync_guild(&l.t, gid(), Mode::Sync, "t").await,
        SyncResult::Failed { code: SyncFailure::CannotManage, reason: Some("That role is above my highest role.".into()) }
    );
    let l = live_app(SettingsPatch::new().set("liveRoleEnabled", false)).await;
    assert_eq!(live::sync_guild(&l.t, gid(), Mode::Sync, "t").await, SyncResult::Failed { code: SyncFailure::Off, reason: None });
    let l = live_app(SettingsPatch::new().set("liveRoleId", Value::Null)).await;
    assert_eq!(live::sync_guild(&l.t, gid(), Mode::Clear, "t").await, SyncResult::Failed { code: SyncFailure::NoRole, reason: None });

    let l = live_app(SettingsPatch::new().set("liveRoleId", DANGER)).await;
    l.d.edit(G, |g| g.members.get_mut(&uid(H1)).unwrap().roles.push(RoleId::new(DANGER.parse().unwrap())));
    let SyncResult::Done(d) = live::sync_guild(&l.t, gid(), Mode::Sync, "t").await else { panic!() };
    assert_eq!((d.added, d.removed), (0, 1));
    assert_eq!(d.adds_blocked.as_deref(), Some("That role has dangerous permissions: Administrator."));
}

#[tokio::test]
async fn sync_fetches_a_streaming_user_missing_from_the_member_cache() {
    let l = live_app(SettingsPatch::new()).await;
    let remote = "640000000000000066";
    l.d.set_presence(G, remote, streaming());
    l.d.remote_members.lock().unwrap().insert(uid(remote), member(G, remote, &[], None));
    let SyncResult::Done(d) = live::sync_guild(&l.t, gid(), Mode::Sync, "t").await else { panic!() };
    assert_eq!(d.added, 2);
    assert!(l.d.calls().contains(&Call::FetchMember { user: remote.into() }));
}

#[tokio::test]
async fn sync_reports_unreachable_settings() {
    let l = live_app(SettingsPatch::new()).await;
    l.t.settings().invalidate_settings(G).await;
    l.t.settings_store.set_available(false);
    assert_eq!(live::sync_guild(&l.t, gid(), Mode::Sync, "t").await, SyncResult::Failed { code: SyncFailure::Settings, reason: None });
}

#[tokio::test]
async fn without_presences_nothing_is_stripped_but_clear_still_works() {
    let l = live_app(SettingsPatch::new()).await;
    ops::set_presences_enabled(&l.t, false);
    assert_eq!(live::sync_guild(&l.t, gid(), Mode::Sync, "t").await, SyncResult::Failed { code: SyncFailure::PresenceUnavailable, reason: None });
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Sync).await, Outcome::None);
    let SyncResult::Done(d) = live::sync_guild(&l.t, gid(), Mode::Clear, "disable").await else { panic!() };
    assert_eq!(d.removed, 2);
}

// ── Go-live posts ────────────────────────────────────────────────────────────

#[tokio::test]
async fn announce_posts_the_stream_link_without_pinging_once_per_cooldown() {
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", ANN)).await;
    let s = l.settings().await;
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &s).await, AnnounceOutcome::Sent);
    assert_eq!(
        l.d.calls(),
        vec![Call::Send { channel: ANN.into(), content: format!("🔴 <@{S1}> is now live on Twitch!\nhttps://www.twitch.tv/someone"), roles: vec![] }]
    );
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &s).await, AnnounceOutcome::Cooldown);
}

#[tokio::test]
async fn announce_uses_the_custom_message_and_pings_only_a_pingable_role() {
    let l = live_app(
        SettingsPatch::new()
            .set("liveRoleAnnounceChannelId", ANN)
            .set("liveRoleAnnounceMessage", "  {platform}: {title} / {game} {url}  ")
            .set("liveRoleAnnouncePingRoleId", PING),
    )
    .await;
    let s = l.settings().await;
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &s).await, AnnounceOutcome::Sent);
    assert_eq!(
        l.d.calls(),
        vec![Call::Send { channel: ANN.into(), content: format!("<@&{PING}> Twitch: Ranked / Valorant https://www.twitch.tv/someone"), roles: vec![PING.into()] }]
    );
    // A role the bot may not mention gets the prefix but no ping.
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", ANN).set("liveRoleAnnouncePingRoleId", QUIET)).await;
    let s = l.settings().await;
    live::announce_live(&l.t, gid(), uid(S2), &s).await;
    let Call::Send { content, roles, .. } = l.d.calls()[0].clone() else { panic!() };
    assert!(content.starts_with(&format!("<@&{QUIET}> ")));
    assert!(roles.is_empty());
    // A deleted ping role: no prefix.
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", ANN).set("liveRoleAnnouncePingRoleId", "840000000000000999")).await;
    let s = l.settings().await;
    live::announce_live(&l.t, gid(), uid(S2), &s).await;
    let Call::Send { content, .. } = l.d.calls()[0].clone() else { panic!() };
    assert!(content.starts_with("🔴"));
}

#[tokio::test]
async fn announce_does_nothing_without_a_usable_channel_or_a_live_stream_and_never_fails() {
    let l = live_app(SettingsPatch::new()).await;
    let s = l.settings().await;
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &s).await, AnnounceOutcome::Off);
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", LOCKED)).await;
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &*l.settings().await).await, AnnounceOutcome::Unusable);
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", "840000000000000999")).await;
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &*l.settings().await).await, AnnounceOutcome::Unusable);
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", ANN)).await;
    assert_eq!(live::announce_live(&l.t, gid(), uid(H1), &*l.settings().await).await, AnnounceOutcome::NotStreaming);
    l.d.fail_next("send", 50013);
    assert_eq!(live::announce_live(&l.t, gid(), uid(S1), &*l.settings().await).await, AnnounceOutcome::Failed);
    assert!(l.d.calls().iter().all(|c| matches!(c, Call::Send { .. })));
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

fn presence_update(user: &str, old: Option<Value>, new: Value) -> PresenceUpdate {
    let p = |acts: Value| -> serenity::all::Presence {
        serde_json::from_value(json!({ "user": { "id": user }, "status": "online", "activities": acts, "client_status": {} })).expect("presence")
    };
    PresenceUpdate { guild_id: gid(), old_presence: old.map(p), new_presence: p(new), member: member(G, user, &[], None) }
}

#[tokio::test]
async fn the_presence_hook_reconciles_posts_after_an_add_and_role_syncs_never_post() {
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", ANN)).await;
    l.t.run_hooks(presence_update(S1, None, streaming())).await;
    let calls = l.d.calls();
    assert_eq!(calls[0], add(S1, live::reasons::PRESENCE_ADD));
    assert!(matches!(calls.get(1), Some(Call::Send { .. })), "{calls:?}");
    // Not a change of streaming state: filtered out.
    l.d.clear();
    l.t.run_hooks(presence_update(S2, Some(streaming()), streaming())).await;
    assert!(l.d.calls().is_empty());
    // A sync adds without posting.
    let l = live_app(SettingsPatch::new().set("liveRoleAnnounceChannelId", ANN)).await;
    live::sync_guild(&l.t, gid(), Mode::Sync, "t").await;
    assert!(!l.d.calls().iter().any(|c| matches!(c, Call::Send { .. })));
}

#[tokio::test]
async fn hooks_do_nothing_without_a_live_role() {
    let l = live_app(SettingsPatch::new().set("liveRoleId", Value::Null)).await;
    l.t.run_hooks(presence_update(S1, None, streaming())).await;
    l.t.run_hooks(GuildReady { guild_id: gid(), reason: GuildReadyReason::Startup }).await;
    live::startup_syncs_idle(&l.t).await;
    assert!(l.d.calls().is_empty());
}

#[tokio::test]
async fn the_member_update_hook_rechecks_on_a_required_role_change() {
    let l = live_app(SettingsPatch::new().set("liveRoleRequiredRoleId", REQ)).await;
    let old = member(G, S1, &[], None);
    let new = member(G, S1, &[REQ], None);
    l.d.edit(G, |g| g.members.insert(uid(S1), new.clone()));
    l.t.run_hooks(GuildMemberUpdate { guild_id: gid(), old_member: Some(old.clone()), member: new.clone(), old_known: true }).await;
    assert_eq!(l.role_calls(), vec![add(S1, live::reasons::SYNC_ADD)]);
    // Unknown old member: never diffed.
    l.d.clear();
    l.d.edit(G, |g| g.members.insert(uid(S1), old.clone()));
    l.t.run_hooks(GuildMemberUpdate { guild_id: gid(), old_member: None, member: old, old_known: false }).await;
    assert!(l.d.calls().is_empty());
}

#[tokio::test]
async fn guild_ready_queues_the_startup_sync_once_and_not_without_presences() {
    let l = live_app(SettingsPatch::new()).await;
    l.t.run_hooks(GuildReady { guild_id: gid(), reason: GuildReadyReason::Startup }).await;
    l.t.run_hooks(GuildReady { guild_id: gid(), reason: GuildReadyReason::Available }).await;
    live::startup_syncs_idle(&l.t).await;
    assert_eq!(l.role_calls(), vec![add(S1, live::reasons::SYNC_ADD), remove(H1, live::reasons::SYNC_REMOVE)]);

    let l = live_app(SettingsPatch::new()).await;
    ops::set_presences_enabled(&l.t, false);
    l.t.run_hooks(GuildReady { guild_id: gid(), reason: GuildReadyReason::Join }).await;
    live::startup_syncs_idle(&l.t).await;
    assert!(l.d.calls().is_empty());
}

// ── /liverole ────────────────────────────────────────────────────────────────

fn admin(sub: &str) -> FakeInteraction {
    FakeInteraction::slash("liverole").guild(G).user(ADMIN).roles(&[ADMINR]).permissions(P::MANAGE_GUILD).channel(ANN).subcommand(sub)
}

async fn run(l: &Live, i: serenity::all::Interaction) -> bot_core::interaction::RecordedCall {
    l.t.responses.clear();
    route_interaction(&l.t, i).await;
    let calls = l.t.responses.calls();
    assert_eq!(calls.first().map(|c| c.method), Some("deferReply"), "{calls:?}");
    assert!(!calls[0].ephemeral(), "admin config commands answer publicly");
    calls.last().cloned().expect("an answer")
}

#[tokio::test]
async fn liverole_set_validates_and_saves_a_dedicated_role() {
    let l = live_app(SettingsPatch::new().set("liveRoleEnabled", false).set("liveRoleId", Value::Null).set("liveRoleRequiredRoleId", REQ)).await;
    let set = |role: &str| admin("set").option("role", option_type::ROLE, json!(role)).build();
    for (role, title) in [(G, "❌  Invalid Role"), (REQ, "❌  Invalid Role"), (DANGER, "❌  Cannot Use That Role"), (HIGH, "❌  Cannot Use That Role")] {
        let a = run(&l, set(role)).await;
        assert_eq!(a.embed_title(), Some(title), "role {role}");
        assert_eq!(a.body["allowed_mentions"]["parse"], json!([]));
    }
    assert_eq!(l.settings().await.live_role_id, None, "nothing written");
    let a = run(&l, set(PING)).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Set"));
    assert_eq!(
        a.embed_description(),
        Some(format!("Members get <@&{PING}> while Discord shows them as **Streaming** (Twitch or YouTube) and they have <@&{REQ}>.\n{}", "ℹ️ The live role is **off**. Turn it on with `/liverole enable`.").as_str())
    );
    assert_eq!(l.settings().await.live_role_id.as_deref(), Some(PING));
    assert!(l.d.calls().is_empty(), "the module stays off and nothing is synced");
    let again = run(&l, set(PING)).await;
    assert_eq!(again.embed_title(), Some("ℹ️  No Change"));
}

#[tokio::test]
async fn liverole_set_refuses_a_populated_role_and_syncs_while_enabled() {
    let l = live_app(SettingsPatch::new()).await;
    // More than 50 members hold PING but are not live.
    l.d.edit(G, |g| {
        for i in 0..51u64 {
            let id = 660_000_000_000_000_000 + i;
            g.members.insert(UserId::new(id), member(G, &id.to_string(), &[PING], None));
        }
        g.member_count = g.members.len() as u64;
    });
    let a = run(&l, admin("set").option("role", option_type::ROLE, json!(PING)).build()).await;
    assert_eq!(a.embed_title(), Some("❌  Use a Dedicated Role"));
    assert!(a.embed_description().unwrap_or("").contains("is held by **51** members who are not live"));

    // A dedicated role while enabled: saved, synced, and the previous role's holders are reported, not touched.
    let l = live_app(SettingsPatch::new()).await;
    let a = run(&l, admin("set").option("role", option_type::ROLE, json!(QUIET)).build()).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Set"), "{a:?}");
    let d = a.embed_description().unwrap_or("").to_string();
    assert!(d.contains("Synced: gave it to **2**, removed it from **0**."), "{d}");
    assert!(d.contains(&format!("ℹ️ **2** member(s) still have the previous live role <@&{LIVE}>.")), "{d}");
    assert!(l.d.roles_of(G, H1).contains(&LIVE.to_string()), "the previous role is never touched");
}

#[tokio::test]
async fn liverole_set_with_a_few_holders_asks_first() {
    let l = live_app(SettingsPatch::new().set("liveRoleEnabled", false)).await;
    l.d.edit(G, |g| g.members.get_mut(&uid(H1)).unwrap().roles.push(RoleId::new(PING.parse().unwrap())));
    let t = l.t.app.clone();
    let task = tokio::spawn(async move {
        route_interaction(&t, admin("set").option("role", option_type::ROLE, json!(PING)).build()).await;
    });
    for _ in 0..200 {
        if !l.t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let prompt = l.t.responses.calls().into_iter().find(|c| c.method == "editReply").expect("the prompt");
    assert_eq!(prompt.embed_title(), Some("⚠️  Use this role as the live role?"));
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or("").to_string();
    route_interaction(&l.t, FakeInteraction::button(&yes).guild(G).user(ADMIN).message_id("1000000000000000100").build()).await;
    task.await.expect("command");
    assert_eq!(l.settings().await.live_role_id.as_deref(), Some(PING));
}

#[tokio::test]
async fn liverole_require_sets_clears_and_rejects_the_live_role() {
    let l = live_app(SettingsPatch::new()).await;
    let a = run(&l, admin("require").option("role", option_type::ROLE, json!(LIVE)).build()).await;
    assert_eq!(a.embed_description(), Some("The required role can't be the live role itself."));
    let a = run(&l, admin("require").option("role", option_type::ROLE, json!(REQ)).build()).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Requirement Updated"));
    assert!(a.embed_description().unwrap_or("").starts_with(&format!("Only members with <@&{REQ}> get the live role.\nSynced: gave it to **0**, removed it from **2**.")));
    let a = run(&l, admin("require").option("role", option_type::ROLE, json!(REQ)).build()).await;
    assert_eq!(a.embed_description(), Some(format!("<@&{REQ}> is already required.").as_str()));
    let a = run(&l, admin("require").build()).await;
    assert!(a.embed_description().unwrap_or("").starts_with("Everyone who streams can get the live role."));
    assert_eq!(l.settings().await.live_role_required_role_id, None);
    let a = run(&l, admin("require").build()).await;
    assert_eq!(a.embed_description(), Some("Everyone who streams already qualifies."));
}

#[tokio::test]
async fn liverole_enable_refuses_without_a_role_then_syncs_and_resyncs() {
    let l = live_app(SettingsPatch::new().set("liveRoleEnabled", false).set("liveRoleId", Value::Null)).await;
    assert_eq!(run(&l, admin("enable").build()).await.embed_title(), Some("❌  No Live Role Set"));
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleId", "840000000000000777")).await.expect("settings");
    assert_eq!(run(&l, admin("enable").build()).await.embed_title(), Some("❌  Live Role Missing"));
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleId", DANGER)).await.expect("settings");
    assert_eq!(run(&l, admin("enable").build()).await.embed_title(), Some("❌  Cannot Use That Role"));
    // QUIET has no holders, so no confirmation.
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleId", QUIET)).await.expect("settings");
    let a = run(&l, admin("enable").build()).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Enabled"));
    assert!(l.settings().await.live_role_enabled);
    assert!(a.embed_description().unwrap_or("").contains("Synced: gave it to **2**"));
    let again = run(&l, admin("enable").build()).await;
    assert_eq!(again.embed_title(), Some("✅  Live Role Already On — Resynced"));
    assert!(again.embed_description().unwrap_or("").contains("Everyone is already in sync."));
}

#[tokio::test]
async fn liverole_disable_removes_the_role_unless_told_not_to() {
    let l = live_app(SettingsPatch::new()).await;
    let a = run(&l, admin("disable").option("remove_roles", option_type::BOOLEAN, json!(false)).build()).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Disabled"));
    assert!(a.embed_description().unwrap_or("").contains(&format!("Members who have <@&{LIVE}> now keep it.")));
    assert!(l.role_calls().is_empty());
    let a = run(&l, admin("disable").build()).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Already Off"));
    assert!(a.embed_description().unwrap_or("").contains("Removed the live role from **2** member(s)."));
    assert!(!l.settings().await.live_role_enabled);
}

#[tokio::test]
async fn liverole_announce_sets_the_channel_resets_the_text_and_turns_posts_off() {
    let l = live_app(SettingsPatch::new()).await;
    let a = run(&l, admin("announce").build()).await;
    assert_eq!(a.embed_title(), Some("ℹ️  No Change"));
    let a = run(&l, admin("announce").option("channel", option_type::CHANNEL, json!(LOCKED)).build()).await;
    assert_eq!(a.embed_title(), Some("❌  Cannot Post There"));
    assert_eq!(a.embed_description(), Some(format!("I need **Send Messages** in <#{LOCKED}>. Pick another channel or fix my permissions there.").as_str()));
    let a = run(&l, admin("announce").option("channel", option_type::CHANNEL, json!(ANN)).build()).await;
    assert_eq!(a.embed_title(), Some("✅  Go-Live Posts Updated"));
    let d = a.embed_description().unwrap_or("").to_string();
    assert!(d.starts_with(&format!("When a member goes live and gets <@&{LIVE}>, I post in <#{ANN}> (at most once an hour per member).")), "{d}");
    assert!(d.ends_with(&format!("**Preview**\n🔴 <@{ADMIN}> is now live on Twitch!\nhttps://twitch.tv/example")), "{d}");
    assert_eq!(l.settings().await.live_role_announce_channel_id.as_deref(), Some(ANN));

    let a = run(&l, admin("announce").option("message", option_type::STRING, json!("{platform} {nope}")).build()).await;
    let d = a.embed_description().unwrap_or("").to_string();
    assert!(d.contains("⚠️ Unknown variables (sent as typed): `{nope}`"), "{d}");
    assert!(d.contains("Tip: add `{url}` to link the stream."), "{d}");
    assert_eq!(l.settings().await.live_role_announce_message.as_deref(), Some("{platform} {nope}"));
    run(&l, admin("announce").option("message", option_type::STRING, json!("Default")).build()).await;
    assert_eq!(l.settings().await.live_role_announce_message, None);

    let a = run(&l, admin("announce").option("ping_role", option_type::ROLE, json!(PING)).build()).await;
    assert!(a.embed_description().unwrap_or("").contains(&format!(" and ping <@&{PING}>")));
    assert_eq!(l.settings().await.live_role_announce_ping_role_id.as_deref(), Some(PING));
    let a = run(&l, admin("announce").option("ping_role", option_type::ROLE, json!(G)).build()).await;
    assert_eq!(a.embed_title(), Some("❌  Invalid Role"));
    run(&l, admin("announce").option("clear_ping_role", option_type::BOOLEAN, json!(true)).build()).await;
    assert_eq!(l.settings().await.live_role_announce_ping_role_id, None);

    let a = run(&l, admin("announce").build()).await;
    assert_eq!(a.embed_title(), Some("✅  Go-Live Posts Off"));
    assert_eq!(l.settings().await.live_role_announce_channel_id, None);
}

#[tokio::test]
async fn liverole_status_is_cache_only_and_flags_problems() {
    let l = live_app(SettingsPatch::new().set("liveRoleRequiredRoleId", "840000000000000888").set("liveRoleAnnounceChannelId", ANN)).await;
    let a = run(&l, admin("status").build()).await;
    assert_eq!(a.embed_title(), Some("🔴 Live Role"));
    let fields: Vec<(String, String)> = a.body["embeds"][0]["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into()))
        .collect();
    assert_eq!(fields[0], ("Status".into(), "✅ On".into()));
    assert_eq!(fields[1], ("Live role".into(), format!("<@&{LIVE}>")));
    assert_eq!(fields[2], ("Required role".into(), "⚠️ Deleted (`840000000000000888`)".into()));
    assert_eq!(fields[3], ("Go-live posts".into(), format!("<#{ANN}> · default message · no ping")));
    assert_eq!(
        fields[4],
        ("Live now".into(), format!("**2** member(s) have the live role · **2** member(s) are streaming\n<@{S2}> · Twitch\n<@{H1}> · not streaming"))
    );
    assert_eq!(fields[5], ("Problems".into(), "⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.".into()));
    assert!(l.d.calls().is_empty());
}

#[tokio::test]
async fn liverole_answers_unavailable_without_presences_except_disable() {
    let l = live_app(SettingsPatch::new()).await;
    ops::set_presences_enabled(&l.t, false);
    for sub in ["set", "require", "enable", "announce", "status"] {
        assert_eq!(run(&l, admin(sub).build()).await.embed_title(), Some("❌  Live Role Unavailable"), "{sub}");
    }
    let a = run(&l, admin("disable").build()).await;
    assert_eq!(a.embed_title(), Some("✅  Live Role Disabled"));
    assert_eq!(l.role_calls().len(), 2, "clear mode still removes it");
}

#[tokio::test]
async fn liverole_settings_unavailable_answers_publicly_without_pinging() {
    let l = live_app(SettingsPatch::new()).await;
    l.t.settings().invalidate_settings(G).await;
    l.t.settings_store.set_available(false);
    for sub in ["require", "enable", "disable", "announce", "status"] {
        l.t.responses.clear();
        route_interaction(&l.t, admin(sub).build()).await;
        let calls = l.t.responses.calls();
        let last = calls.last().cloned().expect("answer");
        assert_eq!(last.embed_title(), Some("❌  Settings Unavailable"), "{sub}: {calls:?}");
        assert_eq!(last.body["allowed_mentions"]["parse"], json!([]));
    }
}

// ── Stream-stop grace: the flap seen live (Removed, then Added in the same minute, while still streaming) ──

fn recheck_job(user: &str) -> Job {
    Job {
        id: "j".into(),
        guild_id: G.into(),
        job_type: live::RECHECK_JOB_TYPE.into(),
        payload: json!({ "userId": user }),
        run_at: chrono::Utc::now(),
        attempts: 1,
        max_attempts: 3,
        dedupe_key: Some(live::recheck_job_key(G, user)),
        version: 0,
    }
}

impl Live {
    async fn run_recheck(&self, payload: Value) -> BotResult<JobOutcome> {
        let mut job = recheck_job(H1);
        job.payload = payload;
        let ctx = JobCtx { app: self.t.app.clone(), guild_id: G.into(), late_by_ms: 0, attempt: 1 };
        live::run_recheck_job(job, ctx).await
    }

    async fn recheck(&self, user: &str) {
        self.run_recheck(json!({ "userId": user })).await.expect("the re-check job");
    }
}

#[test]
fn defers_removal_keeps_its_names_and_grace() {
    assert_eq!(live::RECHECK_JOB_TYPE, "liverole.recheck");
    assert_eq!(live::recheck_job_key("G", "U"), "liverole.recheck:G:U");
    assert_eq!(live::limits::STOP_GRACE_MS, 3 * 60_000);
    assert!(live::defers_removal(Mode::Sync, Source::Presence, false, true));
    assert!(!live::defers_removal(Mode::Sync, Source::Presence, false, false), "lost the required role");
    assert!(!live::defers_removal(Mode::Sync, Source::Sync, false, true));
    assert!(!live::defers_removal(Mode::Sync, Source::Recheck, false, true));
    assert!(!live::defers_removal(Mode::Clear, Source::Presence, false, true));
}

#[tokio::test]
async fn a_stream_stop_keeps_the_role_and_schedules_one_recheck_grace_later() {
    let l = live_app(SettingsPatch::new()).await;
    let due = l.t.app.clock().now() + chrono::Duration::milliseconds(live::limits::STOP_GRACE_MS as i64);
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Presence).await, Outcome::Pending);
    assert!(l.role_calls().is_empty(), "no role call yet");
    assert!(l.d.roles_of(G, H1).contains(&LIVE.to_string()));
    let job = l.t.scheduler().get_job(&live::recheck_job_key(G, H1)).await.expect("the re-check job");
    assert_eq!((job.r#type.as_str(), job.guild_id.as_str(), job.run_at), ("liverole.recheck", G, due));
    assert_eq!(job.payload, json!({ "userId": H1 }));
    // A later stop moves the one job instead of adding another.
    l.t.clock.advance(60_000);
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Presence).await, Outcome::Pending);
    let moved = l.t.scheduler().get_job(&live::recheck_job_key(G, H1)).await.expect("the re-check job");
    assert_eq!(moved.run_at, due + chrono::Duration::milliseconds(60_000));
}

#[tokio::test]
async fn a_flap_leaves_the_role_alone() {
    let l = live_app(SettingsPatch::new()).await;
    l.d.set_presence(G, S2, json!([])); // Discord re-sends the presence without Streaming...
    assert_eq!(l.reconcile(S2, Mode::Sync, Source::Presence).await, Outcome::Pending);
    l.d.set_presence(G, S2, streaming()); // ...and puts it back moments later
    assert_eq!(l.reconcile(S2, Mode::Sync, Source::Presence).await, Outcome::None, "still holds the role, nothing to add");
    l.recheck(S2).await;
    assert!(l.role_calls().is_empty(), "neither removed nor added: {:?}", l.role_calls());
    assert!(l.d.roles_of(G, S2).contains(&LIVE.to_string()));
}

#[tokio::test]
async fn a_real_stop_is_confirmed_by_the_recheck() {
    let l = live_app(SettingsPatch::new()).await;
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Presence).await, Outcome::Pending);
    l.recheck(H1).await;
    assert_eq!(l.role_calls(), vec![remove(H1, "Live role: stopped streaming")]);
    l.recheck(H1).await; // a repeated claim is harmless
    assert_eq!(l.role_calls().len(), 1);
}

#[tokio::test]
async fn the_recheck_never_adds_and_does_nothing_while_the_module_is_off_or_the_payload_is_bad() {
    let l = live_app(SettingsPatch::new()).await;
    assert_eq!(l.reconcile(S1, Mode::Sync, Source::Recheck).await, Outcome::None, "streaming and not holding: nothing to add");
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleEnabled", false)).await.expect("settings");
    l.recheck(H1).await;
    l.t.settings().update_settings(G, SettingsPatch::new().set("liveRoleEnabled", true).set("liveRoleId", Value::Null)).await.expect("settings");
    l.recheck(H1).await;
    assert!(l.role_calls().is_empty());
    for payload in [json!({}), json!({ "userId": "" }), json!({ "userId": 5 })] {
        let e = l.run_recheck(payload).await.expect_err("a bad payload");
        assert!(e.to_string().contains("missing userId"), "{e}");
    }
}

#[tokio::test]
async fn a_recheck_that_cannot_be_scheduled_falls_back_to_removing_the_role_at_once() {
    let (t, failing) = TestApp::with_failing_jobs();
    let l = live_app_on(t, SettingsPatch::new()).await;
    failing.fail_next_schedules(1);
    assert_eq!(l.reconcile(H1, Mode::Sync, Source::Presence).await, Outcome::Removed);
    assert_eq!(l.role_calls(), vec![remove(H1, "Live role: stopped streaming")]);
    assert_eq!(failing.failures(), 1);
}
