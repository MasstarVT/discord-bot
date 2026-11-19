//! Port of `test/role-persist.test.js` (and the role-persist part of `test/sec-moderation.test.js`): the rules against
//! JS-produced fixtures, the leave / rejoin hooks and `/rolepersist` against the real tables with a fake Discord
//! REST, and both directions of the JS round trip (`PersistedMember`, the event-role markers).

mod common;

use std::collections::HashMap;

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, run_node};
use bot_db::GuildSettings;
use common::*;
use feat_events::plugins::role_persist::{AUDIT_REASON, Restore, gate_may_have_cleared, join_filter, leave_filter, restore_member, role_delete_filter};
use feat_events::role_persist_service::{self as rp, Gate, Snapshot, consts as RP};
use serde_json::{Value, json};
use serenity::all::{GuildId, Member, Permissions as P, RoleId, UserId};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

const OWNER: &str = "600000000000000099";
const BOTR: &str = "820000000000000010";
const A: &str = "820000000000000001";
const B: &str = "820000000000000002";
const C: &str = "820000000000000003";
const ADMINR: &str = "820000000000000004";
const HIGH: &str = "820000000000000005";
const MANAGED: &str = "820000000000000006";
const IGN: &str = "820000000000000007";
const CAP: &str = "820000000000000008";
const GONE: &str = "820000000000000099";
const U1: &str = "600000000000000001";

fn role_guild(g: &str, bot: P) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .role(BOTR, 10, bot)
        .role(A, 1, P::empty())
        .role(B, 2, P::empty())
        .role(C, 3, P::empty())
        .role(ADMINR, 4, P::ADMINISTRATOR)
        .role(HIGH, 20, P::empty())
        .role_with(MANAGED, 5, P::empty(), |r| r["managed"] = json!(true))
        .role(IGN, 6, P::empty())
        .role(CAP, 7, P::empty())
        .member(TEST_BOT_ID, &[BOTR])
}

fn member(g: &str, id: &str, roles: &[&str], nick: Option<&str>, pending: bool) -> Member {
    serde_json::from_value(json!({
        "guild_id": g,
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": false },
        "roles": roles, "nick": nick, "pending": pending,
        "joined_at": chrono::Utc::now().to_rfc3339(), "deaf": false, "mute": false, "flags": 0
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

fn settings(f: impl FnOnce(&mut GuildSettings)) -> GuildSettings {
    let mut s = GuildSettings { guild_id: "1".into(), ..Default::default() };
    f(&mut s);
    s
}

fn guild_of(app: &bot_core::App, g: &str) -> serenity::all::Guild {
    bot_core::interaction_helpers::cached_guild(app, GuildId::new(g.parse().unwrap_or(1))).expect("cached guild")
}

fn bot() -> UserId {
    UserId::new(TEST_BOT_ID.parse().unwrap_or(1))
}

// ── Pure rules ────────────────────────────────────────────────────────────────

#[test]
fn rules_match_the_js() {
    let g = golden();
    let r = &g["rolePersist"];
    let c = &r["constants"];
    assert_eq!((c["DEFAULT_DAYS"].as_i64(), c["MIN_DAYS"].as_i64(), c["MAX_DAYS"].as_i64()), (Some(RP::DEFAULT_DAYS), Some(RP::MIN_DAYS), Some(RP::MAX_DAYS)));
    assert_eq!(c["WAIT_MAX"], json!(RP::WAIT_MAX));
    assert_eq!(c["NICKNAME_MAX"], json!(RP::NICKNAME_MAX));
    assert_eq!(c["EVENT_MARKERS_MAX"], json!(RP::EVENT_MARKERS_MAX));
    for v in r["retention"].as_array().into_iter().flatten() {
        assert_eq!(json!(rp::retention_days_of(v.get("days"))), v["out"], "{v}");
    }
    for v in r["merge"].as_array().into_iter().flatten() {
        let snap = |x: &Value| Snapshot { role_ids: x["roleIds"].as_array().into_iter().flatten().map(s).collect(), nickname: x["nickname"].as_str().map(String::from) };
        let old = (!v["old"].is_null()).then(|| snap(&v["old"]));
        let out = rp::merge_snapshot(old.as_ref(), &snap(&v["snap"]));
        assert_eq!(json!({ "roleIds": out.role_ids, "nickname": out.nickname }), v["out"]);
    }
    for v in r["skipped"].as_array().into_iter().flatten() {
        let st = &v["settings"];
        let gs = settings(|x| {
            x.role_persist_ignored_role_ids = st["rolePersistIgnoredRoleIds"].as_array().into_iter().flatten().map(s).collect();
            x.voice_role_any_role_ids = st["voiceRoleAnyRoleIds"].as_array().into_iter().flatten().map(s).collect();
            x.voice_role_links = st.get("voiceRoleLinks").cloned().unwrap_or(json!([]));
        });
        assert_eq!(json!(rp::skipped_role_ids(Some(&gs))), v["out"], "{v}");
    }
    for v in r["owned"].as_array().into_iter().flatten() {
        let st = &v["settings"];
        let gs = settings(|x| {
            x.birthday_role_id = st["birthdayRoleId"].as_str().map(String::from);
            x.mute_role_id = st["muteRoleId"].as_str().map(String::from);
            x.mod_enabled = st["modEnabled"] == json!(true);
        });
        let got: Vec<Value> = rp::module_role_reasons(Some(&gs)).into_iter().map(|(a, b)| json!([a, b])).collect();
        assert_eq!(Value::Array(got), v["out"], "{v}");
    }
    for v in r["expiresAt"].as_array().into_iter().flatten() {
        let gs = settings(|x| x.role_persist_days = v["days"].as_i64().unwrap_or(90) as i32);
        let left = chrono::DateTime::parse_from_rfc3339(&s(&v["leftAt"])).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_default();
        assert_eq!(json!(rp::expires_at(&left, Some(&gs))), v["out"]);
    }
    // `retention: days are clamped to 1..365 and rows expire after them`.
    assert_eq!(rp::retention_days(Some(&settings(|x| x.role_persist_days = 30))), 30);
    assert_eq!(rp::retention_days(Some(&settings(|x| x.role_persist_days = 0))), RP::MIN_DAYS);
    assert_eq!(rp::retention_days(Some(&settings(|x| x.role_persist_days = 9999))), RP::MAX_DAYS);
    assert_eq!(rp::retention_days(None), RP::DEFAULT_DAYS);
    let now = 1_792_454_400_000_i64;
    let ten = settings(|x| x.role_persist_days = 10);
    let at = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
    assert!(!rp::is_stale(&at(now - 9 * RP::DAY_MS), Some(&ten), now));
    assert!(rp::is_stale(&at(now - 10 * RP::DAY_MS), Some(&ten), now));
}

#[tokio::test]
async fn snapshot_and_plan_rules() {
    let g = "940000000000000001";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[role_guild(g, P::MANAGE_ROLES)]);
    let guild = guild_of(&t, g);
    let ign = settings(|x| x.role_persist_ignored_role_ids = vec![IGN.into()]);
    // Skips @everyone, managed, deleted and ignored roles; keeps the (trimmed) nickname.
    let m = member(g, U1, &[A, B, MANAGED, IGN, GONE, g], Some("  Nick  "), false);
    let snap = rp::snapshot_member(Some(&m), &guild, Some(&ign)).expect("snapshot");
    assert_eq!(snap.role_ids, [A, B]);
    assert_eq!(snap.nickname.as_deref(), Some("Nick"));
    // Voice Roles roles are never saved or given back.
    let voice = settings(|x| {
        x.voice_role_links = json!([{ "channelId": "VC1", "roleId": A }]);
        x.voice_role_any_role_ids = vec![B.into()];
    });
    assert!(rp::snapshot_member(Some(&member(g, U1, &[A, B], None, false)), &guild, Some(&voice)).is_none());
    assert!(rp::plan_roles(&guild, bot(), &[A.into(), B.into()], Some(&voice), None, None).give.is_empty());
    // An uncached member, or nothing to give back, saves nothing; a nickname alone is worth saving.
    assert!(rp::snapshot_member(None, &guild, None).is_none());
    assert!(rp::snapshot_member(Some(&member(g, U1, &[], None, false)), &guild, None).is_none());
    assert!(rp::snapshot_member(Some(&member(g, U1, &[MANAGED], None, false)), &guild, None).is_none());
    assert_eq!(rp::snapshot_member(Some(&member(g, U1, &[], Some("Solo"), false)), &guild, None), Some(Snapshot { role_ids: vec![], nickname: Some("Solo".into()) }));
    let long = "x".repeat(40);
    assert_eq!(rp::snapshot_member(Some(&member(g, U1, &[], Some(&long), false)), &guild, None).and_then(|s| s.nickname).map(|n| n.len()), Some(32));

    // plan: gives only roles that exist, are allowed and safe.
    let st = settings(|x| {
        x.role_persist_ignored_role_ids = vec![IGN.into()];
        x.captcha_enabled = true;
        x.captcha_role_id = Some(CAP.into());
    });
    let ids: Vec<String> = [A, B, ADMINR, HIGH, MANAGED, IGN, CAP, GONE, g, A].iter().map(|x| x.to_string()).collect();
    let held = [RoleId::new(B.parse().unwrap_or(1))];
    let plan = rp::plan_roles(&guild, bot(), &ids, Some(&st), Some(&held), None);
    assert_eq!(plan.give, [RoleId::new(A.parse().unwrap_or(1))]);
    let why: HashMap<String, String> = plan.skipped.iter().map(|x| (x.role_id.clone(), x.reason.clone())).collect();
    let mut keys: Vec<&String> = why.keys().collect();
    keys.sort();
    let mut want = vec![ADMINR, HIGH, MANAGED, IGN, CAP, GONE];
    want.sort();
    assert_eq!(keys, want);
    assert!(why[ADMINR].contains("dangerous permissions: Administrator"));
    assert!(why[HIGH].contains("above my highest role"));
    assert!(why[MANAGED].contains("managed"));
    assert!(why[IGN].contains("never-give-back"));
    assert!(why[CAP].contains("captcha"));
    assert!(plan.skipped.iter().any(|x| x.role_id == GONE && x.deleted));
    // Lifecycle skips (an ended event's role) come in through `skips`.
    let skips: HashMap<String, String> = [(A.to_string(), "The event this role was for is over.".to_string())].into();
    let p2 = rp::plan_roles(&guild, bot(), &[A.into(), C.into()], None, None, Some(&skips));
    assert_eq!(p2.give, [RoleId::new(C.parse().unwrap_or(1))]);
    assert_eq!(p2.skipped[0].reason, "The event this role was for is over.");
}

#[tokio::test]
async fn every_dangerous_permission_keeps_a_role_from_being_given_back() {
    let g = "940000000000000002";
    let perms = [P::ADMINISTRATOR, P::MANAGE_GUILD, P::MANAGE_ROLES, P::MANAGE_CHANNELS, P::BAN_MEMBERS, P::KICK_MEMBERS, P::MODERATE_MEMBERS, P::MANAGE_WEBHOOKS, P::MENTION_EVERYONE, P::MANAGE_MESSAGES];
    let mut fg = FakeGuild::new(g, OWNER).role(BOTR, 50, P::MANAGE_ROLES).member(TEST_BOT_ID, &[BOTR]);
    let mut ids = Vec::new();
    for (i, p) in perms.iter().enumerate() {
        let id = format!("83000000000000000{i}");
        fg = fg.role(&id, (i + 1) as u16, *p);
        ids.push(id);
    }
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[fg]);
    let plan = rp::plan_roles(&guild_of(&t, g), bot(), &ids, None, None, None);
    assert!(plan.give.is_empty());
    assert_eq!(plan.skipped.len(), perms.len());
}

#[tokio::test]
async fn gates_rules_screening_and_the_captcha_role() {
    let g = "940000000000000003";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[role_guild(g, P::MANAGE_ROLES)]);
    let guild = guild_of(&t, g);
    let captcha = settings(|x| {
        x.captcha_enabled = true;
        x.captcha_role_id = Some(CAP.into());
    });
    let plain = settings(|_| {});
    assert_eq!(rp::gate_of(Some(&guild), &member(g, U1, &[], None, true), Some(&plain), false), Some(Gate::Pending));
    assert_eq!(rp::gate_of(Some(&guild), &member(g, U1, &[], None, false), Some(&captcha), false), Some(Gate::Captcha));
    assert_eq!(rp::gate_of(Some(&guild), &member(g, U1, &[CAP], None, false), Some(&captcha), false), None, "verified");
    assert_eq!(rp::gate_of(Some(&guild), &member(g, U1, &[], None, false), Some(&plain), false), None);
    let deleted = settings(|x| {
        x.captcha_enabled = true;
        x.captcha_role_id = Some(GONE.into());
    });
    assert_eq!(rp::gate_of(Some(&guild), &member(g, U1, &[], None, false), Some(&deleted), false), None, "a deleted captcha role never gates");
    assert_eq!(rp::gate_of(Some(&guild), &member(g, U1, &[], None, false), Some(&plain), true), Some(Gate::Captcha));
    // The plan never lists the captcha role while captcha is on.
    let plan = rp::plan_roles(&guild, bot(), &[CAP.into(), A.into()], Some(&captcha), None, None);
    assert_eq!(plan.give, [RoleId::new(A.parse().unwrap_or(1))]);
}

#[tokio::test]
async fn waiting_set_is_bounded_and_expiring() {
    let t = TestApp::new();
    let now = 1_800_000_000_000_i64;
    rp::mark_waiting(&t, "GW", "U1", now);
    assert!(rp::is_waiting(&t, "GW", "U1", now + 1));
    assert!(!rp::is_waiting(&t, "GW", "U1", now + RP::WAIT_TTL_MS + 1));
    for i in 0..RP::WAIT_MAX + 10 {
        rp::mark_waiting(&t, "GW", &format!("M{i}"), now);
    }
    assert!(!rp::is_waiting(&t, "GW", "M0", now), "the oldest entry went");
    assert!(rp::is_waiting(&t, "GW", &format!("M{}", RP::WAIT_MAX + 9), now));
    assert_eq!(rp::waiting_len(&t), RP::WAIT_MAX);
    rp::reset_waiting(&t);
    assert_eq!(rp::waiting_len(&t), 0);
}

#[tokio::test]
async fn hook_filters() {
    let g = "940000000000000004";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[role_guild(g, P::MANAGE_ROLES)]);
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let user = |bot: bool| -> serenity::all::User {
        serde_json::from_value(json!({ "id": U1, "username": "u", "discriminator": "0", "avatar": null, "bot": bot })).expect("user")
    };
    let ctx_remove = |bot: bool| bot_core::HookCtx::new(t.app.clone(), GuildMemberRemove { guild_id: gid, member: None, user: user(bot) });
    assert!(leave_filter(&ctx_remove(false)));
    assert!(!leave_filter(&ctx_remove(true)), "bots are skipped");
    let mut bot_member = member(g, U1, &[], None, false);
    bot_member.user.bot = true;
    assert!(!join_filter(&bot_core::HookCtx::new(t.app.clone(), GuildMemberAdd { guild_id: gid, member: bot_member, user: user(true) })));

    // update filter: only gate-clearing updates are looked at (the settings memoised, as any hook would).
    t.settings().update_settings(g, SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", CAP)).await.expect("settings");
    let upd = |old: Member, new: Member, known: bool| {
        bot_core::HookCtx::new(t.app.clone(), GuildMemberUpdate { guild_id: gid, old_member: Some(old), member: new, old_known: known })
    };
    assert!(!gate_may_have_cleared(&upd(member(g, U1, &[A], None, false), member(g, U1, &[A, B], None, false), true)));
    assert!(gate_may_have_cleared(&upd(member(g, U1, &[A], None, false), member(g, U1, &[A, CAP], None, false), true)), "the captcha role gained");
    assert!(gate_may_have_cleared(&upd(member(g, U1, &[], None, true), member(g, U1, &[], None, false), true)), "Rules Screening passed");
    rp::mark_waiting(&t, g, U1, t.clock().now_ms());
    assert!(gate_may_have_cleared(&upd(member(g, U1, &[A], None, false), member(g, U1, &[A], None, false), true)), "a member we wait for");
    rp::clear_waiting(&t, g, U1);
    // An uncached old member: recent joiners only, once a minute.
    let recent = member(g, "600000000000000002", &[], None, false);
    assert!(gate_may_have_cleared(&upd(recent.clone(), recent.clone(), false)));
    assert!(!gate_may_have_cleared(&upd(recent.clone(), recent, false)), "throttled");
    let mut old: Member = member(g, "600000000000000003", &[], None, false);
    old.joined_at = Some(serenity::all::Timestamp::from_unix_timestamp(1_600_000_000).expect("ts"));
    assert!(!gate_may_have_cleared(&upd(old.clone(), old, false)), "joined long ago");
    let pending = member(g, "600000000000000004", &[], None, true);
    assert!(!gate_may_have_cleared(&upd(pending.clone(), pending, false)), "still pending");

    // roleDelete filter: no memo, the handler decides; else only roles on the ignore list.
    let rd = |gg: &str, r: &str| bot_core::HookCtx::new(t.app.clone(), RoleDelete { guild_id: GuildId::new(gg.parse().unwrap_or(1)), role_id: RoleId::new(r.parse().unwrap_or(1)), role: None });
    assert!(role_delete_filter(&rd("940000000000000005", A)), "no memo yet");
    t.settings().update_settings(g, SettingsPatch::new().set("rolePersistIgnoredRoleIds", json!([A, B]))).await.expect("settings");
    assert!(!role_delete_filter(&rd(g, C)));
    assert!(role_delete_filter(&rd(g, A)));
}

// ── Database: leave, rejoin, the command ──────────────────────────────────────

struct Env {
    t: DbTestApp,
    rest: FakeRest,
    g: &'static str,
}

impl Env {
    async fn new(g: &'static str, bot: P) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([g]);
        t.reset_guilds(&[g]).await;
        rp::reset_prune_stamps(&t);
        let rest = FakeRest::new();
        let gg = g.to_string();
        rest.on_with("PATCH", "/guilds/*/members/*", move |call| {
            let id = call.path.rsplit('/').next().unwrap_or_default().to_string();
            (200, json!({
                "guild_id": gg, "user": { "id": id, "username": "u", "discriminator": "0", "avatar": null }, "roles": [],
                "nick": call.body["nick"], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
            }))
        });
        t.set_discord(rest.discord(&[role_guild(g, bot).json()]));
        t.install(registry());
        t.settings().update_settings(g, SettingsPatch::new().set("rolePersistEnabled", true)).await.expect("settings");
        Some(Self { t, rest, g })
    }

    fn gid(&self) -> GuildId {
        GuildId::new(self.g.parse().unwrap_or(1))
    }

    async fn settings(&self, patch: SettingsPatch) -> GuildSettings {
        let s = self.t.settings().update_settings(self.g, patch).await.expect("settings");
        (*s).clone()
    }

    async fn save(&self, user: &str, roles: &[&str], nick: Option<&str>, days_ago: i64) {
        let now = chrono::Utc::now() - chrono::Duration::milliseconds(days_ago * RP::DAY_MS);
        let snap = Snapshot { role_ids: roles.iter().map(|r| r.to_string()).collect(), nickname: nick.map(String::from) };
        rp::save_row(&self.t, self.g, user, &snap, now, None).await.expect("save");
    }

    async fn row(&self, user: &str) -> Option<bot_db::PersistedMember> {
        rp::load_row(&self.t, self.g, user).await.expect("load")
    }

    fn adds(&self, user: &str) -> Vec<String> {
        self.rest.calls_to("PUT", &format!("/guilds/{}/members/{user}/roles/*", self.g)).iter().map(|c| c.path.rsplit('/').next().unwrap_or_default().to_string()).collect()
    }

    async fn restore(&self, m: &Member, st: &GuildSettings, captcha_pending: bool) -> Restore {
        restore_member(&self.t, self.gid(), m, st, captcha_pending, None).await
    }

    async fn leave(&self, m: Member) {
        let user = m.user.clone();
        self.t.run_hooks(GuildMemberRemove { guild_id: self.gid(), member: Some(m), user }).await;
    }

    async fn join(&self, m: Member) {
        let user = m.user.clone();
        self.t.run_hooks(GuildMemberAdd { guild_id: self.gid(), member: m, user }).await;
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn bot_ok() -> P {
    P::MANAGE_ROLES | P::MANAGE_NICKNAMES
}

#[tokio::test]
async fn leave_saves_roles_and_nickname_and_merges_into_a_waiting_row() {
    let Some(env) = Env::new("940000000000000010", bot_ok()).await else { return };
    let g = env.g;
    env.settings(SettingsPatch::new().set("rolePersistIgnoredRoleIds", json!([IGN]))).await;
    env.leave(member(g, U1, &[A, B, IGN, MANAGED], Some("Nick"), false)).await;
    let row = env.row(U1).await.expect("row");
    assert_eq!(row.role_ids, [A, B]);
    assert_eq!(row.nickname.as_deref(), Some("Nick"));

    // A member who leaves again while the restore still waits keeps the roles saved the first time.
    env.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", CAP).set("rolePersistIgnoredRoleIds", json!([]))).await;
    env.save("600000000000000002", &[A, B, C], Some("Old"), 3).await;
    let first = env.row("600000000000000002").await.expect("row").left_at;
    env.join(member(g, "600000000000000002", &[], None, false)).await;
    assert!(rp::is_waiting(&env.t, g, "600000000000000002", env.t.clock().now_ms()), "the captcha holds the restore");
    env.leave(member(g, "600000000000000002", &[ADMINR, B], None, false)).await;
    let row = env.row("600000000000000002").await.expect("row");
    assert_eq!(row.role_ids, [A, B, C, ADMINR], "old ids first, new ids added, no duplicates");
    assert_eq!(row.nickname.as_deref(), Some("Old"), "no nickname known now: the old one stays");
    assert!(row.left_at > first, "leftAt is refreshed");
    assert!(!rp::is_waiting(&env.t, g, "600000000000000002", env.t.clock().now_ms()), "leaving ends the wait");
    // A later rejoin that clears the gate gets all of them back (the plan still drops what is unsafe).
    let st = env.settings(SettingsPatch::new()).await;
    let res = env.restore(&member(g, "600000000000000002", &[CAP], None, false), &st, false).await;
    match res {
        Restore::Restored { given, skipped, .. } => {
            assert_eq!(given, [A, B, C]);
            assert_eq!(skipped.iter().map(|x| x.role_id.as_str()).collect::<Vec<_>>(), [ADMINR]);
        }
        other => panic!("{other:?}"),
    }

    // A newer nickname replaces the older one; an expired row is replaced, not merged.
    env.save("600000000000000003", &[A], Some("Old"), 0).await;
    env.leave(member(g, "600000000000000003", &[B], Some("Newer"), false)).await;
    let row = env.row("600000000000000003").await.expect("row");
    assert_eq!((row.role_ids.clone(), row.nickname.as_deref()), (vec![A.to_string(), B.to_string()], Some("Newer")));
    env.save("600000000000000004", &[A, B], Some("Old"), 91).await;
    env.leave(member(g, "600000000000000004", &[C], None, false)).await;
    let row = env.row("600000000000000004").await.expect("row");
    assert_eq!((row.role_ids.clone(), row.nickname.clone()), (vec![C.to_string()], None));
    // An uncached member, or one with nothing to save, keeps the older row.
    env.save("600000000000000005", &[A], Some("Keep"), 1).await;
    let user: serenity::all::User =
        serde_json::from_value(json!({ "id": "600000000000000005", "username": "u", "discriminator": "0", "avatar": null })).expect("user");
    env.t.run_hooks(GuildMemberRemove { guild_id: env.gid(), member: None, user }).await;
    env.leave(member(g, "600000000000000005", &[], None, false)).await;
    assert_eq!(env.row("600000000000000005").await.expect("row").nickname.as_deref(), Some("Keep"));
    env.done().await;
}

#[tokio::test]
async fn prune_deletes_expired_rows_at_most_once_an_hour() {
    let Some(env) = Env::new("940000000000000011", bot_ok()).await else { return };
    let st = env.settings(SettingsPatch::new().set("rolePersistDays", 30)).await;
    env.save("600000000000000021", &[A], None, 31).await;
    env.save("600000000000000022", &[A], None, 1).await;
    let now = env.t.clock().now_ms();
    assert_eq!(rp::prune_expired(&env.t, env.g, Some(&st), now).await, Some(1));
    assert!(env.row("600000000000000021").await.is_none());
    assert!(env.row("600000000000000022").await.is_some());
    env.save("600000000000000023", &[A], None, 40).await;
    assert_eq!(rp::prune_expired(&env.t, env.g, Some(&st), now + 1000).await, None, "within the hour: skipped");
    assert!(env.row("600000000000000023").await.is_some());
    assert_eq!(rp::prune_expired(&env.t, env.g, Some(&st), now + RP::PRUNE_EVERY_MS).await, Some(1));
    assert_eq!(rp::count_rows(&env.t, env.g).await.expect("count"), 1);
    env.done().await;
}

#[tokio::test]
async fn rejoin_gives_the_safe_roles_one_call_each_then_deletes_the_row() {
    let Some(env) = Env::new("940000000000000012", bot_ok()).await else { return };
    let g = env.g;
    env.settings(SettingsPatch::new().set("rolePersistIgnoredRoleIds", json!([IGN]))).await;
    env.save(U1, &[A, B, ADMINR, HIGH, MANAGED, IGN, GONE], Some("Nick"), 1).await;
    env.join(member(g, U1, &[], None, false)).await;
    assert_eq!(env.adds(U1), [A, B], "one role id per call");
    let puts = env.rest.calls_to("PUT", &format!("/guilds/{g}/members/{U1}/roles/*"));
    assert!(puts.iter().all(|c| c.reason.as_deref() == Some(AUDIT_REASON)));
    let nick = env.rest.calls_to("PATCH", &format!("/guilds/{g}/members/{U1}"));
    assert_eq!(nick[0].body["nick"], "Nick");
    assert_eq!(nick[0].reason.as_deref(), Some(AUDIT_REASON));
    assert!(env.row(U1).await.is_none(), "the row is deleted after the restore");

    // Roles already held are skipped; an empty plan still clears the row.
    env.save("600000000000000002", &[A], None, 1).await;
    let st = env.settings(SettingsPatch::new()).await;
    let res = env.restore(&member(g, "600000000000000002", &[A], None, false), &st, false).await;
    assert_eq!(res.status(), "restored");
    assert!(env.adds("600000000000000002").is_empty());
    assert!(env.row("600000000000000002").await.is_none());
    // A stale row is deleted and ignored; no row means nothing.
    env.save("600000000000000003", &[A], Some("Old"), 91).await;
    assert_eq!(env.restore(&member(g, "600000000000000003", &[], None, false), &st, false).await, Restore::Stale);
    assert!(env.row("600000000000000003").await.is_none());
    assert_eq!(env.restore(&member(g, "600000000000000004", &[], None, false), &st, false).await, Restore::None);
    // A concurrent join and update restore once.
    env.save("600000000000000005", &[C], None, 1).await;
    let m = member(g, "600000000000000005", &[], None, false);
    let (x, y) = tokio::join!(env.restore(&m, &st, false), env.restore(&m, &st, false));
    assert_eq!([x.status(), y.status()].iter().filter(|s| **s == "restored").count(), 1);
    assert_eq!(env.adds("600000000000000005"), [C]);
    env.done().await;
}

#[tokio::test]
async fn rejoin_failures_keep_the_row_and_one_unusable_role_does_not_stop_the_others() {
    let Some(env) = Env::new("940000000000000013", bot_ok()).await else { return };
    let g = env.g;
    let st = env.settings(SettingsPatch::new()).await;
    env.save(U1, &[A, B], None, 1).await;
    env.rest.fail("PUT", "/guilds/*/members/*/roles/*", 500, 0, "Internal Server Error");
    assert_eq!(env.restore(&member(g, U1, &[], None, false), &st, false).await, Restore::Failed);
    assert!(env.row(U1).await.is_some(), "kept for a later attempt");
    env.rest.clear_scripts();
    env.rest.fail("PUT", &format!("/guilds/*/members/*/roles/{A}"), 403, 50013, "Missing Permissions");
    match env.restore(&member(g, U1, &[], None, false), &st, false).await {
        Restore::Restored { given, .. } => assert_eq!(given, [B]),
        other => panic!("{other:?}"),
    }
    assert!(env.row(U1).await.is_none());
    // Unknown Role is skipped the same way.
    env.rest.clear_scripts();
    env.save(U1, &[A, B, C], None, 1).await;
    env.rest.fail("PUT", &format!("/guilds/*/members/*/roles/{B}"), 404, 10011, "Unknown Role");
    match env.restore(&member(g, U1, &[], None, false), &st, false).await {
        Restore::Restored { given, .. } => assert_eq!(given, [A, C]),
        other => panic!("{other:?}"),
    }
    // The member leaving during the restore stops it.
    env.rest.clear_scripts();
    env.rest.clear_calls();
    env.save(U1, &[A, B], None, 1).await;
    env.rest.fail("PUT", "/guilds/*/members/*/roles/*", 404, 10007, "Unknown Member");
    assert_eq!(env.restore(&member(g, U1, &[], None, false), &st, false).await, Restore::Left);
    assert_eq!(env.adds(U1), [A], "stops at the first call that says they are gone");
    assert!(env.row(U1).await.is_some());
    env.done().await;

    // Without Manage Roles nothing is given and the row is kept.
    let Some(env) = Env::new("940000000000000014", P::MANAGE_NICKNAMES).await else { return };
    let st = env.settings(SettingsPatch::new()).await;
    env.save(U1, &[A, B], Some("Nick"), 1).await;
    assert_eq!(env.restore(&member(env.g, U1, &[], None, false), &st, false).await, Restore::Blocked);
    assert!(env.row(U1).await.is_some());
    assert!(env.rest.calls().is_empty());
    env.done().await;
}

#[tokio::test]
async fn nicknames_are_given_back_only_when_on_free_and_manageable() {
    let Some(env) = Env::new("940000000000000015", bot_ok()).await else { return };
    let g = env.g;
    let on = env.settings(SettingsPatch::new()).await;
    let nick_of = |env: &Env, u: &str| env.rest.calls_to("PATCH", &format!("/guilds/{}/members/{u}", env.g)).len();
    env.save("600000000000000031", &[], Some("Nick"), 1).await;
    assert!(matches!(env.restore(&member(g, "600000000000000031", &[], None, false), &on, false).await, Restore::Restored { nickname: true, .. }));
    assert_eq!(nick_of(&env, "600000000000000031"), 1);
    let off = env.settings(SettingsPatch::new().set("rolePersistNicknames", false)).await;
    env.save("600000000000000032", &[], Some("Nick"), 1).await;
    env.restore(&member(g, "600000000000000032", &[], None, false), &off, false).await;
    assert_eq!(nick_of(&env, "600000000000000032"), 0);
    env.save("600000000000000033", &[], Some("Nick"), 1).await;
    env.restore(&member(g, "600000000000000033", &[], Some("Chosen"), false), &on, false).await;
    assert_eq!(nick_of(&env, "600000000000000033"), 0, "a nickname they already set is kept");
    env.save("600000000000000034", &[], Some("Nick"), 1).await;
    env.restore(&member(g, "600000000000000034", &[HIGH], None, false), &on, false).await;
    assert_eq!(nick_of(&env, "600000000000000034"), 0, "not manageable");
    assert!(env.row("600000000000000034").await.is_none(), "the row still clears");
    env.done().await;
    let Some(env) = Env::new("940000000000000016", P::MANAGE_ROLES).await else { return };
    let on = env.settings(SettingsPatch::new()).await;
    env.save(U1, &[], Some("Nick"), 1).await;
    env.restore(&member(env.g, U1, &[], None, false), &on, false).await;
    assert_eq!(nick_of(&env, U1), 0, "no Manage Nicknames");
    env.done().await;
}

#[tokio::test]
async fn gates_hold_the_restore_until_rules_screening_or_the_captcha_clears() {
    let Some(env) = Env::new("940000000000000017", bot_ok()).await else { return };
    let g = env.g;
    env.save(U1, &[A, B], Some("Nick"), 1).await;
    env.join(member(g, U1, &[], None, true)).await;
    assert!(env.adds(U1).is_empty());
    assert!(env.row(U1).await.is_some());
    assert!(rp::is_waiting(&env.t, g, U1, env.t.clock().now_ms()));
    env.t
        .run_hooks(GuildMemberUpdate { guild_id: env.gid(), old_member: Some(member(g, U1, &[], None, true)), member: member(g, U1, &[], None, false), old_known: true })
        .await;
    assert_eq!(env.adds(U1), [A, B]);
    assert!(env.row(U1).await.is_none());
    assert!(!rp::is_waiting(&env.t, g, U1, env.t.clock().now_ms()));

    // The captcha: never bypassed; the roles come once the captcha role is held, and the captcha role is never given.
    env.settings(SettingsPatch::new().set("captchaEnabled", true).set("captchaRoleId", CAP)).await;
    let u2 = "600000000000000002";
    env.save(u2, &[A, CAP, B], None, 1).await;
    env.join(member(g, u2, &[], None, false)).await;
    assert!(env.adds(u2).is_empty(), "unverified: no roles");
    env.t.run_hooks(GuildMemberUpdate { guild_id: env.gid(), old_member: Some(member(g, u2, &[], None, false)), member: member(g, u2, &[CAP], None, false), old_known: true }).await;
    let mut got = env.adds(u2);
    got.sort();
    assert_eq!(got, [A, B]);
    // The join hook's flag gates even before the role check.
    let u3 = "600000000000000003";
    env.save(u3, &[A], None, 1).await;
    let st = env.settings(SettingsPatch::new().set("captchaEnabled", false)).await;
    assert_eq!(env.restore(&member(g, u3, &[], None, false), &st, true).await, Restore::Waiting(Gate::Captcha));
    assert!(env.row(u3).await.is_some());
    env.done().await;
}

#[tokio::test]
async fn role_delete_takes_the_role_off_the_ignore_list() {
    let Some(env) = Env::new("940000000000000018", bot_ok()).await else { return };
    env.settings(SettingsPatch::new().set("rolePersistIgnoredRoleIds", json!([A, B]))).await;
    let rd = |r: &str| RoleDelete { guild_id: env.gid(), role_id: RoleId::new(r.parse().unwrap_or(1)), role: None };
    env.t.run_hooks(rd(C)).await;
    assert_eq!(env.t.settings().get_settings(env.g).await.expect("s").role_persist_ignored_role_ids, [A, B]);
    env.t.run_hooks(rd(A)).await;
    assert_eq!(env.t.settings().get_settings(env.g).await.expect("s").role_persist_ignored_role_ids, [B]);
    env.done().await;
}

#[tokio::test]
async fn lifecycle_skips_an_ended_events_role_and_an_ended_mute() {
    let Some(env) = Env::new("940000000000000019", bot_ok()).await else { return };
    let g = env.g;
    let st = env.settings(SettingsPatch::new().set("muteRoleId", C).set("modEnabled", false)).await;
    // An event that gave U1 the role A, now over; another (open) one that gave B and still runs.
    let ended = feat_events::event_service::NewEvent {
        title: "Over".into(),
        description: None,
        location: None,
        starts_at: chrono::Utc::now() + chrono::Duration::days(1),
        ends_at: None,
        host_id: "600000000000000099".into(),
        role_id: Some(A.into()),
        capacity: None,
        reminder_minutes: 0,
    };
    let mk = |role: &str| feat_events::event_service::NewEvent { role_id: Some(role.into()), ..ended.clone() };
    let e1 = match feat_events::event_service::create_event_row(&env.t, g, &mk(A), "810000000000000001", None).await.expect("row") {
        feat_events::event_service::CreateRow::Created(e) => *e,
        other => panic!("{other:?}"),
    };
    let e2 = match feat_events::event_service::create_event_row(&env.t, g, &mk(B), "810000000000000001", None).await.expect("row") {
        feat_events::event_service::CreateRow::Created(e) => *e,
        other => panic!("{other:?}"),
    };
    for e in [&e1, &e2] {
        feat_events::event_service::apply_rsvp(&env.t, &e.id, g, U1, bot_db::RsvpStatus::Going).await.expect("rsvp");
        feat_events::event_service::mark_role_granted(&env.t, &e.id, U1, true).await.expect("flag");
    }
    feat_events::event_service::close_event(&env.t, &e1.id, bot_db::GuildEventStatus::Ended, feat_events::event_service::CloseReason::Manual, None)
        .await
        .expect("closed");
    let ids: Vec<String> = [A, B, C].iter().map(|x| x.to_string()).collect();
    let now = chrono::Utc::now();
    let out = rp::lifecycle_skips(&env.t, g, U1, &ids, Some(&st), now).await.expect("skips");
    assert_eq!(out.skips.get(A).map(String::as_str), Some("The event this role was for is over."));
    assert!(!out.skips.contains_key(B), "the open event still holds its grant");
    assert_eq!(out.skips.get(C).map(String::as_str), Some("The mute has ended."));
    assert_eq!(out.markers, std::slice::from_ref(&e1.id));
    // The JS reads the same data the same way.
    let js = run_node(
        "rust/fixtures/gen/feat-events-js.mjs",
        &["rpSkips", g, U1, &serde_json::to_string(&ids).unwrap_or_default(), &json!({ "muteRoleId": C, "modEnabled": false }).to_string()],
    );
    let mut rust_skips = serde_json::Map::new();
    for (k, v) in &out.skips {
        rust_skips.insert(k.clone(), json!(v));
    }
    assert_eq!(js, json!({ "skips": rust_skips, "markers": out.markers }));
    // An active mute keeps the mute role.
    let muted = run_node("rust/fixtures/gen/feat-events-js.mjs", &["mute", g, U1, "-"]);
    assert!(muted.get("error").is_none(), "{muted}");
    let out2 = rp::lifecycle_skips(&env.t, g, U1, &ids, Some(&st), now).await.expect("skips");
    assert!(!out2.skips.contains_key(C));
    // A restore skips the event role and clears the marker.
    env.save(U1, &[A, B], None, 1).await;
    match env.restore(&member(g, U1, &[], None, false), &st, false).await {
        Restore::Restored { given, skipped, .. } => {
            assert_eq!(given, [B]);
            assert_eq!(skipped[0].reason, "The event this role was for is over.");
        }
        other => panic!("{other:?}"),
    }
    let flag: (bool,) = sqlx::query_as(r#"SELECT "roleGranted" FROM "EventRsvp" WHERE "eventId" = $1 AND "userId" = $2"#)
        .bind(&e1.id)
        .bind(U1)
        .fetch_one(env.t.db())
        .await
        .expect("row");
    assert!(!flag.0, "the marker is cleared once the restore acted on it");
    env.done().await;
}

#[tokio::test]
async fn js_and_rust_read_each_others_saved_members() {
    let Some(env) = Env::new("940000000000000020", bot_ok()).await else { return };
    let g = env.g;
    // JS writes, Rust reads and restores.
    let js = run_node("rust/fixtures/gen/feat-events-js.mjs", &["rpSave", g, U1, &json!({ "roleIds": [A, B], "nickname": "Nick" }).to_string(), "2026-11-01T10:00:00.000Z"]);
    assert!(js.get("error").is_none(), "{js}");
    let row = env.row(U1).await.expect("row");
    assert_eq!(to_json(&row), js, "the row reads the same");
    // Rust writes (merging into the JS row), JS reads.
    rp::save_row(&env.t, g, U1, &Snapshot { role_ids: vec![C.into()], nickname: None }, chrono::Utc::now(), None).await.expect("save");
    let back = run_node("rust/fixtures/gen/feat-events-js.mjs", &["rpLoad", g, U1]);
    assert_eq!(back["roleIds"], json!([A, B, C]));
    assert_eq!(back["nickname"], "Nick");
    assert_eq!(to_json(&env.row(U1).await.expect("row")), back);
    let st = env.settings(SettingsPatch::new()).await;
    match env.restore(&member(g, U1, &[], None, false), &st, false).await {
        Restore::Restored { given, nickname, .. } => {
            assert_eq!(given, [A, B, C]);
            assert!(nickname);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(run_node("rust/fixtures/gen/feat-events-js.mjs", &["rpLoad", g, U1]), Value::Null);
    env.done().await;
}

async fn admin(env: &Env, f: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    env.t.responses.clear();
    let i = f.guild(env.g).user(OWNER).channel("810000000000000001").build();
    bot_core::router::route_interaction(&env.t, i).await;
    env.t.responses.calls()
}

fn text_of(call: &bot_core::interaction::RecordedCall) -> String {
    let e = &call.body["embeds"][0];
    let mut parts = vec![s(&e["title"]), s(&e["description"])];
    for f in e["fields"].as_array().into_iter().flatten() {
        parts.push(s(&f["name"]));
        parts.push(s(&f["value"]));
    }
    parts.push(s(&e["footer"]["text"]));
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn rolepersist_command_status_ignore_view_clear() {
    use bot_core::command::option_type as ot;
    let Some(env) = Env::new("940000000000000021", P::MANAGE_ROLES).await else { return };
    let g = env.g;
    env.settings(SettingsPatch::new().set("rolePersistDays", 30).set("rolePersistIgnoredRoleIds", json!([A]))).await;
    env.save("600000000000000041", &[A], None, 1).await;
    env.save("600000000000000042", &[B], None, 1).await;
    let calls = admin(&env, FakeInteraction::slash("rolepersist").subcommand("status")).await;
    assert_eq!(calls[0].method, "deferReply");
    assert!(!calls[0].ephemeral(), "admin config commands answer publicly");
    let text = text_of(&calls[1]);
    assert!(text.contains("✅ On"), "{text}");
    assert!(text.contains("30 days"));
    assert!(text.contains("Saved members\n2"));
    assert!(text.contains(&format!("<@&{A}>")));
    assert!(text.contains("Manage Nicknames"), "a missing permission is reported");
    env.settings(SettingsPatch::new().set("rolePersistEnabled", false)).await;
    let text = text_of(&admin(&env, FakeInteraction::slash("rolepersist").subcommand("status")).await[1]);
    assert!(text.contains("/module enable rolepersist"));

    // ignore add / remove
    env.settings(SettingsPatch::new().set("rolePersistEnabled", true).set("rolePersistIgnoredRoleIds", json!([]))).await;
    let run = |sub: &'static str, role: &'static str| FakeInteraction::slash("rolepersist").group("ignore", sub).option("role", ot::ROLE, json!(role));
    assert!(text_of(&admin(&env, run("add", A)).await[1]).contains("Role Ignored"));
    assert_eq!(env.t.settings().get_settings(g).await.expect("s").role_persist_ignored_role_ids, [A]);
    assert!(text_of(&admin(&env, run("add", A)).await[1]).contains("already on the never-give-back list"));
    let everyone: &'static str = "940000000000000021";
    assert!(text_of(&admin(&env, run("add", everyone)).await[1]).contains("@everyone"));
    assert!(text_of(&admin(&env, run("add", MANAGED)).await[1]).contains("managed by an integration"));
    assert!(text_of(&admin(&env, run("remove", A)).await[1]).contains("Role No Longer Ignored"));
    assert!(env.t.settings().get_settings(g).await.expect("s").role_persist_ignored_role_ids.is_empty());
    assert!(text_of(&admin(&env, run("remove", A)).await[1]).contains("is not on the never-give-back list"));
    let full: Vec<String> = (0..bot_config::limits::ROLEPERSIST_IGNORED_ROLES).map(|i| format!("85000000000000{i:04}")).collect();
    env.settings(SettingsPatch::new().set("rolePersistIgnoredRoleIds", json!(full))).await;
    assert!(text_of(&admin(&env, run("add", B)).await[1]).contains("List Full"));
    env.settings(SettingsPatch::new().set("rolePersistIgnoredRoleIds", json!([])).set("rolePersistEnabled", false)).await;
    assert!(text_of(&admin(&env, run("add", A)).await[1]).contains("/module enable rolepersist"), "tells the admin the module is off");

    // view
    env.settings(SettingsPatch::new().set("rolePersistDays", 90).set("rolePersistIgnoredRoleIds", json!([IGN]))).await;
    env.save(U1, &[A, HIGH, IGN, GONE], Some("Nick"), 1).await;
    let view = |u: &'static str| FakeInteraction::slash("rolepersist").subcommand("view").option("user", ot::USER, json!(u));
    let text = text_of(&admin(&env, view(U1)).await[1]);
    for want in [
        format!("<@{U1}>"),
        format!("✅ <@&{A}>"),
        format!("⚠️ <@&{HIGH}> — That role is above my highest role."),
        format!("⚠️ <@&{IGN}> — The role is on the never-give-back list."),
        format!("deleted role (`{GONE}`)"),
        "`Nick`".to_string(),
        "Expires".to_string(),
    ] {
        assert!(text.contains(&want), "{want} in {text}");
    }
    assert!(text_of(&admin(&env, view("600000000000000077")).await[1]).contains("Nothing is saved for <@600000000000000077>"));
    env.save("600000000000000078", &[A], None, 200).await;
    assert!(text_of(&admin(&env, view("600000000000000078")).await[1]).contains("Expired"));

    // clear: forgets a member and stops a waiting restore
    rp::mark_waiting(&env.t, g, U1, env.t.clock().now_ms());
    let clear = || FakeInteraction::slash("rolepersist").subcommand("clear").option("user", ot::USER, json!(U1));
    assert!(text_of(&admin(&env, clear()).await[1]).contains("will not get their old roles back"));
    assert!(env.row(U1).await.is_none());
    assert!(!rp::is_waiting(&env.t, g, U1, env.t.clock().now_ms()));
    assert!(text_of(&admin(&env, clear()).await[1]).contains(&format!("Nothing was saved for <@{U1}>")));
    env.done().await;
}
