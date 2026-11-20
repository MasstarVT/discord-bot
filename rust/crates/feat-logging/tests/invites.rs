//! Port of `test/invites.test.js`: the snapshot and attribution functions (T1-T12) against JS-produced output, the
//! runtime against a fake Discord, and (with `TEST_DATABASE_URL` / `TEST_REDIS_URL`) the SQL, attribution end to end,
//! gateway order through the hook runner, the `/invites` command and both cross-language directions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, offline_discord_as, redis_url, run_node};
use feat_logging::invite_tracker::{
    self as inv, CodeLife, InviteApi, InviteState, JoinRow, Observation, Snapshot, Vanity, apply_invite_create, apply_invite_delete, build_snapshot,
    classify_join, compute_stats, get_code_state, is_snapshot, parse_snapshot, record_invite_create, record_invite_delete, refresh_snapshot, resolve_join,
    set_invite_api, set_trusted_since, track_join, track_leave,
};
use feat_logging::logging_service::ApiError;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Permissions as P, UserId};

fn golden() -> Value {
    let path = bot_core::testing::golden_dir().join("feat-logging").join("cases.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("run node rust/fixtures/gen/feat-logging-golden.mjs")).expect("json")
}

fn state_of(v: &Value) -> InviteState {
    InviteState {
        code: v["code"].as_str().unwrap_or("").into(),
        uses: v["uses"].as_i64().unwrap_or(0),
        max_uses: v["maxUses"].as_i64().unwrap_or(0),
        inviter_id: v["inviterId"].as_str().map(String::from),
        expires_at: v["expiresAt"].as_i64(),
    }
}

fn observation_of(v: &Value) -> Observation {
    Observation {
        invites: v["invites"].as_array().map(|a| a.iter().map(state_of).collect()).unwrap_or_default(),
        vanity: v["vanity"].as_object().map(|o| Vanity { code: o["code"].as_str().unwrap_or("").into(), uses: o["uses"].as_i64().unwrap_or(0) }),
    }
}

fn snap(v: &Value) -> Snapshot {
    parse_snapshot(v).unwrap_or_else(|| panic!("not a snapshot: {v}"))
}

const NOW: i64 = 10_000_000;

fn s(codes: Value, extra: Value) -> Snapshot {
    let mut v = json!({ "v": 1, "takenAt": 1000, "codes": codes, "tombstones": {}, "vanity": null });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    snap(&v)
}

fn c(uses: i64, max: i64, inviter: Option<&str>, expires: Option<i64>) -> Value {
    json!({ "uses": uses, "maxUses": max, "inviterId": inviter, "expiresAt": expires })
}

fn o(code: &str, uses: i64) -> InviteState {
    InviteState { code: code.into(), uses, ..Default::default() }
}

fn obs(list: Vec<InviteState>, vanity: Option<(&str, i64)>) -> Observation {
    Observation { invites: list, vanity: vanity.map(|(c, u)| Vanity { code: c.into(), uses: u }) }
}

// ── T1-T12 against the JS ─────────────────────────────────────────────────────

#[test]
fn snapshot_functions_match_the_js_output() {
    let g = golden();
    let cases = g["snapshots"].as_array().cloned().unwrap_or_default();
    assert!(cases.len() >= 19);
    for case in &cases {
        let a = &case["args"];
        let got = match case["fn"].as_str().unwrap_or("") {
            "resolveJoin" => {
                let r = resolve_join(&snap(&a["before"]), &observation_of(&a["observation"]), a["now"].as_i64().unwrap_or(0));
                json!({ "match": r.matched, "reason": r.reason, "candidates": r.candidates, "next": r.next })
            }
            "buildSnapshot" => {
                let prev = a["previous"].is_object().then(|| snap(&a["previous"]));
                build_snapshot(&observation_of(&a["observation"]), prev.as_ref(), a["now"].as_i64().unwrap_or(0)).to_json()
            }
            "applyInviteCreate" => apply_invite_create(&snap(&a["snapshot"]), &state_of(&a["invite"]), a["now"].as_i64().unwrap_or(0)).to_json(),
            "applyInviteDelete" => apply_invite_delete(&snap(&a["snapshot"]), a["code"].as_str().unwrap_or(""), a["now"].as_i64().unwrap_or(0)).to_json(),
            "isSnapshot" => {
                // JSON.stringify drops `vanity: undefined`, which is what makes that shape invalid.
                Value::Array(a["values"].as_array().into_iter().flatten().map(|v| json!(is_snapshot(v))).collect())
            }
            "computeStats" => {
                let rows: Vec<JoinRow> = a["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| JoinRow {
                        user_id: r["userId"].as_str().unwrap_or("").into(),
                        inviter_id: r["inviterId"].as_str().map(String::from),
                        joined_ms: r["joinedAt"].as_i64().unwrap_or(0),
                        left_ms: r["leftAt"].as_i64(),
                    })
                    .collect();
                serde_json::to_value(compute_stats(&rows, a["now"].as_i64().unwrap_or(0))).unwrap_or(Value::Null)
            }
            other => panic!("unknown fn {other}"),
        };
        assert_eq!(got, case["out"], "{}", case["name"]);
    }
}

#[test]
fn t1_t2_a_single_increment_matches() {
    let r = resolve_join(&s(json!({ "alice": c(5, 0, Some("A"), None), "bob": c(2, 0, Some("B"), None) }), json!({})), &obs(vec![o("alice", 5), o("bob", 3)], None), NOW);
    let m = r.matched.expect("a match");
    assert_eq!((m.code.as_str(), m.inviter_id.as_deref(), r.reason), ("bob", Some("B"), "matched"));
    assert_eq!(r.next.codes["bob"].uses, 3);
    let r = resolve_join(&s(json!({ "alice99": c(50, 0, None, None), "bob123": c(3, 0, None, None) }), json!({})), &obs(vec![o("alice99", 50), o("bob123", 4)], None), NOW);
    assert_eq!(r.matched.map(|m| m.code), Some("bob123".into()));
}

#[test]
fn t1b_t2b_legacy_and_pre_ready_snapshots_are_not_trusted() {
    assert!(!is_snapshot(&json!({ "alice": 5, "bob": 2 })));
    assert!(!is_snapshot(&Value::Null));
    assert!(!is_snapshot(&json!([])));
    let one = s(json!({}), json!({}));
    assert!(!inv::is_trusted_snapshot(Some(&one), Some(2000)));
    assert!(inv::is_trusted_snapshot(Some(&one), Some(1000)));
    assert!(!inv::is_trusted_snapshot(Some(&one), None));
    assert!(!inv::is_trusted_snapshot(None, Some(0)));
}

#[test]
fn t3_consumed_single_use_invites_and_tombstones() {
    let before = s(json!({ "single": c(0, 1, Some("C"), None), "perm": c(5, 0, None, None) }), json!({}));
    let r = resolve_join(&before, &obs(vec![o("perm", 5)], None), NOW);
    let m = r.matched.clone().expect("match");
    assert_eq!((m.code.as_str(), m.kind, m.inviter_id.as_deref()), ("single", inv::CandidateKind::Vanished, Some("C")));
    assert_eq!(r.next.tombstones["single"].state.uses, 1);
    let again = resolve_join(&r.next, &obs(vec![o("perm", 5)], None), NOW + 1000);
    assert!(again.next.tombstones.contains_key("single"));
    assert_eq!(again.reason, "none", "an exhausted tombstone never matches twice");

    let b2 = apply_invite_delete(&before, "single", NOW);
    assert_eq!(resolve_join(&b2, &obs(vec![o("perm", 5)], None), NOW + 1000).matched.map(|m| m.code), Some("single".into()));
    assert_eq!(resolve_join(&b2, &obs(vec![o("perm", 5)], None), NOW + inv::TOMBSTONE_TTL_MS).matched.map(|m| m.code), Some("single".into()));
    assert_eq!(resolve_join(&b2, &obs(vec![o("perm", 5)], None), NOW + inv::TOMBSTONE_TTL_MS + 1).reason, "none");
}

#[test]
fn t4_t5_t6_t7_candidates() {
    assert_eq!(resolve_join(&s(json!({ "gone": c(3, 10, Some("X"), None) }), json!({})), &obs(vec![], None), NOW).reason, "none");
    assert_eq!(resolve_join(&s(json!({ "gone": c(0, 1, Some("X"), Some(NOW - 1)) }), json!({})), &obs(vec![], None), NOW).reason, "none");
    assert_eq!(resolve_join(&s(json!({ "gone": c(0, 1, Some("X"), Some(NOW + 1)) }), json!({})), &obs(vec![], None), NOW).reason, "matched");
    assert_eq!(resolve_join(&s(json!({ "gone": c(0, 0, Some("X"), None) }), json!({})), &obs(vec![], None), NOW).reason, "none");
    let r = resolve_join(&s(json!({ "alice": c(5, 0, None, None), "bob": c(2, 0, None, None) }), json!({})), &obs(vec![o("alice", 6), o("bob", 3)], None), NOW);
    assert_eq!((r.matched.is_none(), r.reason, r.candidates.len()), (true, "ambiguous", 2));

    let before = s(json!({ "alice": c(5, 0, None, None) }), json!({ "vanity": { "code": "cool", "uses": 40 } }));
    let r = resolve_join(&before, &obs(vec![o("alice", 5)], Some(("cool", 41))), NOW);
    assert_eq!(r.matched.as_ref().map(|m| m.kind), Some(inv::CandidateKind::Vanity));
    let r2 = resolve_join(&r.next, &obs(vec![o("alice", 6)], Some(("cool", 41))), NOW);
    assert_eq!(r2.matched.map(|m| m.code), Some("alice".into()));
    assert_eq!(r2.next.vanity.as_ref().map(|v| v.uses), Some(41));
    assert_eq!(resolve_join(&r2.next, &obs(vec![o("alice", 6)], Some(("cool", 41))), NOW).reason, "none", "a discovery join");
    assert_eq!(build_snapshot(&obs(vec![o("a", 1)], None), None, NOW).vanity, None);
    let changed = s(json!({ "a": c(1, 0, None, None) }), json!({ "vanity": { "code": "old", "uses": 5 } }));
    assert_eq!(resolve_join(&changed, &obs(vec![o("a", 1)], Some(("new", 9))), NOW).reason, "none");

    let created = apply_invite_create(&s(json!({ "alice": c(9, 0, Some("A"), None) }), json!({})), &InviteState { code: "alice".into(), inviter_id: Some("Z".into()), ..Default::default() }, NOW);
    assert_eq!((created.codes["alice"].uses, created.codes["alice"].inviter_id.as_deref(), created.taken_at), (9, Some("A"), 1000));
    let r = resolve_join(&s(json!({ "alice": c(5, 0, None, None) }), json!({})), &obs(vec![o("alice", 5), InviteState { code: "fresh".into(), uses: 1, inviter_id: Some("F".into()), ..Default::default() }], None), NOW);
    assert_eq!(r.matched.map(|m| (m.code, m.inviter_id)), Some(("fresh".into(), Some("F".into()))));
}

#[test]
fn t10_a_json_round_trip_keeps_the_snapshot() {
    let before = s(json!({ "alice": c(5, 0, Some("A"), None), "single": c(0, 1, Some("C"), None) }), json!({ "vanity": { "code": "cool", "uses": 3 } }));
    let trip = parse_snapshot(&before.to_json()).expect("valid");
    assert_eq!(trip, before);
    let observation = obs(vec![InviteState { code: "alice".into(), uses: 5, inviter_id: Some("A".into()), ..Default::default() }], Some(("cool", 3)));
    assert_eq!(resolve_join(&before, &observation, NOW), resolve_join(&trip, &observation, NOW));
    assert_eq!(inv::empty_snapshot(7).taken_at, 7);
    assert!(is_snapshot(&inv::empty_snapshot(7).to_json()));
}

#[test]
fn t11_t12_classify_and_stats() {
    let row = |inviter: &str, joined: i64, left: Option<i64>| JoinRow { user_id: "1".into(), inviter_id: Some(inviter.into()), joined_ms: joined, left_ms: left };
    let day = inv::FAKE_THRESHOLD_MS;
    assert_eq!(classify_join(&row("2", 0, None)), "active");
    assert_eq!(classify_join(&row("2", 0, Some(day - 1))), "fake");
    assert_eq!(classify_join(&row("2", 0, Some(day))), "left");
    assert_eq!(classify_join(&row("1", 0, None)), "fake");
    assert_eq!(classify_join(&row("1", 0, Some(day * 3))), "fake");
}

#[test]
fn invite_to_state_reads_the_invite_defensively() {
    assert_eq!(inv::invite_json_to_state(&json!({ "code": "x" })), InviteState { code: "x".into(), ..Default::default() });
    assert_eq!(
        inv::invite_json_to_state(&json!({ "code": "x", "uses": 2, "maxUses": 5, "inviter": { "id": "U" }, "expiresTimestamp": 99 })),
        InviteState { code: "x".into(), uses: 2, max_uses: 5, inviter_id: Some("U".into()), expires_at: Some(99) }
    );
    let event: serenity::all::InviteCreateEvent = serde_json::from_value(json!({
        "channel_id": CH, "code": "e", "created_at": "2026-01-01T00:00:00.000Z", "guild_id": "1", "max_age": 60, "max_uses": 3, "temporary": false, "uses": 1,
        "inviter": { "id": "5", "username": "i", "discriminator": "0", "avatar": null }
    }))
    .expect("event");
    assert_eq!(
        inv::invite_event_to_state(&event),
        InviteState { code: "e".into(), uses: 1, max_uses: 3, inviter_id: Some("5".into()), expires_at: Some(1_767_225_600_000 + 60_000) }
    );
}

#[test]
fn a_code_that_is_a_prototype_property_name_cannot_poison_the_lookup() {
    let r = resolve_join(&s(json!({}), json!({})), &obs(vec![o("constructor", 0), InviteState { code: "toString".into(), uses: 1, inviter_id: Some("T".into()), ..Default::default() }], None), NOW);
    assert_eq!(r.matched.map(|m| m.code), Some("toString".into()));
}

#[test]
fn invites_payload() {
    let json = feat_logging::commands::invites::command().payload();
    let subs: Vec<&str> = json["options"].as_array().unwrap().iter().filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(subs, vec!["show", "inviter", "leaderboard", "link"]);
    assert_eq!(json["contexts"], json!([0]));
    assert!(json.get("default_member_permissions").is_none_or(Value::is_null));
    assert_eq!(json["options"][3]["options"][0]["channel_types"], json!([0, 5, 2, 13]));
    let meta = feat_logging::commands::invites::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("invites"), json!(true), json!(0)));
}

#[test]
fn the_plugin_declares_five_hooks_and_the_leave_filter_skips_bots() {
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    let p = reg.plugin_named("invites").expect("declared");
    let mut hooks: Vec<String> = p.hooks.iter().map(|h| format!("{}:{}", h.event().as_str(), h.id())).collect();
    hooks.sort();
    assert_eq!(hooks, vec!["guildMemberAdd:invites", "guildMemberRemove:invites", "guildReady:invites", "inviteCreate:invites", "inviteDelete:invites"]);
    let always: Vec<bool> = p.hooks.iter().map(|h| h.always()).collect();
    assert_eq!(always, vec![false, true, true, false, false]);
}

// ── Runtime against a fake Discord ────────────────────────────────────────────

#[derive(Default)]
struct FakeDiscord {
    live: Mutex<Vec<InviteState>>,
    vanity_uses: Mutex<i64>,
    fail_fetch: Mutex<Option<u64>>,
    fetch_calls: Mutex<u32>,
    deleted: Mutex<Vec<(String, String)>>,
    delete_delay_ms: Mutex<u64>,
    created: Mutex<u32>,
    create_error: Mutex<Option<u64>>,
    invite_guild: Mutex<Option<Result<Option<String>, ApiError>>>,
    /// Prefix of the codes `create_invite` hands out ("NEW" when unset). PersonalInvite.code is unique across guilds
    /// and the tests in this file run in parallel on one database, so each test that saves a link needs its own.
    code_prefix: Mutex<Option<&'static str>>,
}

impl FakeDiscord {
    fn with(invites: &[(&str, i64, i64, Option<&str>)]) -> Arc<Self> {
        let d = Self::default();
        *d.live.lock().unwrap() =
            invites.iter().map(|(c, u, m, i)| InviteState { code: (*c).into(), uses: *u, max_uses: *m, inviter_id: i.map(String::from), expires_at: None }).collect();
        Arc::new(d)
    }
    fn set_uses(&self, code: &str, uses: i64) {
        if let Some(i) = self.live.lock().unwrap().iter_mut().find(|i| i.code == code) {
            i.uses = uses;
        }
    }
    fn add(&self, code: &str, max: i64, inviter: &str) {
        self.live.lock().unwrap().push(InviteState { code: code.into(), max_uses: max, inviter_id: Some(inviter.into()), ..Default::default() });
    }
    fn remove(&self, code: &str) {
        self.live.lock().unwrap().retain(|i| i.code != code);
    }
    fn get(&self, code: &str) -> InviteState {
        self.live.lock().unwrap().iter().find(|i| i.code == code).cloned().unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl InviteApi for FakeDiscord {
    async fn fetch_invites(&self, _g: GuildId) -> Result<Vec<InviteState>, ApiError> {
        *self.fetch_calls.lock().unwrap() += 1;
        if let Some(code) = *self.fail_fetch.lock().unwrap() {
            return Err(ApiError::new(Some(code), "Missing Access"));
        }
        Ok(self.live.lock().unwrap().clone())
    }
    async fn fetch_vanity(&self, _g: GuildId) -> Result<Option<Vanity>, ApiError> {
        Ok(Some(Vanity { code: "cool".into(), uses: *self.vanity_uses.lock().unwrap() }))
    }
    async fn fetch_invite_guild(&self, _code: &str) -> Result<Option<String>, ApiError> {
        self.invite_guild.lock().unwrap().clone().unwrap_or(Err(ApiError::new(Some(10006), "Unknown Invite")))
    }
    async fn delete_invite(&self, _g: GuildId, code: &str, reason: &str) -> Result<(), ApiError> {
        let delay = *self.delete_delay_ms.lock().unwrap();
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        self.deleted.lock().unwrap().push((code.into(), reason.into()));
        Ok(())
    }
    async fn create_invite(&self, _c: ChannelId, _r: &str) -> Result<String, ApiError> {
        if let Some(code) = *self.create_error.lock().unwrap() {
            return Err(ApiError::new(Some(code), "refused"));
        }
        let mut n = self.created.lock().unwrap();
        *n += 1;
        Ok(format!("{}{n}", self.code_prefix.lock().unwrap().unwrap_or("NEW")))
    }
}

const BOT_ROLE: &str = "820000000000000001";
const CH: &str = "700000000000000001";

fn fake_guild(id: &str, bot: P, vanity: bool) -> Value {
    let mut g = FakeGuild::new(id, "600000000000000099")
        .everyone(P::VIEW_CHANNEL | P::CREATE_INSTANT_INVITE)
        .role(BOT_ROLE, 1, bot)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .channel(CH, 0, "general", &[])
        .channel("700000000000000002", 0, "locked", &[(id, false, P::empty(), P::CREATE_INSTANT_INVITE), (BOT_ROLE, false, P::CREATE_INSTANT_INVITE, P::empty())])
        .channel("700000000000000003", 4, "category", &[])
        .channel("700000000000000004", 0, "bot-blind", &[(BOT_ROLE, false, P::empty(), P::VIEW_CHANNEL)])
        .json();
    if vanity {
        g["vanity_url_code"] = json!("cool");
    }
    g["name"] = json!("Test Guild");
    g
}

const TRACK: P = P::MANAGE_GUILD.union(P::VIEW_CHANNEL).union(P::CREATE_INSTANT_INVITE).union(P::SEND_MESSAGES);

fn offline(id: &str, bot: P, api: Arc<FakeDiscord>) -> TestApp {
    let t = TestApp::new().with_guilds([id]);
    t.set_discord(offline_discord_as(&[fake_guild(id, bot, false)]));
    set_invite_api(&t, api);
    set_trusted_since(&t, 0);
    t
}

fn gid(id: &str) -> GuildId {
    GuildId::new(id.parse().unwrap())
}

#[tokio::test]
async fn refresh_warms_a_snapshot_and_incremental_edits_make_no_rest_call() {
    const G: &str = "955010000000000101";
    let api = FakeDiscord::with(&[("A", 2, 0, Some("U1"))]);
    let t = offline(G, TRACK, api.clone());
    record_invite_create(&t, gid(G), InviteState { code: "B".into(), ..Default::default() }).await;
    assert_eq!(get_code_state(&t, gid(G), "B", false, None).await, CodeLife::Unknown, "never a partial snapshot");
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    assert_eq!(*api.fetch_calls.lock().unwrap(), 1);
    assert_eq!(get_code_state(&t, gid(G), "A", false, None).await, CodeLife::Live);
    record_invite_create(&t, gid(G), InviteState { code: "B".into(), ..Default::default() }).await;
    assert_eq!(get_code_state(&t, gid(G), "B", false, None).await, CodeLife::Live);
    record_invite_delete(&t, gid(G), "B".into()).await;
    assert_eq!(get_code_state(&t, gid(G), "B", false, None).await, CodeLife::Gone);
    assert_eq!(*api.fetch_calls.lock().unwrap(), 1);
    let raw = t.cache_backend.raw(&inv::snapshot_key(G)).expect("mirrored to the cache");
    let stored: Value = serde_json::from_str(&raw).expect("json");
    assert!(is_snapshot(&stored));
    assert_eq!(stored["tombstones"]["B"]["uses"], 0);
    let ttl = t.cache_backend.pttl(&inv::snapshot_key(G));
    assert!(ttl > 604_000_000 && ttl <= 604_800_000, "{ttl}");
}

#[tokio::test]
async fn refresh_reports_why_it_did_nothing_and_keeps_the_stored_snapshot() {
    const G: &str = "955010000000000102";
    let api = FakeDiscord::with(&[("A", 0, 0, None)]);
    let t = offline(G, TRACK, api.clone());
    let unavailable = TestApp::new();
    set_invite_api(&unavailable, api.clone());
    assert_eq!(refresh_snapshot(&unavailable, gid(G)).await.reason, Some("unavailable"));

    let no_perm = offline("955010000000000103", P::VIEW_CHANNEL, api.clone());
    assert!(!inv::bot_can_track(&no_perm, gid("955010000000000103")));
    let before = *api.fetch_calls.lock().unwrap();
    assert_eq!(refresh_snapshot(&no_perm, gid("955010000000000103")).await.reason, Some("no-permission"));
    assert_eq!(*api.fetch_calls.lock().unwrap(), before);

    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    *api.fail_fetch.lock().unwrap() = Some(50001);
    assert_eq!(refresh_snapshot(&t, gid(G)).await.reason, Some("fetch-failed"));
    assert_eq!(get_code_state(&t, gid(G), "A", false, None).await, CodeLife::Live, "the old snapshot survives");
    assert!(inv::bot_can_track(&TestApp::new(), gid(G)), "an uncached me is optimistic");
}

#[tokio::test]
async fn a_snapshot_from_before_this_process_is_replaced_not_inherited() {
    const G: &str = "955010000000000104";
    let api = FakeDiscord::with(&[("A", 0, 1, None)]);
    let t = offline(G, TRACK, api.clone());
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    // A restart: this process became ready after the stored snapshot. A vanished code must NOT become a tombstone.
    inv::forget_memo(&t, G);
    t.clock.advance(1000);
    set_trusted_since(&t, t.app.clock().now_ms());
    api.remove("A");
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    assert_eq!(get_code_state(&t, gid(G), "A", false, None).await, CodeLife::Unknown);
}

/// inviteTracker.js `trustedSince(guild)` is `client.readyTimestamp ?? +Infinity`: before the client is ready no
/// snapshot is trusted (a join only re-baselines); after it, a snapshot taken since then is.
#[tokio::test]
async fn snapshots_are_trusted_only_from_the_ready_time() {
    const G: &str = "955010000000000106";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1"))]);
    let t = TestApp::new().with_guilds([G]);
    t.set_discord(offline_discord_as(&[fake_guild(G, TRACK, false)]));
    set_invite_api(&t, api.clone());
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    api.set_uses("A", 1);
    let before_ready = track_join(&t, gid(G), UserId::new(200000000000000001), None, None).await;
    assert_eq!(before_ready, None, "not ready: never diffed");

    t.clock.advance(1000);
    assert!(t.app.set_ready_at_ms(t.app.clock().now_ms()));
    api.set_uses("A", 2);
    let first = track_join(&t, gid(G), UserId::new(200000000000000002), None, None).await;
    assert_eq!(first, None, "the snapshot is older than the ready time: re-baselined");
    api.set_uses("A", 3);
    let matched = track_join(&t, gid(G), UserId::new(200000000000000003), None, None).await;
    assert_eq!(matched.and_then(|i| i.code), Some("A".to_string()));
}

#[tokio::test]
async fn get_code_state_rules() {
    const G: &str = "955010000000000105";
    let api = FakeDiscord::with(&[("A", 0, 0, None)]);
    let t = offline(G, TRACK, api.clone());
    assert_eq!(get_code_state(&t, gid(G), "X", false, None).await, CodeLife::Unknown);
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    let now = t.app.clock().now_ms();
    assert_eq!(get_code_state(&t, gid(G), "X", false, Some(now - 60_000)).await, CodeLife::Gone);
    assert_eq!(get_code_state(&t, gid(G), "X", false, Some(now + 60_000)).await, CodeLife::Unknown);
    *api.invite_guild.lock().unwrap() = Some(Ok(Some(G.into())));
    assert_eq!(get_code_state(&t, gid(G), "X", true, Some(now + 60_000)).await, CodeLife::Live);
    *api.invite_guild.lock().unwrap() = Some(Ok(Some("other".into())));
    assert_eq!(get_code_state(&t, gid(G), "X", true, Some(now + 60_000)).await, CodeLife::Gone);
    *api.invite_guild.lock().unwrap() = Some(Err(ApiError::new(Some(10006), "nope")));
    assert_eq!(get_code_state(&t, gid(G), "X", true, Some(now + 60_000)).await, CodeLife::Gone);
    *api.invite_guild.lock().unwrap() = Some(Err(ApiError::new(None, "network")));
    assert_eq!(get_code_state(&t, gid(G), "X", true, Some(now + 60_000)).await, CodeLife::Unknown);
    inv::delete_discord_invite(&t, gid(G), "X", "why").await;
    assert_eq!(*api.deleted.lock().unwrap(), vec![("X".to_string(), "why".to_string())]);
}

#[tokio::test]
async fn the_join_hook_returns_at_once_and_bots_are_not_tracked() {
    const G: &str = "955010000000009998";
    let api = FakeDiscord::with(&[]);
    let t = offline(G, TRACK, api);
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    t.install(reg);
    let bot: serenity::all::Member = serde_json::from_value(json!({
        "guild_id": G, "user": { "id": "333333333333333333", "username": "robot", "discriminator": "0", "avatar": null, "bot": true },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    }))
    .expect("member");
    let user = bot.user.clone();
    let ctx = t.run_hooks(GuildMemberAdd { guild_id: gid(G), member: bot, user }).await;
    assert_eq!(ctx.state.get::<Option<feat_logging::InviteInfo>>("invite"), Some(None), "null for bots");
    assert!(ctx.state.get::<feat_logging::InvitePending>("invitePromise").is_none());
}

// ── With PostgreSQL ───────────────────────────────────────────────────────────

async fn db(id: &'static str, bot: P, vanity: bool, api: Arc<FakeDiscord>) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds([id]);
    t.reset_guilds(&[id]).await;
    t.set_discord(offline_discord_as(&[fake_guild(id, bot, vanity)]));
    set_invite_api(&t, api);
    set_trusted_since(&t, 0);
    Some(t)
}

/// `(userId, inviterId, inviteCode, joinedAt ms, leftAt ms)`.
type SeedRow<'a> = (&'a str, Option<&'a str>, Option<&'a str>, i64, Option<i64>);

async fn seed_rows(t: &DbTestApp, g: &str, rows: &[SeedRow<'_>]) {
    t.settings().ensure_guild_settings(g).await.expect("settings row");
    for (user, inviter, code, joined, left) in rows {
        let ts = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().naive_utc();
        bot_db::sqlx::query(r#"INSERT INTO "GuildMemberCache" ("id", "guildId", "userId", "inviterId", "inviteCode", "joinedAt", "leftAt") VALUES ($1, $2, $3, $4, $5, $6, $7)"#)
            .bind(bot_db::new_cuid())
            .bind(g)
            .bind(user)
            .bind(inviter)
            .bind(code)
            .bind(ts(*joined))
            .bind(left.map(ts))
            .execute(t.db())
            .await
            .expect("row");
    }
}

#[tokio::test]
async fn every_raw_statement_executes_against_the_schema() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let sqls = inv::raw_sql();
    assert_eq!(sqls.len(), 2);
    let mut tx = pool.begin().await.expect("tx");
    bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(sqls[0].1.clone())).bind("0").bind("0").fetch_all(&mut *tx).await.expect("stats");
    bot_db::sqlx::query(bot_db::sqlx::AssertSqlSafe(sqls[1].1.clone())).bind("0").bind(vec!["0".to_string()]).bind(10_i64).fetch_all(&mut *tx).await.expect("leaderboard");
    tx.rollback().await.expect("rollback");
    for (_, sql) in sqls {
        assert!(!sql.contains("now()") || sql.contains("(now() AT TIME ZONE 'UTC')"));
        for alias in ["total", "active", "leftCount", "fake", "recent"] {
            assert!(sql.contains(&format!("::int AS \"{alias}\"")), "{alias}");
        }
    }
}

const H: i64 = 3_600_000;

#[tokio::test]
async fn stats_match_compute_stats_and_the_leaderboard_ranks_by_valid_invites() {
    const G: &str = "955010000000000201";
    let Some(t) = db(G, TRACK, false, FakeDiscord::with(&[])).await else { return };
    let now = bot_util::js::now_ms() as i64;
    let rows = [
        ("a", Some("I1"), None, now - H, None),
        ("b", Some("I1"), None, now - 48 * H, None),
        ("c", Some("I1"), None, now - 100 * H, Some(now - 70 * H)),
        ("d", Some("I1"), None, now - 100 * H, Some(now - 98 * H)),
        ("I1", Some("I1"), None, now - 10 * H, None),
        ("e", None, None, now - H, None),
        ("f", Some("I2"), None, now - 5 * H, None),
    ];
    seed_rows(&t, G, &rows).await;
    let sql = inv::get_invite_stats(t.db(), G, "I1").await.expect("stats");
    let js_rows: Vec<JoinRow> = rows
        .iter()
        .filter(|r| r.1 == Some("I1"))
        .map(|r| JoinRow { user_id: r.0.into(), inviter_id: r.1.map(String::from), joined_ms: r.3, left_ms: r.4 })
        .collect();
    assert_eq!(sql, compute_stats(&js_rows, now));
    assert_eq!(serde_json::to_value(sql).unwrap(), json!({ "total": 5, "active": 2, "left": 1, "fake": 2, "valid": 3, "recent": 1 }));
    assert_eq!(inv::get_invite_stats(t.db(), G, "nobody").await.expect("stats"), inv::InviteStats::default());

    const L: &str = "955010000000000202";
    t.reset_guilds(&[L]).await;
    let mk = |u: &'static str, i: &'static str| (u, Some(i), None, now - 50 * H, None);
    seed_rows(&t, L, &[
        mk("a1", "I1"), mk("a2", "I1"), ("a3", Some("I1"), None, now - 50 * H, Some(now - 10 * H)),
        mk("b1", "I5"), mk("b2", "I5"), mk("c1", "I2"), mk("c2", "I2"),
        ("d1", Some("I3"), None, now - 5 * H, Some(now - 4 * H)),
        mk("e1", "BOT"), mk("e2", "BOT"), mk("e3", "BOT"), mk("e4", "BOT"),
        ("z", None, None, now, None),
    ])
    .await;
    let lb = inv::get_invite_leaderboard(t.db(), L, 200, &["BOT".to_string()]).await.expect("leaderboard");
    assert_eq!(lb.iter().map(|r| (r.inviter_id.as_str(), r.stats.valid)).collect::<Vec<_>>(), vec![("I1", 3), ("I2", 2), ("I5", 2)]);
    assert_eq!(serde_json::to_value(&lb[0]).unwrap(), json!({ "inviterId": "I1", "total": 3, "active": 2, "left": 1, "fake": 0, "valid": 3, "recent": 0 }));
    assert_eq!(inv::get_invite_leaderboard(t.db(), L, 200, &[]).await.expect("lb")[0].inviter_id, "BOT");
    assert_eq!(inv::get_invite_leaderboard(t.db(), L, 2, &["BOT".to_string()]).await.expect("lb").len(), 2);
    t.reset_guilds(&[G, L]).await;
}

#[tokio::test]
async fn save_personal_invite_upserts_and_resets_created_at() {
    const G: &str = "955010000000000203";
    let Some(t) = db(G, TRACK, false, FakeDiscord::with(&[])).await else { return };
    let first = inv::save_personal_invite(&t, G, "U1", "CODE1", "c1").await.expect("save");
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let second = inv::save_personal_invite(&t, G, "U1", "CODE2", "c2").await.expect("save");
    assert_eq!(second.id, first.id);
    assert_eq!((second.code.as_str(), second.channel_id.as_str()), ("CODE2", "c2"));
    assert!(second.created_at > first.created_at);
    assert!(inv::get_personal_invite(t.db(), G, "nobody").await.expect("read").is_none());
    t.reset_guilds(&[G]).await;
}

fn join(t: &DbTestApp, g: &str, user: &str) -> impl std::future::Future<Output = Option<feat_logging::InviteInfo>> + Send + 'static + use<> {
    track_join(t, gid(g), UserId::new(fake_id(user)), None, None)
}

/// A stable numeric id for a readable test name.
fn fake_id(name: &str) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for b in name.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(1099511628211);
    }
    (h % 900_000_000_000_000_000) + 100_000_000_000_000_000
}

async fn record(t: &DbTestApp, g: &str, user: &str) -> Option<bot_db::GuildMemberCache> {
    inv::get_join_record(t.db(), g, &fake_id(user).to_string()).await.expect("read")
}

#[tokio::test]
async fn attribution_end_to_end() {
    const G: &str = "955010000000000301";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1")), ("B", 0, 0, Some("U2"))]);
    *api.vanity_uses.lock().unwrap() = 10;
    let Some(t) = db(G, TRACK, true, api.clone()).await else { return };
    let info = |c: &str, i: Option<&str>, v: bool| Some(feat_logging::InviteInfo { code: Some(c.into()), inviter_id: i.map(String::from), vanity: v });

    assert_eq!(join(&t, G, "m1").await, None, "no snapshot yet");
    let r = record(&t, G, "m1").await.expect("row");
    assert_eq!((r.inviter_id, r.invite_code, r.left_at), (None, None, None));
    api.set_uses("A", 1);
    assert_eq!(join(&t, G, "m2").await, info("A", Some("U1"), false));
    let r = record(&t, G, "m2").await.expect("row");
    assert_eq!((r.inviter_id.as_deref(), r.invite_code.as_deref()), (Some("U1"), Some("A")));
    api.set_uses("A", 2);
    api.set_uses("B", 1);
    assert_eq!(join(&t, G, "m3").await, None, "ambiguous");
    *api.vanity_uses.lock().unwrap() = 11;
    assert_eq!(join(&t, G, "m4").await, info("cool", None, true));
    assert_eq!(record(&t, G, "m4").await.and_then(|r| r.invite_code).as_deref(), Some("cool"));
    assert_eq!(join(&t, G, "m5").await, None, "a discovery join");

    inv::save_personal_invite(&t, G, "U3", "PERS", "c1").await.expect("save");
    api.add("PERS", 0, TEST_BOT_ID);
    record_invite_create(&t, gid(G), api.get("PERS")).await;
    api.set_uses("PERS", 1);
    assert_eq!(join(&t, G, "m6").await, info("PERS", Some("U3"), false), "a personal link is credited to its owner");
    api.add("BOTLINK", 0, TEST_BOT_ID);
    record_invite_create(&t, gid(G), api.get("BOTLINK")).await;
    api.set_uses("BOTLINK", 1);
    assert_eq!(join(&t, G, "m7").await, info("BOTLINK", None, false), "never credit the bot");

    api.add("ONCE", 1, "U4");
    record_invite_create(&t, gid(G), api.get("ONCE")).await;
    api.remove("ONCE");
    assert_eq!(join(&t, G, "m8").await, info("ONCE", Some("U4"), false));
    assert_eq!(join(&t, G, "m9").await, None, "the exhausted code cannot match twice");

    track_leave(&t, gid(G), UserId::new(fake_id("m2"))).await;
    assert!(record(&t, G, "m2").await.and_then(|r| r.left_at).is_some());
    assert_eq!(join(&t, G, "m2").await, None);
    let again = record(&t, G, "m2").await.expect("row");
    assert_eq!((again.inviter_id, again.invite_code, again.left_at), (None, None, None), "a rejoin overwrites");
    track_leave(&t, gid(G), UserId::new(fake_id("never-joined"))).await;
    assert!(record(&t, G, "never-joined").await.is_none(), "a leave never creates a row");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn join_then_leave_keeps_the_order_and_the_raid_cap_holds() {
    const G: &str = "955010000000000302";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1"))]);
    let Some(t) = db(G, TRACK, false, api.clone()).await else { return };
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    api.set_uses("A", 1);
    let joined = join(&t, G, "quick"); // no await in between
    let left = track_leave(&t, gid(G), UserId::new(fake_id("quick")));
    assert_eq!(joined.await.and_then(|i| i.code), Some("A".into()));
    left.await;
    let r = record(&t, G, "quick").await.expect("row");
    assert!(r.left_at.is_some());
    assert_eq!(classify_join(&JoinRow::from(&r)), "fake");
    assert_eq!(serde_json::to_value(inv::get_invite_stats(t.db(), G, "U1").await.expect("stats")).unwrap(), json!({ "total": 1, "active": 0, "left": 0, "fake": 1, "valid": 0, "recent": 0 }));

    const R: &str = "955010000000000303";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1"))]);
    let Some(t) = db(R, TRACK, false, api.clone()).await else { return };
    assert!(refresh_snapshot(&t, gid(R)).await.ok);
    let before = *api.fetch_calls.lock().unwrap();
    let total = inv::MAX_PENDING_JOINS + 4;
    let futs: Vec<_> = (0..total).map(|i| join(&t, R, &format!("raid{i}"))).collect();
    let mut results = Vec::new();
    for f in futs {
        results.push(f.await);
    }
    assert!(results.iter().all(Option::is_none));
    assert_eq!(*api.fetch_calls.lock().unwrap() - before, inv::MAX_PENDING_JOINS as u32, "only the first joins fetched");
    let count: i64 = bot_db::sqlx::query_scalar(r#"SELECT COUNT(*) FROM "GuildMemberCache" WHERE "guildId" = $1"#).bind(R).fetch_one(t.db()).await.expect("count");
    assert_eq!(count, total as i64, "every join still has a row");
    assert_eq!(join(&t, R, "after1").await, None, "the burst dropped the snapshot");
    api.set_uses("A", 1);
    assert_eq!(join(&t, R, "after2").await.and_then(|i| i.code), Some("A".into()));
    t.reset_guilds(&[G, R]).await;
}

#[tokio::test]
async fn a_disabled_module_or_a_missing_permission_records_unknown_rows() {
    const G: &str = "955010000000000304";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1"))]);
    let Some(t) = db(G, TRACK, false, api.clone()).await else { return };
    assert!(refresh_snapshot(&t, gid(G)).await.ok);
    api.set_uses("A", 1);
    let off: inv::IsEnabled = Box::new(|| Box::pin(async { false }));
    assert_eq!(track_join(&t, gid(G), UserId::new(fake_id("off1")), None, Some(off)).await, None);
    assert_eq!(record(&t, G, "off1").await.and_then(|r| r.inviter_id), None);
    assert_eq!(get_code_state(&t, gid(G), "A", false, None).await, CodeLife::Unknown, "the snapshot was dropped");
    assert_eq!(join(&t, G, "on1").await, None);
    api.set_uses("A", 2);
    assert_eq!(join(&t, G, "on2").await.and_then(|i| i.code), Some("A".into()));

    const N: &str = "955010000000000305";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1"))]);
    let Some(t) = db(N, P::VIEW_CHANNEL, false, api.clone()).await else { return };
    assert_eq!(join(&t, N, "np").await, None);
    assert_eq!(*api.fetch_calls.lock().unwrap(), 0);
    assert!(record(&t, N, "np").await.is_some());
    t.reset_guilds(&[G, N]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn through_the_hook_runner_join_then_leave_keeps_gateway_order() {
    const G: &str = "955010000000000306";
    let api = FakeDiscord::with(&[("A", 0, 0, Some("U1"))]);
    let Some(t) = db(G, TRACK, false, api.clone()).await else { return };
    t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", true)).await.expect("write");
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    t.install(reg);
    bot_core::hooks::run_guild_ready(&t, gid(G), GuildReadyReason::Startup).await;
    assert_eq!(get_code_state(&t, gid(G), "A", false, None).await, CodeLife::Live, "guildReady warmed the snapshot");

    let invite: serenity::all::InviteCreateEvent = serde_json::from_value(json!({
        "channel_id": CH, "code": "N", "created_at": "2026-01-01T00:00:00.000Z", "guild_id": G, "max_age": 0, "max_uses": 0, "temporary": false, "uses": 0
    }))
    .expect("invite");
    t.run_hooks(InviteCreate { guild_id: gid(G), invite }).await;
    assert_eq!(get_code_state(&t, gid(G), "N", false, None).await, CodeLife::Live);
    let deleted: serenity::all::InviteDeleteEvent = serde_json::from_value(json!({ "channel_id": CH, "guild_id": G, "code": "N" })).expect("invite");
    t.run_hooks(InviteDelete { guild_id: gid(G), invite: deleted }).await;
    assert_eq!(get_code_state(&t, gid(G), "N", false, None).await, CodeLife::Gone);

    api.set_uses("A", 1);
    let uid = fake_id("hooked");
    let member: serenity::all::Member = serde_json::from_value(json!({
        "guild_id": G, "user": { "id": uid.to_string(), "username": "hooked", "discriminator": "0", "avatar": null },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    }))
    .expect("member");
    let user = member.user.clone();
    let j = t.run_hooks(GuildMemberAdd { guild_id: gid(G), member: member.clone(), user: user.clone() });
    let l = t.run_hooks(GuildMemberRemove { guild_id: gid(G), member: Some(member), user });
    let (jctx, _) = tokio::join!(j, l);
    let pending = jctx.state.get::<feat_logging::InvitePending>("invitePromise").expect("published");
    assert_eq!(pending.0.await.and_then(|i| i.code), Some("A".into()));
    track_leave(&t, gid(G), UserId::new(1)).await; // a fence behind the leave
    let r = inv::get_join_record(t.db(), G, &uid.to_string()).await.expect("read").expect("row");
    assert_eq!(r.inviter_id.as_deref(), Some("U1"));
    assert!(r.left_at.is_some(), "leftAt was set after the join row existed");
    t.reset_guilds(&[G]).await;
}

// ── /invites through the router ───────────────────────────────────────────────

fn slash(g: &str, sub: &str, user: &str) -> FakeInteraction {
    FakeInteraction::slash("invites").guild(g).user(user).channel(CH).subcommand(sub)
}

fn with_user(mut i: FakeInteraction, id: &str, name: &str, global: Option<&str>) -> FakeInteraction {
    let mut users = serde_json::Map::new();
    users.insert(id.into(), json!({ "id": id, "username": name, "global_name": global, "discriminator": "0", "avatar": null }));
    i.json_mut()["data"]["resolved"] = json!({ "users": users });
    i.option("member", option_type::USER, json!(id))
}

async fn route(t: &DbTestApp, i: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    t.responses.clear();
    bot_core::router::route_interaction(t, i.build()).await;
    t.responses.calls()
}

fn embed(calls: &[bot_core::interaction::RecordedCall]) -> Value {
    calls.last().map(|c| c.body["embeds"][0].clone()).unwrap_or(Value::Null)
}

fn fields(e: &Value) -> HashMap<String, String> {
    e["fields"].as_array().into_iter().flatten().map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into())).collect()
}

#[tokio::test]
async fn invites_show_inviter_and_leaderboard() {
    const G: &str = "955010000000000401";
    let Some(t) = db(G, TRACK, true, FakeDiscord::with(&[])).await else { return };
    t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", true)).await.expect("write");
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    t.install(reg);
    let now = bot_util::js::now_ms() as i64;
    seed_rows(&t, G, &[
        ("200000000000000001", Some("100000000000000011"), Some("CODE"), now - 2 * H, None),
        ("200000000000000002", Some("100000000000000011"), Some("CODE"), now - 100 * H, Some(now - 98 * H)),
        ("200000000000000003", None, Some("cool"), now - 100 * H, None),
        ("200000000000000004", None, Some("ORPH"), now - 100 * H, None),
        ("200000000000000005", None, None, now - 100 * H, None),
        ("200000000000000006", Some("100000000000000011"), Some("CODE"), now - 100 * H, Some(now - 60 * H)),
    ])
    .await;
    inv::save_personal_invite(&t, G, "100000000000000011", "PERSONAL", "chan1").await.expect("save");

    let calls = route(&t, with_user(slash(G, "show", "100000000000000012"), "100000000000000011", "ivy", Some("Ivy"))).await;
    assert!(!calls[0].ephemeral(), "show is public");
    let e = embed(&calls);
    assert_eq!(e["title"], "📨 Invites — Ivy");
    let d = e["description"].as_str().unwrap_or("");
    assert!(d.starts_with("<@100000000000000011> has **2** valid invites."), "{d}");
    assert!(d.contains("1 of them joined less than 24 hours ago"));
    let f = fields(&e);
    assert_eq!((f["✅ Valid"].as_str(), f["🏠 Still here"].as_str(), f["🚪 Left (after 24 h)"].as_str(), f["❌ Fake"].as_str(), f["📥 Total joins"].as_str()), ("2", "1", "1", "1", "3"));
    assert!(f["🙋 Invited by"].starts_with("Unknown"));
    assert_eq!(f["🔗 Personal link"], "https://discord.gg/PERSONAL (opens <#chan1>)");
    assert_eq!(e["footer"]["text"], "Valid = still here or stayed 24 h · Fake = left within 24 h");

    let e = embed(&route(&t, with_user(slash(G, "show", "100000000000000012"), "200000000000000001", "al", None)).await);
    let by = fields(&e)["🙋 Invited by"].clone();
    assert!(by.starts_with("<@100000000000000011> · `discord.gg/CODE` · joined <t:") && by.ends_with(":R>"), "{by}");

    let text = |id: &str| {
        let t = &t;
        let id = id.to_string();
        async move { embed(&route(t, with_user(slash(G, "inviter", "100000000000000012"), &id, &id, None)).await)["description"].as_str().unwrap_or("").to_string() }
    };
    assert!(text("200000000000000001").await.starts_with("<@200000000000000001> was invited by <@100000000000000011> with `discord.gg/CODE` <t:"));
    assert!(text("200000000000000003").await.starts_with("<@200000000000000003> joined through the vanity URL `discord.gg/cool` <t:"));
    assert!(text("200000000000000004").await.ends_with(", but I don't know who created that invite."));
    assert!(text("200000000000000005").await.starts_with("I don't know who invited <@200000000000000005>."));
    assert!(text("200000000000000006").await.contains("\nThey left <t:"));
    assert!(text("200000000000000007").await.starts_with("I don't know who invited <@200000000000000007>."));

    let e = embed(&route(&t, slash(G, "leaderboard", "100000000000000011")).await);
    assert_eq!(e["title"], "🏆 Test Guild — Invite Leaderboard");
    assert_eq!(e["description"], "🥇 <@100000000000000011> — **2** invites (1 left · 1 fake)");
    assert!(e["footer"]["text"].as_str().unwrap_or("").starts_with("Valid = still here or stayed 24 h · Your rank: #1"));

    bot_db::sqlx::query(r#"DELETE FROM "GuildMemberCache" WHERE "guildId" = $1"#).bind(G).execute(t.db()).await.expect("clear");
    let e = embed(&route(&t, slash(G, "leaderboard", "100000000000000011")).await);
    assert_eq!((e["title"].as_str(), e["description"].as_str()), (Some("🏆 Invite Leaderboard"), Some("No invites have been tracked in this server yet.")));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn invites_link_creates_reuses_replaces_and_maps_failures() {
    const G: &str = "955010000000000403";
    let api = FakeDiscord::with(&[]);
    let Some(t) = db(G, TRACK, false, api.clone()).await else { return };
    t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", true)).await.expect("write");
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    t.install(reg);
    const U: &str = "100000000000000020";
    let link = |ch: Option<&str>| {
        let i = slash(G, "link", U);
        match ch {
            Some(c) => i.option("channel", option_type::CHANNEL, json!(c)),
            None => i,
        }
    };
    let content = |calls: &[bot_core::interaction::RecordedCall]| calls.last().and_then(|c| c.body["content"].as_str().map(String::from));

    let first = route(&t, link(None)).await;
    assert!(first[0].ephemeral());
    assert_eq!(content(&first).as_deref(), Some("https://discord.gg/NEW1"));
    assert_eq!(first.last().unwrap().body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(inv::get_personal_invite(t.db(), G, U).await.unwrap().map(|p| p.code).as_deref(), Some("NEW1"));

    *api.invite_guild.lock().unwrap() = Some(Ok(Some(G.into())));
    let again = route(&t, link(None)).await;
    assert_eq!(content(&again).as_deref(), Some("https://discord.gg/NEW1"));
    assert_eq!(embed(&again)["footer"]["text"], "This is your existing link.");
    *api.invite_guild.lock().unwrap() = Some(Err(ApiError::new(None, "network")));
    assert_eq!(content(&route(&t, link(None)).await).as_deref(), Some("https://discord.gg/NEW1"), "unknown counts as alive");
    assert_eq!(*api.created.lock().unwrap(), 1);

    // The cooldown (60 s) holds the next creation back; the JS test runs with Redis down (fail open).
    assert_eq!(embed(&route(&t, link(Some("700000000000000003"))).await)["title"], "❌  Unsupported Channel");
    *api.invite_guild.lock().unwrap() = Some(Err(ApiError::new(Some(10006), "Unknown Invite")));
    assert_eq!(embed(&route(&t, link(None)).await)["title"], "⚠️  Slow Down");
    t.cache().del_one(&feat_logging::commands::invites::link_cooldown_key(G, U)).await;
    // invites.js createLink: `deleteDiscordInvite(...).catch(() => {})` is not awaited; the reply does not wait for it.
    *api.delete_delay_ms.lock().unwrap() = 1500;
    let replaced = route(&t, link(None)).await;
    assert_eq!(content(&replaced).as_deref(), Some("https://discord.gg/NEW2"));
    assert!(api.deleted.lock().unwrap().is_empty(), "answered before the old invite was deleted");
    for _ in 0..100 {
        if !api.deleted.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    *api.delete_delay_ms.lock().unwrap() = 0;
    assert_eq!(api.deleted.lock().unwrap().iter().map(|d| d.0.as_str()).collect::<Vec<_>>(), vec!["NEW1"]);
    assert!(embed(&replaced)["footer"].is_null(), "the old code was already gone");

    // The cooldown key is set while Redis works; this app's memory cache is fresh per call path, so clear it.
    t.cache().del_one(&feat_logging::commands::invites::link_cooldown_key(G, U)).await;
    *api.invite_guild.lock().unwrap() = Some(Ok(Some(G.into())));
    let moved = route(&t, link(Some("700000000000000003"))).await;
    assert_eq!(embed(&moved)["title"], "❌  Unsupported Channel");
    t.cache().del_one(&feat_logging::commands::invites::link_cooldown_key(G, U)).await;
    assert_eq!(embed(&route(&t, link(Some("700000000000000004"))).await)["title"], "❌  Missing Permissions");
    assert_eq!(embed(&route(&t, link(Some("700000000000000002"))).await)["title"], "❌  Not Allowed");

    *api.invite_guild.lock().unwrap() = Some(Err(ApiError::new(Some(10006), "Unknown Invite")));
    for (code, title) in [(30016, "❌  Invite Limit Reached"), (50013, "❌  Missing Permissions"), (10003, "❌  Channel Not Found"), (99999, "❌  Something Went Wrong")] {
        *api.create_error.lock().unwrap() = Some(code);
        assert_eq!(embed(&route(&t, link(None)).await)["title"], title);
    }
    *api.create_error.lock().unwrap() = None;
    assert_eq!(inv::get_personal_invite(t.db(), G, U).await.unwrap().map(|p| p.code).as_deref(), Some("NEW2"), "failures leave the stored link alone");
    t.reset_guilds(&[G]).await;
}

/// invites.js `createLink` uses the option's resolved channel; one missing from the cache is fetched, not refused.
#[tokio::test]
async fn invites_link_fetches_an_uncached_option_channel() {
    const G: &str = "955010000000000404";
    const FRESH: &str = "700000000000000055";
    let api = FakeDiscord::with(&[]);
    *api.code_prefix.lock().unwrap() = Some("FRESH"); // not NEW1, which invites_link_* saves at the same time
    let Some(url) = database_url() else { return };
    let t = DbTestApp::new(&url).await.with_guilds([G]);
    t.reset_guilds(&[G]).await;
    let rest = bot_core::testing::FakeRest::new();
    rest.on("GET", &format!("/channels/{FRESH}"), 200, json!({ "id": FRESH, "type": 0, "guild_id": G, "name": "fresh", "position": 9, "permission_overwrites": [] }));
    t.set_discord(rest.discord(&[fake_guild(G, TRACK, false)]));
    set_invite_api(&t, api.clone());
    set_trusted_since(&t, 0);
    t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", true)).await.expect("write");
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    t.install(reg);
    let calls = route(&t, slash(G, "link", "100000000000000021").option("channel", option_type::CHANNEL, json!(FRESH))).await;
    assert_eq!(rest.calls_to("GET", &format!("/channels/{FRESH}")).len(), 1);
    assert_eq!(calls.last().and_then(|c| c.body["content"].as_str().map(String::from)).as_deref(), Some("https://discord.gg/FRESH1"), "{:?}", embed(&calls));
    let saved = inv::get_personal_invite(t.db(), G, "100000000000000021").await.expect("read").expect("row");
    assert_eq!(saved.channel_id, FRESH);
    t.reset_guilds(&[G]).await;
}

// ── Cross-language: the JS tracker writes, Rust reads; Rust writes, the JS reads ──

#[tokio::test]
async fn js_and_rust_read_each_others_snapshots_and_join_rows() {
    let (Some(db_url), Some(redis)) = (database_url(), redis_url()) else { return };
    const G: &str = "955010000000000501";
    let t = DbTestApp::with_redis(&db_url, &redis).await.with_guilds([G]);
    t.reset_guilds(&[G]).await;
    t.cache().del_one(&inv::snapshot_key(G)).await;

    let js = run_node("rust/fixtures/gen/feat-logging-js.mjs", &["track", G]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["joined"], json!({ "code": "A", "inviterId": "U1", "vanity": false }));
    // Rust reads the JS snapshot from Redis (equal JSON), the rows and the personal link.
    let stored = t.cache().get(&inv::snapshot_key(G)).await.expect("the JS snapshot");
    let snap = parse_snapshot(&stored).expect("a valid snapshot");
    assert_eq!(snap.to_json(), js["raw"]);
    assert_eq!(snap.codes["A"].uses, 1);
    let stats = inv::get_invite_stats(t.db(), G, "U1").await.expect("stats");
    assert_eq!((stats.total, stats.active), (1, 1));
    let unknown = inv::get_join_record(t.db(), G, "m2").await.expect("read").expect("row");
    assert!(unknown.inviter_id.is_none() && unknown.left_at.is_some());
    assert_eq!(inv::get_personal_invite(t.db(), G, "U1").await.expect("read").map(|p| p.code).as_deref(), Some("JSLINK"));
    // The Rust tracker continues from the JS snapshot once it trusts it.
    let api = FakeDiscord::with(&[("A", 2, 0, Some("U1")), ("ONCE", 0, 1, Some("U2"))]);
    t.set_discord(offline_discord_as(&[fake_guild(G, TRACK, false)]));
    set_invite_api(&t, api);
    set_trusted_since(&t, 0);
    assert_eq!(join(&t, G, "rust-join").await.and_then(|i| i.inviter_id), Some("U1".into()));

    // Rust writes; the JS reads it back through its own service.
    inv::save_personal_invite(&t, G, "U1", "RUSTLINK", "700000000000000002").await.expect("save");
    let back = run_node("rust/fixtures/gen/feat-logging-js.mjs", &["read", G, "U1", &fake_id("rust-join").to_string()]);
    assert!(back.get("error").is_none(), "{back}");
    assert_eq!(back["isSnapshot"], true, "the JS accepts the Rust snapshot");
    let ours = t.cache().get(&inv::snapshot_key(G)).await.expect("snapshot");
    assert_eq!(back["raw"], ours, "byte-for-byte the same JSON");
    assert_eq!(back["stats"], serde_json::to_value(inv::get_invite_stats(t.db(), G, "U1").await.expect("stats")).unwrap());
    assert_eq!(back["record"]["inviterId"], "U1");
    assert_eq!(back["record"]["inviteCode"], "A");
    let rust_row = serde_json::to_value(inv::get_join_record(t.db(), G, &fake_id("rust-join").to_string()).await.expect("read")).unwrap();
    assert_eq!(back["record"], rust_row, "the row reads the same through Prisma and Rust");
    assert_eq!(back["personal"]["code"], "RUSTLINK");
    assert_eq!(back["leaderboard"][0], serde_json::to_value(&inv::get_invite_leaderboard(t.db(), G, 10, &[]).await.expect("lb")[0]).unwrap());
    t.cache().del_one(&inv::snapshot_key(G)).await;
    t.reset_guilds(&[G]).await;
}
