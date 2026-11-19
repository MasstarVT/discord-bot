//! Port of `test/stat-channels.test.js`, plus the JS-sweep, database and cross-language checks.
//!
//! Offline: templates (against the JS), names, channel counts, the snapshot and the values, the rename decision, the
//! next-run time, the small helpers, the filters and the declarations. With `TEST_DATABASE_URL`: the CRUD, the refresh
//! engine against a fake Discord layer (renames, the reservation, aliases, errors, peaks, a cache miss is not a
//! deletion), the job and the hooks, `/stats` through the router, and both directions of the JS round trip.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::clock::Clock;
use bot_core::guild_members::FetchFailure;
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, database_url, run_node};
use bot_db::{StatChannel, StatKind};
use bot_util::placeholder_parser::PlaceholderGuild;
use feat_starboard_stats::discord_io::DiscordError;
use feat_starboard_stats::stats_io::{CreateVoiceChannel, StatsChannel, StatsGuild, StatsIo, StatsMember, set_stats_io};
use feat_starboard_stats::stats_service::{self as stats, ChannelCheck, RefreshNow, RenameDecision, RowLite, StatsError};
use serde_json::{Value, json};

const JS: &str = "rust/fixtures/gen/feat-starboard-stats-js.mjs";
const BOT: &str = "900000000000000001";

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn within(v: i64, lo: i64, hi: i64) -> bool {
    v >= lo && v <= hi
}

fn guild_of(member_count: u64, members: Vec<StatsMember>, presences: &[&str], channels: Vec<StatsChannel>, roles: &[&str], boosts: u64) -> StatsGuild {
    StatsGuild {
        id: "g1".into(),
        name: "Guild".into(),
        available: true,
        member_count,
        members,
        presences: presences.iter().map(|s| s.to_string()).collect(),
        channels,
        roles: roles.iter().map(|r| (r.to_string(), r.to_string())).collect(),
        boosts,
        has_me: true,
        placeholder: Some(PlaceholderGuild { id: "g1".into(), name: Some("Guild".into()), ..Default::default() }),
    }
}

fn mk(bot: bool, roles: &[&str]) -> StatsMember {
    StatsMember { bot, roles: roles.iter().map(|s| s.to_string()).collect() }
}

fn ch(id: &str, kind: u8, thread: bool) -> StatsChannel {
    StatsChannel { id: id.into(), name: format!("c{id}"), kind, is_thread: thread, bot_permissions: None }
}

// ═══════════════════════════ Offline cases ════════════════════════════════════

#[test]
fn t01_validate_stat_template() {
    let g = guild_of(0, vec![], &[], vec![], &[], 0);
    let pg = g.placeholder.as_ref();
    let v = |input: &str| stats::validate_stat_template(input, StatKind::Members, None, pg);
    assert_eq!(v("Members: {count}"), Ok("Members: {count}".into()));
    assert_eq!(v("  Members:   {count}  "), Ok("Members: {count}".into()));
    assert!(v("{COUNT} online").is_ok());
    assert!(v("Members").unwrap_err().contains("{count}"));
    assert!(v("{count} {random}").unwrap_err().contains("Random variables"));
    assert!(v("{role}: {count}").unwrap_err().contains("Role members"));
    assert!(stats::validate_stat_template("{role}: {count}", StatKind::RoleMembers, Some("Mods"), pg).is_ok());
    assert!(stats::validate_stat_template("{role}: {count}", StatKind::RoleMembers, Some("{vip}"), pg).is_ok(), "braces in a role name");
    assert!(v("{count} {user}").unwrap_err().contains("Unknown variable `{user}`"));
    assert!(v("{count} {nope}").unwrap_err().contains("Unknown variable `{nope}`"));
    assert!(v("{server}: {count}").is_ok());
    assert!(v(&format!("{}{{count}}", "x".repeat(94))).unwrap_err().contains("1–100 characters"));
    let long = v(&format!("{}{{count}}", "x".repeat(88))).unwrap_err();
    assert!(long.contains("large count") && long.contains("101"), "{long}");
    assert!(v(&format!("{}{{count}}", "x".repeat(87))).is_ok());
    for k in stats::STAT_KINDS {
        assert!(stats::validate_stat_template(stats::kind_info(*k).1, *k, Some("Mods"), pg).is_ok(), "{k:?}");
    }
}

#[test]
fn t02_render_stat_name() {
    assert_eq!(stats::render_stat_name("Members: {count}", Some(1234), None, None), "Members: 1,234");
    assert_eq!(stats::render_stat_name("Members: {count}", None, None, None), "Members: …");
    assert_eq!(stats::render_stat_name("{role}: {count}", Some(5), Some("Mods"), None), "Mods: 5");
    assert!(stats::render_stat_name("{role}: {count}", Some(5), Some(&"r".repeat(150)), None).chars().count() <= 100);
    assert_eq!(stats::render_stat_name("{count}", Some(0), None, None), "0");
    let g = guild_of(0, vec![], &[], vec![], &[], 0);
    assert_eq!(stats::render_stat_name("{server}: {count}", Some(3), None, g.placeholder.as_ref()), "Guild: 3");
}

/// Templates and names agree with the JS on a sweep (no guild on either side).
#[test]
fn t01b_templates_agree_with_the_js() {
    let long_role = "r".repeat(150);
    let cases: Vec<(String, &str, Option<&str>)> = vec![
        ("Members: {count}".into(), "MEMBERS", None),
        ("  Members:   {count}  ".into(), "MEMBERS", None),
        ("{COUNT} online".into(), "ONLINE", None),
        ("Members".into(), "MEMBERS", None),
        ("{count} {random}".into(), "MEMBERS", None),
        ("{role}: {count}".into(), "MEMBERS", None),
        ("{role}: {count}".into(), "ROLE_MEMBERS", Some("Mods")),
        ("{ROLE}: {count}".into(), "ROLE_MEMBERS", Some("{vip}")),
        ("{count} {user}".into(), "MEMBERS", None),
        ("{server}: {count}".into(), "MEMBERS", None),
        (format!("{}{{count}}", "x".repeat(94)), "MEMBERS", None),
        (format!("{}{{count}}", "x".repeat(88)), "MEMBERS", None),
        (format!("{}{{count}}", "x".repeat(87)), "MEMBERS", None),
        ("{role}: {count}".into(), "ROLE_MEMBERS", Some(long_role.as_str())),
        ("Peak Online: {count}".into(), "PEAK_ONLINE", None),
        ("\u{1F465} {count} \u{2022} {count}".into(), "HUMANS", None),
        ("{count}{{count}}".into(), "BOTS", None),
        ("{ count }".into(), "BOTS", None),
        ("".into(), "BOTS", None),
    ];
    let input: Vec<Value> = cases.iter().map(|(i, k, r)| json!({ "input": i, "kind": k, "roleName": r })).collect();
    let js = run_node(JS, &["templates", &serde_json::to_string(&input).unwrap_or_default()]);
    let js = js.as_array().cloned().unwrap_or_default();
    assert_eq!(js.len(), cases.len(), "{js:?}");
    for ((i, k, r), expected) in cases.iter().zip(js) {
        let kind = StatKind::parse(k).expect("kind");
        let check = match stats::validate_stat_template(i, kind, *r, None) {
            Ok(t) => json!({ "ok": true, "template": t }),
            Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(check, expected["check"], "{i:?}");
        if !i.contains("random") {
            assert_eq!(stats::render_stat_name(i, Some(1234), *r, None), expected["name"], "{i:?}");
            assert_eq!(stats::render_stat_name(i, None, *r, None), expected["loading"], "{i:?}");
        }
    }
}

#[test]
fn t03_count_channels() {
    let channels = vec![ch("1", 0, false), ch("2", 5, false), ch("3", 15, false), ch("4", 2, false), ch("5", 13, false), ch("6", 4, false), ch("7", 11, true)];
    assert_eq!(stats::count_channels(&channels), (3, 2, 5));
}

#[test]
fn t04_build_snapshot_and_compute_value() {
    let value = |kind, snap: &stats::Snapshot, role: Option<&str>, peak: Option<i64>| stats::compute_value(kind, snap, role, peak);
    let members = vec![mk(false, &["r1"]), mk(false, &[]), mk(false, &["r1"]), mk(true, &[]), mk(true, &[])];
    let complete = guild_of(5, members.clone(), &[], vec![], &["g1", "r1"], 0);
    let snap = stats::build_snapshot(&complete, &["r1".into()], None, true);
    assert_eq!(value(StatKind::Members, &snap, None, None).value, Some(5));
    assert_eq!(value(StatKind::Humans, &snap, None, None).value, Some(3));
    assert_eq!(value(StatKind::Bots, &snap, None, None).value, Some(2));
    assert_eq!(value(StatKind::RoleMembers, &snap, Some("r1"), None).value, Some(2));
    let incomplete = stats::build_snapshot(&guild_of(6, members, &[], vec![], &[], 0), &[], None, true);
    assert_eq!(value(StatKind::Humans, &incomplete, None, None), stats::Computed { value: None, problem: Some("members-incomplete") });
    assert_eq!(value(StatKind::Members, &incomplete, None, None).value, Some(6));
    let huge = stats::build_snapshot(&guild_of(150_000, vec![], &[], vec![], &[], 0), &[], None, true);
    assert_eq!(value(StatKind::Humans, &huge, None, None).problem, Some("server-too-large"));
    let gone = stats::build_snapshot(&complete, &["missing".into()], None, true);
    assert_eq!(value(StatKind::RoleMembers, &gone, Some("missing"), None).problem, Some("role-deleted"));
    let p = stats::build_snapshot(&guild_of(4, vec![], &["online", "idle", "dnd", "offline"], vec![], &[], 0), &[], None, true);
    assert_eq!(value(StatKind::Online, &p, None, None).value, Some(3));
    let big = guild_of(80_000, vec![], &[], vec![], &[], 0);
    assert_eq!(value(StatKind::Online, &stats::build_snapshot(&big, &[], Some(1234), true), None, None).value, Some(1234));
    assert_eq!(value(StatKind::Online, &stats::build_snapshot(&big, &[], None, true), None, None).problem, Some("presence-unavailable"));
    // DISABLE_PRESENCES: the empty presence cache is never counted as "nobody online".
    let small = guild_of(4, vec![], &[], vec![], &[], 0);
    assert_eq!(value(StatKind::Online, &stats::build_snapshot(&small, &[], Some(2), false), None, None).value, Some(2));
    assert_eq!(value(StatKind::Online, &stats::build_snapshot(&small, &[], None, false), None, None).problem, Some("presence-unavailable"));
    let online = |n: usize| stats::build_snapshot(&guild_of(n as u64, vec![], &vec!["online"; n], vec![], &[], 0), &[], None, true);
    let unavailable = stats::build_snapshot(&big, &[], None, true);
    assert_eq!(value(StatKind::PeakOnline, &online(7), None, Some(10)).value, Some(10));
    assert_eq!(value(StatKind::PeakOnline, &online(12), None, Some(10)).value, Some(12));
    assert_eq!(value(StatKind::PeakOnline, &unavailable, None, None).problem, Some("presence-unavailable"));
    assert_eq!(value(StatKind::PeakOnline, &unavailable, None, Some(9)).value, Some(9));
    let roles = stats::build_snapshot(&guild_of(0, vec![], &[], vec![], &["e", "a", "b", "c"], 0), &[], None, true);
    assert_eq!(value(StatKind::Roles, &roles, None, None).value, Some(3));
    assert_eq!(value(StatKind::Boosts, &roles, None, None).value, Some(0));
}

#[test]
fn t05_t06_t07_member_cache_rename_and_next_run() {
    let members = |n| vec![mk(false, &[]); n];
    assert!(stats::is_member_cache_complete(&guild_of(5, members(5), &[], vec![], &[], 0)));
    assert!(!stats::is_member_cache_complete(&guild_of(5, members(4), &[], vec![], &[], 0)));
    assert!(stats::is_member_cache_complete(&guild_of(5, members(6), &[], vec![], &[], 0)));

    let now = 1_000_000_000;
    assert_eq!(stats::decide_rename("a", "a", None, None, now), RenameDecision::Same);
    assert_eq!(stats::decide_rename("a", "b", None, None, now), RenameDecision::Rename);
    let four = now - 4 * 60_000;
    assert_eq!(stats::decide_rename("a", "b", None, Some(four), now), RenameDecision::Wait { retry_at: four + 300_000 });
    assert_eq!(stats::decide_rename("a", "b", None, Some(now - 5 * 60_000), now), RenameDecision::Rename);
    assert_eq!(stats::decide_rename("A", "a", Some(&("A".into(), "a".into())), Some(four), now), RenameDecision::Same);

    assert_eq!(stats::compute_next_run_at(0, &[], false), 600_000);
    assert_eq!(stats::compute_next_run_at(0, &[120_000], false), 125_000);
    assert_eq!(stats::compute_next_run_at(0, &[10_000], false), 60_000, "the 60 s floor");
    assert_eq!(stats::compute_next_run_at(0, &[], true), 60_000);
    assert_eq!(stats::compute_next_run_at(0, &[900_000], false), 600_000);
}

#[tokio::test]
async fn t08_small_helpers() {
    assert_eq!(stats::parse_channel_ref("<#123456789012345678>").as_deref(), Some("123456789012345678"));
    assert_eq!(stats::parse_channel_ref("123456789012345678").as_deref(), Some("123456789012345678"));
    assert_eq!(stats::parse_channel_ref("abc"), None);
    assert_eq!(stats::parse_channel_ref("<#12>"), None);
    let o = stats::build_overwrites("g", "b");
    assert_eq!(o.len(), 2);
    assert_eq!(o[0].to_json(), json!({ "id": "g", "type": 0, "allow": "0", "deny": "1048576" }));
    assert_eq!(o[1].to_json(), json!({ "id": "b", "type": 1, "allow": "1040", "deny": "0" }));
    assert_eq!(stats::refresh_job_key("g"), "stats.refresh:g");

    let t = TestApp::new();
    assert!(stats::claim_pull(&t, "g", 0));
    assert!(!stats::claim_pull(&t, "g", 30_000));
    assert!(stats::claim_pull(&t, "g", 61_000));

    let (tx, rx) = tokio::sync::oneshot::channel::<&str>();
    let app = t.app.clone();
    let first = tokio::spawn(async move { stats::run_exclusive(&app, "guild", || async move { rx.await.unwrap_or("dropped") }).await });
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(stats::run_exclusive(&t, "guild", || async { 1 }).await, None, "busy");
    let _ = tx.send("done");
    assert_eq!(first.await.expect("joined"), Some("done"));
    assert_eq!(stats::run_exclusive(&t, "guild", || async { 2 }).await, Some(2), "released after the run");

    assert_eq!(stats::describe_problem("nope"), stats::describe_problem("error"));
    assert_eq!(stats::describe_problem("constructor"), stats::describe_problem("error"));
    assert_eq!(stats::clamp_int(-5.0), 0);
    assert_eq!(stats::clamp_int(3e12), 2_147_483_647);
    assert_eq!(stats::clamp_int(7.9), 7);
    assert_eq!(stats::clamp_int(f64::NAN), 0);
    assert_eq!(stats::normalize_template("  a \n b "), "a b");

    // __resetState
    stats::claim_pull(&t, "g2", 0);
    stats::reset_state(&t);
    assert!(stats::claim_pull(&t, "g2", 30_000), "pending pulls are cleared");
    stats::set_rows_memo(&t, "g", vec![RowLite { id: "1".into(), channel_id: "1".into(), kind: StatKind::Members, role_id: None }], None);
    assert!(stats::member_event_filter(&t, "g"));
    stats::reset_state(&t);
    assert!(stats::member_event_filter(&t, "g"), "no memo fails open");
}

#[tokio::test]
async fn t09_kinds_and_filters() {
    let schema = std::fs::read_to_string(bot_core::testing::repo_root().join("prisma").join("schema.prisma")).expect("schema");
    let body = schema.split("enum StatKind {").nth(1).and_then(|s| s.split('}').next()).unwrap_or("");
    let values: Vec<&str> = body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("//")).collect();
    assert_eq!(stats::STAT_KINDS.iter().map(|k| k.as_str()).collect::<Vec<_>>(), values);

    let t = TestApp::new();
    let row = |kind| RowLite { id: "1".into(), channel_id: "c9".into(), kind, role_id: None };
    assert!(stats::member_event_filter(&t, "unknown"));
    stats::set_rows_memo(&t, "gx", vec![], None);
    assert!(!stats::member_event_filter(&t, "gx"));
    stats::set_rows_memo(&t, "gx", vec![row(StatKind::TextChannels)], None);
    assert!(!stats::member_event_filter(&t, "gx"));
    stats::set_rows_memo(&t, "gx", vec![row(StatKind::Members)], None);
    assert!(stats::member_event_filter(&t, "gx"));
    stats::set_rows_memo(&t, "gx", vec![], Some(t.clock.now_ms() - 16 * 60_000));
    assert!(stats::member_event_filter(&t, "gx"), "an expired memo fails open");

    assert!(!stats::channel_delete_filter(&t, "gy", 0, "c1"));
    stats::set_rows_memo(&t, "gy", vec![row(StatKind::Members)], None);
    assert!(!stats::channel_delete_filter(&t, "gy", 2, "c1"), "a voice channel that is not a counter");
    assert!(stats::channel_delete_filter(&t, "gy", 2, "c9"));
    assert!(stats::channel_delete_filter(&t, "unknown", 2, "c1"), "an unknown memo fails open");
    assert!(stats::channel_delete_filter(&t, "no-settings", 4, "cat"), "no settings memo");
}

#[test]
fn t10_t11_declarations() {
    let mut reg = Registry::new();
    feat_starboard_stats::register(&mut reg);
    bot_core::testing::assert_plugin_matches_golden(&reg, "stat-channels");
    let p = reg.plugin_named("stat-channels").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Stats));
    let json = feat_starboard_stats::commands::stats::command().payload();
    let subs: Vec<&str> = json["options"].as_array().into_iter().flatten().filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(subs, vec!["create", "edit", "delete", "list", "refresh", "category"]);
    let choices: Vec<&str> = json["options"][0]["options"][0]["choices"].as_array().into_iter().flatten().filter_map(|c| c["value"].as_str()).collect();
    assert_eq!(choices, stats::STAT_KINDS.iter().map(|k| k.as_str()).collect::<Vec<_>>());
    assert_eq!(json["default_member_permissions"], "32");
    let meta = feat_starboard_stats::commands::stats::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("stats"), json!(false), json!(2)));
    assert_eq!(stats::RAW_SQL.len(), 10);
}

// ── The fake Discord layer ───────────────────────────────────────────────────

type During = Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
enum Rename {
    Normal,
    Lower,
    Fail(DiscordError),
    During(During),
}

#[derive(Default)]
struct Fake {
    guild: Mutex<StatsGuild>,
    no_perms: Mutex<HashSet<String>>,
    renames: Mutex<HashMap<String, Rename>>,
    renamed: Mutex<Vec<(String, String, String)>>,
    created: Mutex<Vec<CreateVoiceChannel>>,
    create_error: Mutex<Option<DiscordError>>,
    deleted: Mutex<Vec<(String, String)>>,
    delete_error: Mutex<Option<DiscordError>>,
    fetch_override: Mutex<HashMap<String, Result<Option<StatsChannel>, DiscordError>>>,
    fetch_default: Mutex<Option<DiscordError>>,
    approx: Mutex<Option<Result<Option<u64>, DiscordError>>>,
    on_fetch_members: Mutex<Option<Result<Vec<StatsMember>, FetchFailure>>>,
    member_fetches: AtomicU64,
    next_channel_id: Mutex<Option<String>>,
}

fn l<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static CHAN: AtomicU64 = AtomicU64::new(1);

fn cid() -> String {
    format!("9550220100{:08}", CHAN.fetch_add(1, Ordering::SeqCst))
}

impl Fake {
    fn new(id: &str, humans: usize, bots: usize) -> Arc<Self> {
        let f = Fake::default();
        let mut members = vec![mk(false, &[]); humans];
        members.extend(vec![mk(true, &[]); bots]);
        *l(&f.guild) = StatsGuild {
            id: id.into(),
            name: "Test Guild".into(),
            available: true,
            member_count: (humans + bots) as u64,
            members,
            presences: Vec::new(),
            channels: Vec::new(),
            roles: vec![(id.to_string(), "@everyone".to_string())],
            boosts: 0,
            has_me: true,
            placeholder: Some(PlaceholderGuild { id: id.into(), name: Some("Test Guild".into()), ..Default::default() }),
        };
        Arc::new(f)
    }
    fn add_channel(&self, name: &str) -> String {
        let id = cid();
        l(&self.guild).channels.push(StatsChannel { id: id.clone(), name: name.into(), kind: 2, is_thread: false, bot_permissions: None });
        id
    }
    fn name_of(&self, id: &str) -> Option<String> {
        l(&self.guild).channel(id).map(|c| c.name.clone())
    }
    fn uncache(&self, id: &str) {
        l(&self.guild).channels.retain(|c| c.id != id);
    }
    fn add_role(&self, id: &str, name: &str) {
        l(&self.guild).roles.push((id.into(), name.into()));
    }
}

#[async_trait::async_trait]
impl StatsIo for Fake {
    fn guild(&self, _app: &App, guild_id: &str) -> Option<StatsGuild> {
        let g = l(&self.guild).clone();
        (g.id == guild_id).then_some(g)
    }
    fn bot_user_id(&self, _app: &App) -> Option<String> {
        Some(BOT.into())
    }
    fn missing_bot_permissions(&self, _app: &App, _g: &str, channel_id: Option<&str>, perms: &[&str]) -> Vec<String> {
        let blocked = l(&self.no_perms).contains(channel_id.unwrap_or("guild"));
        if blocked { perms.iter().map(|p| p.to_string()).collect() } else { Vec::new() }
    }
    async fn fetch_channel(&self, _app: &App, _g: &str, channel_id: &str) -> Result<Option<StatsChannel>, DiscordError> {
        if let Some(r) = l(&self.fetch_override).get(channel_id).cloned() {
            return r;
        }
        if let Some(e) = l(&self.fetch_default).clone() {
            return Err(e);
        }
        l(&self.guild).channel(channel_id).cloned().map(Some).ok_or_else(|| DiscordError::code(10003))
    }
    async fn rename_channel(&self, _app: &App, _g: &str, channel_id: &str, name: &str, reason: &str) -> Result<Option<String>, DiscordError> {
        let behaviour = l(&self.renames).get(channel_id).cloned().unwrap_or(Rename::Normal);
        let applied = match behaviour {
            Rename::Fail(e) => return Err(e),
            Rename::Lower => name.to_lowercase(),
            Rename::Normal => name.to_string(),
            Rename::During(f) => {
                f().await;
                name.to_string()
            }
        };
        l(&self.renamed).push((channel_id.into(), name.into(), reason.into()));
        let mut g = l(&self.guild);
        let found = g.channels.iter_mut().find(|c| c.id == channel_id).map(|c| {
            c.name = applied.clone();
        });
        if found.is_none() {
            // A channel Discord returns but the cache misses: it is renamed all the same.
            g.channels.push(StatsChannel { id: channel_id.into(), name: applied.clone(), kind: 2, is_thread: false, bot_permissions: None });
        }
        Ok(Some(applied))
    }
    async fn create_channel(&self, _app: &App, _g: &str, req: &CreateVoiceChannel) -> Result<StatsChannel, DiscordError> {
        if let Some(e) = l(&self.create_error).clone() {
            return Err(e);
        }
        l(&self.created).push(req.clone());
        let id = l(&self.next_channel_id)
            .take()
            .unwrap_or_else(|| format!("9550220300{:08}", CHAN.fetch_add(1, Ordering::SeqCst)));
        let c = StatsChannel { id, name: req.name.clone(), kind: 2, is_thread: false, bot_permissions: None };
        l(&self.guild).channels.push(c.clone());
        Ok(c)
    }
    async fn delete_channel(&self, _app: &App, _g: &str, channel_id: &str, reason: &str) -> Result<(), DiscordError> {
        if let Some(e) = l(&self.delete_error).clone() {
            return Err(e);
        }
        l(&self.deleted).push((channel_id.into(), reason.into()));
        l(&self.guild).channels.retain(|c| c.id != channel_id);
        Ok(())
    }
    async fn approximate_presence_count(&self, _app: &App, _g: &str) -> Result<Option<u64>, DiscordError> {
        l(&self.approx).clone().unwrap_or(Ok(None))
    }
    async fn fetch_all_members(&self, _app: &App, _g: &str, _t: u64, _m: u64) -> Result<(), FetchFailure> {
        self.member_fetches.fetch_add(1, Ordering::SeqCst);
        match l(&self.on_fetch_members).clone() {
            None => Ok(()),
            Some(Err(e)) => Err(e),
            Some(Ok(extra)) => {
                l(&self.guild).members.extend(extra);
                Ok(())
            }
        }
    }
}

// ═══════════════════════════ Database cases ═══════════════════════════════════

static GUILD: AtomicU64 = AtomicU64::new(1);

fn new_gid() -> String {
    format!("95502200000000{:04}", GUILD.fetch_add(1, Ordering::SeqCst))
}

async fn db(humans: usize, bots: usize) -> Option<(DbTestApp, String, Arc<Fake>)> {
    let Some(url) = database_url() else {
        println!("skipped: TEST_DATABASE_URL not set");
        return None;
    };
    let g = new_gid();
    let t = DbTestApp::new(&url).await.with_guilds([g.clone()]);
    t.reset_guilds(&[&g]).await;
    let f = Fake::new(&g, humans, bots);
    set_stats_io(&t, f.clone());
    Some((t, g, f))
}

struct Seed {
    kind: StatKind,
    name: &'static str,
    template: Option<String>,
    role_id: Option<String>,
    last_renamed_at: Option<i64>,
    last_value: Option<i32>,
    peak_value: Option<i32>,
}

fn seed(kind: StatKind) -> Seed {
    Seed { kind, name: "old name", template: None, role_id: None, last_renamed_at: None, last_value: None, peak_value: None }
}

async fn insert(t: &App, g: &str, f: &Fake, s: Seed) -> (StatChannel, String) {
    t.settings().ensure_guild_settings(g).await.expect("settings");
    let channel = f.add_channel(s.name);
    let template = s.template.unwrap_or_else(|| stats::kind_info(s.kind).1.to_string());
    let renamed = s.last_renamed_at.map(|ms| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap_or_default().naive_utc());
    let row: StatChannel = sqlx::query_as(
        r#"INSERT INTO "StatChannel" ("id","guildId","channelId","kind","roleId","template","lastValue","lastRenamedAt","peakValue","updatedAt")
           VALUES ($1,$2,$3,$4::"StatKind",$5,$6,$7,$8,$9,(now() AT TIME ZONE 'UTC')) RETURNING *"#,
    )
    .bind(bot_db::new_cuid())
    .bind(g)
    .bind(&channel)
    .bind(s.kind.as_str())
    .bind(s.role_id)
    .bind(template)
    .bind(s.last_value)
    .bind(renamed)
    .bind(s.peak_value)
    .fetch_one(t.db())
    .await
    .expect("row");
    (row, channel)
}

async fn row_of(t: &App, id: &str) -> Option<StatChannel> {
    sqlx::query_as(r#"SELECT * FROM "StatChannel" WHERE "id" = $1"#).bind(id).fetch_optional(t.db()).await.expect("row")
}

async fn job_of(t: &App, g: &str) -> Option<bot_db::ScheduledJob> {
    t.scheduler().get_job(&stats::refresh_job_key(g)).await
}

async fn fresh(t: &App, g: &str) -> Vec<StatChannel> {
    stats::load_rows(t, g).await.expect("rows")
}

async fn refresh(t: &App, g: &str) -> stats::RefreshResult {
    let rows = fresh(t, g).await;
    stats::refresh_guild(t, g, &rows, true, t.clock().now_ms()).await
}

fn code(e: &anyhow::Error) -> Option<&'static str> {
    e.downcast_ref::<StatsError>().map(|s| s.code)
}

#[tokio::test]
async fn db_create_stat_channel_creates_the_channel_row_job_and_memo() {
    let Some((t, g, f)) = db(8, 2).await else { return };
    let made = stats::create_stat_channel(&t, &g, StatKind::Members, "Members: {count}", None, None, "tester#0").await.expect("made");
    assert_eq!(made.value, Some(10));
    let req = l(&f.created)[0].clone();
    assert_eq!(req.name, "Members: 10");
    assert_eq!(req.parent, None);
    assert_eq!(req.overwrites, stats::build_overwrites(&g, BOT));
    assert!(req.reason.contains("tester#0"));
    assert_eq!(req.to_json()["type"], 2);
    let row = row_of(&t, &made.row.id).await.expect("row");
    assert_eq!((row.channel_id.as_str(), row.last_value, row.last_renamed_at), (made.channel.id.as_str(), Some(10), None));
    let job = job_of(&t, &g).await.expect("job");
    assert_eq!(job.status, bot_db::JobStatus::Pending);
    assert_eq!((job.r#type.as_str(), job.payload.clone()), ("stats.refresh", json!({})));
    assert!(within(job.run_at.timestamp_millis() - now_ms(), stats::REFRESH_INTERVAL_MS - 10_000, stats::REFRESH_INTERVAL_MS + 10_000));
    assert!(stats::member_event_filter(&t, &g), "the memo knows the new counter");

    // An unknown value (incomplete member list): an ellipsis, and a run in ~60 s.
    let Some((t2, g2, f2)) = db(3, 0).await else { return };
    l(&f2.guild).member_count = 9;
    let made = stats::create_stat_channel(&t2, &g2, StatKind::Humans, "Humans: {count}", None, None, "t").await.expect("made");
    assert_eq!(made.value, None);
    assert_eq!(l(&f2.created)[0].name, "Humans: …");
    assert_eq!(row_of(&t2, &made.row.id).await.expect("row").last_value, None);
    assert!(within(job_of(&t2, &g2).await.expect("job").run_at.timestamp_millis() - now_ms(), 50_000, 70_000));

    // The full member list is fetched for member-list kinds.
    let Some((t3, g3, f3)) = db(2, 0).await else { return };
    l(&f3.guild).member_count = 5;
    *l(&f3.on_fetch_members) = Some(Ok(vec![mk(true, &[]), mk(true, &[]), mk(false, &[])]));
    let made = stats::create_stat_channel(&t3, &g3, StatKind::Bots, "Bots: {count}", None, None, "t").await.expect("made");
    assert_eq!(made.value, Some(2));
    assert_eq!(l(&f3.created)[0].name, "Bots: 2");

    // Inside a category.
    stats::create_stat_channel(&t3, &g3, StatKind::Boosts, "Boosts: {count}", None, Some("777000000000000001".into()), "t").await.expect("made");
    assert_eq!(l(&f3.created)[1].parent.as_deref(), Some("777000000000000001"));
    assert_eq!(l(&f3.created)[1].name, "Boosts: 0");
    for (app, gid) in [(&t, &g), (&t2, &g2), (&t3, &g3)] {
        app.reset_guilds(&[gid]).await;
    }
}

#[tokio::test]
async fn db_duplicates_the_role_cap_the_guild_cap_and_discord_errors() {
    let Some((t, g, f)) = db(4, 0).await else { return };
    let create = |kind: StatKind, role: Option<(String, String)>| {
        let (t, g) = (t.app.clone(), g.clone());
        async move {
            let template = if kind == StatKind::RoleMembers { "{role}: {count}" } else { stats::kind_info(kind).1 };
            stats::create_stat_channel(&t, &g, kind, template, role, None, "t").await
        }
    };
    let first = create(StatKind::Members, None).await.expect("made");
    let dup = create(StatKind::Members, None).await.unwrap_err();
    let se = dup.downcast_ref::<StatsError>().expect("StatsError");
    assert_eq!((se.code, se.channel_id.as_deref()), ("DUPLICATE", Some(first.channel.id.as_str())));
    create(StatKind::Bots, None).await.expect("made");
    for i in 1..=bot_config::limits::ROLE_STAT_CHANNELS {
        f.add_role(&format!("r{i}"), &format!("Role {i}"));
        create(StatKind::RoleMembers, Some((format!("r{i}"), format!("Role {i}")))).await.expect("made");
    }
    f.add_role("rx", "Role x");
    assert_eq!(code(&create(StatKind::RoleMembers, Some(("rx".into(), "Role x".into()))).await.unwrap_err()), Some("ROLE_LIMIT"));
    t.reset_guilds(&[&g]).await;

    // The same role twice is a duplicate even below the cap.
    let Some((t2, g2, f2)) = db(2, 0).await else { return };
    f2.add_role("ra", "A");
    stats::create_stat_channel(&t2, &g2, StatKind::RoleMembers, "{role}: {count}", Some(("ra".into(), "A".into())), None, "t").await.expect("made");
    let again = stats::create_stat_channel(&t2, &g2, StatKind::RoleMembers, "{role}: {count}", Some(("ra".into(), "A".into())), None, "t").await;
    assert_eq!(again.as_ref().err().and_then(code), Some("DUPLICATE"));

    // Discord errors map to codes, and nothing is saved.
    for (c, expected) in [(50013, "MISSING_PERMISSIONS"), (50001, "MISSING_PERMISSIONS"), (30013, "CHANNEL_LIMIT"), (50035, "DISCORD_REJECTED")] {
        *l(&f2.create_error) = Some(DiscordError::code(c));
        let e = stats::create_stat_channel(&t2, &g2, StatKind::Members, "Members: {count}", None, None, "t").await.unwrap_err();
        assert_eq!(code(&e), Some(expected), "{c}");
    }
    *l(&f2.create_error) = Some(DiscordError::other("boom"));
    let e = stats::create_stat_channel(&t2, &g2, StatKind::Members, "Members: {count}", None, None, "t").await.unwrap_err();
    assert!(code(&e).is_none() && e.to_string() == "boom");
    *l(&f2.create_error) = None;
    assert_eq!(fresh(&t2, &g2).await.len(), 1, "no row after the failures");
    t2.reset_guilds(&[&g2]).await;

    // The guild cap.
    let Some((t3, g3, f3)) = db(2, 0).await else { return };
    for _ in 0..bot_config::limits::STAT_CHANNELS {
        insert(&t3, &g3, &f3, seed(StatKind::Members)).await;
    }
    let e = stats::create_stat_channel(&t3, &g3, StatKind::Roles, "Roles: {count}", None, None, "t").await.unwrap_err();
    assert_eq!(code(&e), Some("LIMIT"));
    t3.reset_guilds(&[&g3]).await;
}

#[tokio::test]
async fn db_a_row_that_cannot_be_saved_deletes_the_new_channel() {
    let Some((t, g, f)) = db(3, 0).await else { return };
    // Discord hands back a channel id a counter already uses (StatChannel.channelId is unique): the insert fails.
    let (_, taken) = insert(&t, &g, &f, seed(StatKind::TextChannels)).await;
    *l(&f.next_channel_id) = Some(taken.clone());
    let e = stats::create_stat_channel(&t, &g, StatKind::Members, "Members: {count}", None, None, "t").await.unwrap_err();
    assert!(code(&e).is_none(), "the original database error: {e}");
    assert_eq!(l(&f.deleted).iter().map(|d| d.0.clone()).collect::<Vec<_>>(), vec![taken.clone()], "the new channel is deleted again");
    assert_eq!(l(&f.deleted)[0].1, "Statistics channel could not be saved");
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&t, &taken).as_deref(), Some("stats"));
    assert_eq!(fresh(&t, &g).await.len(), 1);
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_edit_template_and_delete_stat_channel() {
    let Some((t, g, f)) = db(3, 0).await else { return };
    let (row, _) = insert(&t, &g, &f, seed(StatKind::Members)).await;
    stats::edit_template(&t, &g, &row, "People: {count}").await.expect("edited");
    assert_eq!(row_of(&t, &row.id).await.expect("row").template, "People: {count}");
    assert!(within(job_of(&t, &g).await.expect("job").run_at.timestamp_millis() - now_ms(), 0, 20_000), "pulled to ~5 s");
    sqlx::query(r#"DELETE FROM "StatChannel" WHERE "id" = $1"#).bind(&row.id).execute(t.db()).await.expect("delete");
    assert_eq!(code(&stats::edit_template(&t, &g, &row, "x {count}").await.unwrap_err()), Some("NOT_FOUND"));

    let (a, a_ch) = insert(&t, &g, &f, seed(StatKind::Members)).await;
    let (b, b_ch) = insert(&t, &g, &f, seed(StatKind::Bots)).await;
    stats::ensure_refresh_job(&t, &g, None).await;
    assert!(!stats::delete_stat_channel(&t, &g, &a, true, "t").await.expect("deleted"));
    assert!(f.name_of(&a_ch).is_some(), "the channel is kept");
    assert!(row_of(&t, &a.id).await.is_none());
    assert!(job_of(&t, &g).await.is_some(), "the job stays while a counter is left");
    l(&f.no_perms).insert(b_ch.clone());
    assert_eq!(code(&stats::delete_stat_channel(&t, &g, &b, false, "t").await.unwrap_err()), Some("MISSING_PERMISSIONS"));
    assert!(row_of(&t, &b.id).await.is_some());
    assert!(l(&f.deleted).is_empty());
    l(&f.no_perms).clear();
    *l(&f.delete_error) = Some(DiscordError::code(50013));
    assert_eq!(code(&stats::delete_stat_channel(&t, &g, &b, false, "t").await.unwrap_err()), Some("MISSING_PERMISSIONS"));
    assert!(row_of(&t, &b.id).await.is_some(), "the row survives a refused delete");
    *l(&f.delete_error) = None;
    assert!(stats::delete_stat_channel(&t, &g, &b, false, "tester#0").await.expect("deleted"));
    assert_eq!(bot_core::deletion_tracker::was_deleted_by_bot(&t, &b_ch).as_deref(), Some("stats"));
    assert!(l(&f.deleted)[0].1.contains("tester#0"));
    assert!(row_of(&t, &b.id).await.is_none());
    assert!(job_of(&t, &g).await.is_none(), "no counters left, no job");

    // An uncached channel is deleted by id; a 10003 still removes the row.
    let (c, c_ch) = insert(&t, &g, &f, seed(StatKind::Members)).await;
    f.uncache(&c_ch);
    stats::delete_stat_channel(&t, &g, &c, false, "t").await.expect("deleted");
    assert_eq!(l(&f.deleted).last().map(|d| d.0.clone()), Some(c_ch));
    let (d, d_ch) = insert(&t, &g, &f, seed(StatKind::Bots)).await;
    f.uncache(&d_ch);
    *l(&f.delete_error) = Some(DiscordError::code(10003));
    assert!(!stats::delete_stat_channel(&t, &g, &d, false, "t").await.expect("deleted"));
    assert!(row_of(&t, &d.id).await.is_none());
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_find_row_by_ref_and_list_rows_safe() {
    let Some((t, g, f)) = db(3, 0).await else { return };
    let (a, a_ch) = insert(&t, &g, &f, Seed { name: "Members: 3", ..seed(StatKind::Members) }).await;
    assert_eq!(stats::find_row_by_ref(&t, &g, &a_ch).await.map(|r| r.id), Some(a.id.clone()));
    assert_eq!(stats::find_row_by_ref(&t, &g, &format!("<#{a_ch}>")).await.map(|r| r.id), Some(a.id.clone()));
    assert_eq!(stats::find_row_by_ref(&t, &g, " members: 3 ").await.map(|r| r.id), Some(a.id.clone()));
    assert!(stats::find_row_by_ref(&t, &g, "nothing").await.is_none());
    assert!(stats::find_row_by_ref(&t, &g, "123456789012345678").await.is_none());
    assert!(stats::find_row_by_ref(&t, &g, "").await.is_none());
    assert!(stats::find_row_by_ref(&TestApp::new(), &g, "x").await.is_none(), "never fails");
    let safe = stats::list_rows_safe(&t, &g, 2_000).await;
    assert_eq!(safe, vec![stats::RowRef { channel_id: a_ch, kind: StatKind::Members, role_id: None }]);
    assert!(stats::list_rows_safe(&TestApp::new(), &g, 2_000).await.is_empty());
    t.reset_guilds(&[&g]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_refresh_renames_once_reserves_first_and_respects_the_gap() {
    let Some((t, g, f)) = db(12, 0).await else { return };
    let (row, ch) = insert(&t, &g, &f, Seed { name: "Members: 10", last_value: Some(10), ..seed(StatKind::Members) }).await;
    let r = refresh(&t, &g).await;
    assert_eq!(r.renamed, 1);
    assert_eq!(f.name_of(&ch).as_deref(), Some("Members: 12"));
    assert_eq!(l(&f.renamed)[0].2, "Statistics channel update");
    let after = row_of(&t, &row.id).await.expect("row");
    assert_eq!(after.last_value, Some(12));
    assert!(after.last_renamed_at.is_some_and(|d| now_ms() - d.timestamp_millis() < 10_000));
    assert!(within(r.next_run_at - now_ms(), stats::REFRESH_INTERVAL_MS - 5_000, stats::REFRESH_INTERVAL_MS + 5_000));

    // Up to date: no rename and no write; a stale lastValue is brought up to date only.
    let before = row_of(&t, &row.id).await.expect("row");
    let r = refresh(&t, &g).await;
    assert_eq!((r.unchanged, r.renamed), (1, 0));
    assert_eq!(row_of(&t, &row.id).await.expect("row").updated_at, before.updated_at, "no database write when nothing changed");
    sqlx::query(r#"UPDATE "StatChannel" SET "lastValue" = 5 WHERE "id" = $1"#).bind(&row.id).execute(t.db()).await.expect("u");
    assert_eq!(refresh(&t, &g).await.unchanged, 1);
    assert_eq!(row_of(&t, &row.id).await.expect("row").last_value, Some(12));
    assert_eq!(l(&f.renamed).len(), 1);
    t.reset_guilds(&[&g]).await;

    // A change inside the gap waits, and the trailing update is scheduled.
    let Some((t, g, f)) = db(12, 0).await else { return };
    let renamed_at = now_ms() - 3 * 60_000;
    let (row, ch) = insert(&t, &g, &f, Seed { name: "Members: 10", last_value: Some(10), last_renamed_at: Some(renamed_at), ..seed(StatKind::Members) }).await;
    let r = refresh(&t, &g).await;
    assert_eq!((r.renamed, l(&f.renamed).len(), r.waiting.len()), (0, 0, 1));
    assert_eq!(r.waiting[0].1, renamed_at + stats::MIN_RENAME_GAP_MS);
    assert!((r.next_run_at - (renamed_at + stats::MIN_RENAME_GAP_MS + 5_000)).abs() < 2_000);
    assert_eq!(row_of(&t, &row.id).await.expect("row").last_renamed_at.map(|d| d.timestamp_millis()), Some(renamed_at));
    {
        let mut gd = l(&f.guild);
        gd.members.push(mk(false, &[]));
        gd.member_count += 1;
    }
    sqlx::query(r#"UPDATE "StatChannel" SET "lastRenamedAt" = $2 WHERE "id" = $1"#)
        .bind(&row.id)
        .bind(chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms() - 6 * 60_000).unwrap_or_default().naive_utc())
        .execute(t.db())
        .await
        .expect("u");
    assert_eq!(refresh(&t, &g).await.renamed, 1);
    assert_eq!(f.name_of(&ch).as_deref(), Some("Members: 13"));
    t.reset_guilds(&[&g]).await;

    // Concurrent refreshes rename a channel once (the reservation).
    let Some((t, g, f)) = db(7, 0).await else { return };
    insert(&t, &g, &f, Seed { name: "Members: 1", last_value: Some(1), ..seed(StatKind::Members) }).await;
    let rows = fresh(&t, &g).await;
    let now = t.clock().now_ms();
    let (a, b) = tokio::join!(stats::refresh_guild(&t, &g, &rows, true, now), stats::refresh_guild(&t, &g, &rows, true, now));
    assert_eq!(a.renamed + b.renamed, 1);
    assert_eq!(l(&f.renamed).len(), 1, "exactly one rename");
    assert_eq!(a.waiting.len() + b.waiting.len(), 1, "the loser is reported as waiting");
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_refresh_aliases_errors_peaks_and_online() {
    let Some((t, g, f)) = db(7, 0).await else { return };
    let (row, ch) = insert(&t, &g, &f, Seed { name: "members: 1", ..seed(StatKind::Members) }).await;
    l(&f.renames).insert(ch.clone(), Rename::Lower);
    assert_eq!(refresh(&t, &g).await.renamed, 1);
    assert_eq!(f.name_of(&ch).as_deref(), Some("members: 7"));
    sqlx::query(r#"UPDATE "StatChannel" SET "lastRenamedAt" = NULL WHERE "id" = $1"#).bind(&row.id).execute(t.db()).await.expect("u");
    assert_eq!(refresh(&t, &g).await.unchanged, 1);
    assert_eq!(l(&f.renamed).len(), 1, "no rename loop");
    t.reset_guilds(&[&g]).await;

    // Rename errors.
    let Some((t, g, f)) = db(9, 1).await else { return };
    let (stuck, stuck_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    let (noperm, noperm_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Bots) }).await;
    let (vanished, vanished_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Roles) }).await;
    let (broken, broken_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Boosts) }).await;
    l(&f.renames).insert(stuck_ch.clone(), Rename::Fail(DiscordError::timeout("stats rename")));
    l(&f.renames).insert(noperm_ch.clone(), Rename::Fail(DiscordError::code(50013)));
    l(&f.renames).insert(vanished_ch.clone(), Rename::Fail(DiscordError::code(10003)));
    l(&f.renames).insert(broken_ch.clone(), Rename::Fail(DiscordError::other("boom")));
    let r = refresh(&t, &g).await;
    assert_eq!(r.renamed, 0);
    assert_eq!(r.waiting.iter().map(|w| w.0.clone()).collect::<Vec<_>>(), vec![stuck_ch]);
    assert!(within(r.waiting[0].1 - now_ms(), stats::MIN_RENAME_GAP_MS - 5_000, stats::MIN_RENAME_GAP_MS + 1_000));
    let mut skipped: Vec<(String, &str)> = r.skipped.clone();
    skipped.sort();
    let mut expected = vec![(noperm_ch, "missing-permissions"), (broken_ch, "error")];
    expected.sort();
    assert_eq!(skipped, expected);
    assert_eq!(r.removed, 1);
    assert!(row_of(&t, &vanished.id).await.is_none());
    for kept in [&stuck, &noperm, &broken] {
        assert!(row_of(&t, &kept.id).await.is_some(), "only a 10003 removes a row");
    }
    assert!(row_of(&t, &stuck.id).await.expect("row").last_renamed_at.is_some(), "the reservation counts");
    assert!(row_of(&t, &broken.id).await.expect("row").last_renamed_at.is_some());
    // A rate limit waits for Discord's retry_after.
    l(&f.renames).insert(broken.channel_id.clone(), Rename::Fail(DiscordError::rate_limited(42_000)));
    sqlx::query(r#"UPDATE "StatChannel" SET "lastRenamedAt" = NULL WHERE "id" = $1"#).bind(&broken.id).execute(t.db()).await.expect("u");
    let r = refresh(&t, &g).await;
    let w = r.waiting.iter().find(|w| w.0 == broken.channel_id).expect("waiting");
    assert!(within(w.1 - now_ms(), 40_000, 43_000), "retry after Discord's retry_after");
    t.reset_guilds(&[&g]).await;

    // PEAK_ONLINE only ever increases.
    let Some((t, g, f)) = db(20, 0).await else { return };
    l(&f.guild).presences = vec!["online".into(); 7];
    let (row, ch) = insert(&t, &g, &f, Seed { name: "x", peak_value: Some(10), last_value: Some(10), ..seed(StatKind::PeakOnline) }).await;
    refresh(&t, &g).await;
    assert_eq!(row_of(&t, &row.id).await.expect("row").peak_value, Some(10));
    assert_eq!(f.name_of(&ch).as_deref(), Some("Peak Online: 10"));
    l(&f.guild).presences.extend(vec!["idle".to_string(); 5]);
    l(&f.guild).presences.push("offline".into());
    sqlx::query(r#"UPDATE "StatChannel" SET "lastRenamedAt" = $2 WHERE "id" = $1"#)
        .bind(&row.id)
        .bind(chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms() - 6 * 60_000).unwrap_or_default().naive_utc())
        .execute(t.db())
        .await
        .expect("u");
    refresh(&t, &g).await;
    assert_eq!(row_of(&t, &row.id).await.expect("row").peak_value, Some(12));
    assert_eq!(f.name_of(&ch).as_deref(), Some("Peak Online: 12"));
    l(&f.guild).presences.extend(vec!["dnd".to_string(); 3]);
    let waiting = refresh(&t, &g).await;
    assert_eq!(waiting.waiting.len(), 1);
    assert_eq!(row_of(&t, &row.id).await.expect("row").peak_value, Some(15), "stored while the rename is blocked");
    t.reset_guilds(&[&g]).await;

    // ONLINE: non-offline presences; the approximate count above 75,000 members; no data keeps the name.
    let Some((t, g, f)) = db(5, 0).await else { return };
    l(&f.guild).presences = vec!["online".into(), "online".into(), "online".into(), "offline".into()];
    let (_, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Online) }).await;
    refresh(&t, &g).await;
    assert_eq!(f.name_of(&ch).as_deref(), Some("Online: 3"));
    t.reset_guilds(&[&g]).await;
    let Some((t, g, f)) = db(2, 0).await else { return };
    l(&f.guild).member_count = 80_000;
    *l(&f.approx) = Some(Ok(Some(1234)));
    let (_, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Online) }).await;
    refresh(&t, &g).await;
    assert_eq!(f.name_of(&ch).as_deref(), Some("Online: 1,234"));
    *l(&f.approx) = Some(Err(DiscordError::other("boom")));
    l(&f.guild).channels.iter_mut().for_each(|c| c.name = "keep me".into());
    let r = refresh(&t, &g).await;
    assert_eq!(f.name_of(&ch).as_deref(), Some("keep me"));
    assert_eq!(r.skipped, vec![(ch, "presence-unavailable")]);
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_refresh_member_lists_roles_permissions_budget_and_availability() {
    // A member-list count from an incomplete cache is never shown.
    let Some((t, g, f)) = db(3, 0).await else { return };
    l(&f.guild).member_count = 8;
    let (_, ch) = insert(&t, &g, &f, Seed { name: "Humans: 3", last_value: Some(3), ..seed(StatKind::Humans) }).await;
    let rows = fresh(&t, &g).await;
    let blocked = stats::refresh_guild(&t, &g, &rows, false, t.clock().now_ms()).await;
    assert_eq!(blocked.skipped.iter().map(|s| s.1).collect::<Vec<_>>(), vec!["members-incomplete"]);
    assert_eq!(f.name_of(&ch).as_deref(), Some("Humans: 3"));
    assert!(within(blocked.next_run_at - now_ms(), 50_000, 70_000), "soon");
    *l(&f.on_fetch_members) = Some(Ok(vec![mk(false, &[]), mk(false, &[]), mk(false, &[]), mk(false, &[]), mk(true, &[])]));
    let done = refresh(&t, &g).await;
    assert_eq!(done.renamed, 1);
    assert_eq!(f.name_of(&ch).as_deref(), Some("Humans: 7"));
    t.reset_guilds(&[&g]).await;

    // A rate-limited member fetch backs off and keeps the old name.
    let Some((t, g, f)) = db(3, 0).await else { return };
    l(&f.guild).member_count = 8;
    *l(&f.on_fetch_members) = Some(Err(FetchFailure::RateLimited { retry_after_ms: 45_000 }));
    let (_, ch) = insert(&t, &g, &f, Seed { name: "Bots: 0", ..seed(StatKind::Bots) }).await;
    let r = refresh(&t, &g).await;
    assert_eq!(f.member_fetches.load(Ordering::SeqCst), 1);
    assert_eq!(f.name_of(&ch).as_deref(), Some("Bots: 0"));
    assert_eq!(r.skipped.iter().map(|s| s.1).collect::<Vec<_>>(), vec!["members-incomplete"]);
    assert!(r.next_run_at - now_ms() >= 40_000, "the next run waits for the back-off");
    assert!(stats::fetch_blocked_until_of(&t, &g).is_some_and(|b| b - now_ms() >= 40_000));
    let again = refresh(&t, &g).await;
    assert_eq!(f.member_fetches.load(Ordering::SeqCst), 1, "no second fetch while backed off");
    assert_eq!(again.skipped.len(), 1);
    t.reset_guilds(&[&g]).await;

    // ROLE_MEMBERS counts the role, shows its name, and keeps the name when the role is deleted.
    let Some((t, g, f)) = db(0, 0).await else { return };
    f.add_role("role1", "Mods");
    {
        let mut gd = l(&f.guild);
        gd.members = vec![mk(false, &["role1"]), mk(false, &["role1"]), mk(false, &["role1"]), mk(false, &[])];
        gd.member_count = 4;
    }
    let (row, ch) = insert(&t, &g, &f, Seed { name: "x", role_id: Some("role1".into()), template: Some("{role}: {count}".into()), ..seed(StatKind::RoleMembers) }).await;
    refresh(&t, &g).await;
    assert_eq!(f.name_of(&ch).as_deref(), Some("Mods: 3"));
    l(&f.guild).roles.retain(|(id, _)| id != "role1");
    sqlx::query(r#"UPDATE "StatChannel" SET "lastRenamedAt" = NULL WHERE "id" = $1"#).bind(&row.id).execute(t.db()).await.expect("u");
    let r = refresh(&t, &g).await;
    assert_eq!(r.skipped.iter().map(|s| s.1).collect::<Vec<_>>(), vec!["role-deleted"]);
    assert_eq!(f.name_of(&ch).as_deref(), Some("Mods: 3"));
    assert!(row_of(&t, &row.id).await.is_some());
    t.reset_guilds(&[&g]).await;

    // Without Manage Channels: skipped, not renamed, no slot reserved.
    let Some((t, g, f)) = db(5, 0).await else { return };
    let (row, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    l(&f.no_perms).insert(ch.clone());
    let r = refresh(&t, &g).await;
    assert_eq!(r.skipped, vec![(ch.clone(), "missing-permissions")]);
    assert!(l(&f.renamed).is_empty());
    assert!(row_of(&t, &row.id).await.expect("row").last_renamed_at.is_none(), "no slot was reserved");
    l(&f.no_perms).clear();

    // The time budget defers the remaining counters.
    insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Bots) }).await;
    let rows = fresh(&t, &g).await;
    let r = stats::refresh_guild(&t, &g, &rows, true, t.clock().now_ms() - 41_000).await;
    assert_eq!((r.deferred, r.renamed), (2, 0));
    assert!(within(r.next_run_at - now_ms(), 55_000, 65_000));

    // An unavailable guild is left alone.
    l(&f.guild).available = false;
    f.uncache(&ch);
    let r = refresh(&t, &g).await;
    assert_eq!((r.removed, r.renamed), (0, 0));
    assert!(row_of(&t, &row.id).await.is_some());
    let snapshot = l(&f.guild).clone();
    assert!(matches!(stats::confirm_channel(&t, &snapshot, "x").await, ChannelCheck::Unknown(None)), "never gone while unavailable");
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_rule_20_a_cache_miss_is_not_a_deletion() {
    let Some((t, g, f)) = db(5, 0).await else { return };
    let (gone, gone_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    f.uncache(&gone_ch);
    let r = refresh(&t, &g).await;
    assert_eq!(r.removed, 1, "only after a REST 10003");
    assert!(row_of(&t, &gone.id).await.is_none());

    let (a, a_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    let (b, b_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Bots) }).await;
    f.uncache(&a_ch);
    f.uncache(&b_ch);
    l(&f.fetch_override).insert(a_ch.clone(), Err(DiscordError::code(50001)));
    l(&f.fetch_override).insert(b_ch.clone(), Err(DiscordError { status: Some(503), ..DiscordError::other("Service Unavailable") }));
    let r = refresh(&t, &g).await;
    assert_eq!(r.removed, 0);
    let mut skipped = r.skipped.clone();
    skipped.sort();
    let mut expected = vec![(a_ch.clone(), "missing-permissions"), (b_ch.clone(), "channel-unknown")];
    expected.sort();
    assert_eq!(skipped, expected);
    assert!(row_of(&t, &a.id).await.is_some() && row_of(&t, &b.id).await.is_some());
    assert!(within(r.next_run_at - now_ms(), 55_000, 130_000), "an unchecked channel is looked at again soon");

    // A channel missing from the cache that Discord still returns is renamed normally.
    l(&f.fetch_override).insert(a_ch.clone(), Ok(Some(StatsChannel { id: a_ch.clone(), name: "x".into(), kind: 2, is_thread: false, bot_permissions: None })));
    l(&f.fetch_override).insert(b_ch.clone(), Ok(Some(StatsChannel { id: b_ch.clone(), name: "x".into(), kind: 2, is_thread: false, bot_permissions: None })));
    let r = refresh(&t, &g).await;
    assert_eq!(r.renamed, 2);
    assert_eq!(f.name_of(&a_ch).as_deref(), Some("Members: 5"));
    // A fetched channel's own permissions decide (`permissionsFor` on the fetched channel), not the cache's.
    let (c, c_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Roles) }).await;
    f.uncache(&c_ch);
    l(&f.fetch_override).insert(c_ch.clone(), Ok(Some(StatsChannel { id: c_ch.clone(), name: "x".into(), kind: 2, is_thread: false, bot_permissions: Some(1 << 10) })));
    let r = refresh(&t, &g).await;
    assert!(r.skipped.contains(&(c_ch.clone(), "missing-permissions")), "{r:?}");
    assert!(row_of(&t, &c.id).await.expect("row").last_renamed_at.is_none());
    t.reset_guilds(&[&g]).await;

    // Create prunes only rows whose channel is confirmed gone, and counts unchecked rows against the caps.
    let Some((t, g, f)) = db(5, 0).await else { return };
    let (dead, dead_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Roles) }).await;
    let (unsure, unsure_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Boosts) }).await;
    f.uncache(&dead_ch);
    f.uncache(&unsure_ch);
    l(&f.fetch_override).insert(unsure_ch.clone(), Err(DiscordError { status: Some(500), ..DiscordError::other("Server Error") }));
    stats::create_stat_channel(&t, &g, StatKind::Members, "Members: {count}", None, None, "t").await.expect("made");
    assert!(row_of(&t, &dead.id).await.is_none(), "a confirmed 10003 is pruned");
    assert!(row_of(&t, &unsure.id).await.is_some(), "an unchecked row is kept");
    let e = stats::create_stat_channel(&t, &g, StatKind::Boosts, "Boosts: {count}", None, None, "t").await.unwrap_err();
    let se = e.downcast_ref::<StatsError>().expect("StatsError");
    assert_eq!((se.code, se.channel_id.as_deref()), ("DUPLICATE", Some(unsure_ch.as_str())));
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_channel_delete_events() {
    let Some((t, g, f)) = db(5, 0).await else { return };
    let (a, a_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    let (b, b_ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Bots) }).await;
    stats::ensure_refresh_job(&t, &g, None).await;
    stats::on_channel_delete(&t, &g, 2, &a_ch, None).await;
    assert!(row_of(&t, &a.id).await.is_none());
    assert!(job_of(&t, &g).await.is_some(), "a counter is left, the job stays");
    stats::on_channel_delete(&t, &g, 2, &b_ch, None).await;
    assert!(row_of(&t, &b.id).await.is_none());
    assert!(job_of(&t, &g).await.is_none());
    stats::on_channel_delete(&t, &g, 2, "zzz", None).await;

    // Deleting the stats category clears statsCategoryId (and only that category).
    t.settings().update_settings(&g, SettingsPatch::new().set("statsCategoryId", "555000000000000001")).await.expect("settings");
    let s = t.settings().get_settings_safe(&g).await;
    stats::on_channel_delete(&t, &g, 4, "555000000000000009", s.clone()).await;
    assert_eq!(t.settings().get_settings_safe(&g).await.and_then(|s| s.stats_category_id.clone()).as_deref(), Some("555000000000000001"));
    stats::on_channel_delete(&t, &g, 4, "555000000000000001", s).await;
    assert_eq!(t.settings().get_settings_safe(&g).await.and_then(|s| s.stats_category_id.clone()), None);
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_ensure_refresh_job_and_pull_refresh() {
    let Some((t, g, _f)) = db(1, 0).await else { return };
    let at = |ms: i64| Some(now_ms() + ms);
    assert!(stats::ensure_refresh_job(&t, &g, at(10 * 60_000)).await);
    let created = job_of(&t, &g).await.expect("job");
    assert_eq!((created.status, created.r#type.as_str()), (bot_db::JobStatus::Pending, "stats.refresh"));
    stats::ensure_refresh_job(&t, &g, at(20 * 60_000)).await;
    assert_eq!(job_of(&t, &g).await.expect("job").run_at, created.run_at, "a later time does not postpone a healthy job");
    stats::ensure_refresh_job(&t, &g, at(2 * 60_000)).await;
    assert!(job_of(&t, &g).await.expect("job").run_at < created.run_at, "an earlier time pulls it forward");
    sqlx::query(r#"UPDATE "ScheduledJob" SET "status" = 'FAILED', "lastError" = 'boom' WHERE "dedupeKey" = $1"#).bind(stats::refresh_job_key(&g)).execute(t.db()).await.expect("u");
    stats::ensure_refresh_job(&t, &g, at(30 * 60_000)).await;
    let revived = job_of(&t, &g).await.expect("job");
    assert_eq!((revived.status, revived.last_error.clone()), (bot_db::JobStatus::Pending, None));
    assert!(within(revived.run_at.timestamp_millis() - now_ms(), 29 * 60_000, 31 * 60_000));

    stats::ensure_refresh_job(&t, &g, at(10 * 60_000)).await;
    sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = $2 WHERE "dedupeKey" = $1"#)
        .bind(stats::refresh_job_key(&g))
        .bind(chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms() + 10 * 60_000).unwrap_or_default().naive_utc())
        .execute(t.db())
        .await
        .expect("u");
    assert!(stats::pull_refresh(&t, &g, 60_000).await);
    let pulled = job_of(&t, &g).await.expect("job");
    assert!(within(pulled.run_at.timestamp_millis() - now_ms(), 50_000, 61_000));
    stats::pull_refresh(&t, &g, 5 * 60_000).await;
    assert_eq!(job_of(&t, &g).await.expect("job").run_at, pulled.run_at, "a later pull is a no-op");
    t.reset_guilds(&[&g]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_the_job_handler_and_the_hooks() {
    let Some((t, g, f)) = db(5, 0).await else { return };
    t.settings().ensure_guild_settings(&g).await.expect("settings");
    assert_eq!(stats::refresh_job_handler(&t, &g).await, None, "no counters: the job is deleted");
    let (_, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    let next = stats::refresh_job_handler(&t, &g).await.expect("next run");
    assert_eq!(f.name_of(&ch).as_deref(), Some("Members: 5"));
    assert!(within(next - now_ms(), stats::REFRESH_INTERVAL_MS - 5_000, stats::REFRESH_INTERVAL_MS + 5_000));
    t.settings().update_settings(&g, SettingsPatch::new().set("statsEnabled", false)).await.expect("settings");
    {
        let mut gd = l(&f.guild);
        gd.members.push(mk(false, &[]));
        gd.member_count += 1;
    }
    let off = stats::refresh_job_handler(&t, &g).await.expect("next run");
    assert!(within(off - now_ms(), stats::REFRESH_INTERVAL_MS - 5_000, stats::REFRESH_INTERVAL_MS + 5_000));
    assert_eq!(f.name_of(&ch).as_deref(), Some("Members: 5"), "module off: no Discord calls");
    t.settings().update_settings(&g, SettingsPatch::new().set("statsEnabled", true)).await.expect("settings");
    l(&f.guild).available = false;
    let unavailable = stats::refresh_job_handler(&t, &g).await.expect("next run");
    assert!(within(unavailable - now_ms(), 4 * 60_000, 6 * 60_000));
    l(&f.guild).available = true;

    // The busy guard: retried in 60 s.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let (a, g2) = (t.app.clone(), g.clone());
    let holder = tokio::spawn(async move { stats::run_exclusive(&a, &g2, || async move { let _ = rx.await; }).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let busy = stats::refresh_job_handler(&t, &g).await.expect("next run");
    assert!(within(busy - now_ms(), 55_000, 65_000));
    let _ = tx.send(());
    let _ = holder.await;

    // A failing database: retry in 5 min.
    let offline = TestApp::new().with_guilds([g.clone()]);
    set_stats_io(&offline, f.clone());
    let out = stats::refresh_job_handler(&offline, &g).await.expect("next run");
    assert!(within(out - offline.clock.now_ms(), 4 * 60_000, 6 * 60_000));

    // refreshNow: empty, the result, busy.
    sqlx::query(r#"DELETE FROM "StatChannel" WHERE "guildId" = $1"#).bind(&g).execute(t.db()).await.expect("d");
    assert_eq!(stats::refresh_now(&t, &g).await.expect("ok"), RefreshNow::Empty);
    let (_, ch2) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    match stats::refresh_now(&t, &g).await.expect("ok") {
        RefreshNow::Done(r) => assert_eq!(r.renamed, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(f.name_of(&ch2).as_deref(), Some("Members: 6"));
    assert!(job_of(&t, &g).await.is_some());
    t.reset_guilds(&[&g]).await;

    // A member event during a run caps the next run.
    let Some((t, g, f)) = db(5, 0).await else { return };
    let (_, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
    let (a, g2) = (t.app.clone(), g.clone());
    l(&f.renames).insert(
        ch.clone(),
        Rename::During(Arc::new(move || {
            let (a, g2) = (a.clone(), g2.clone());
            Box::pin(async move {
                stats::claim_pull(&a, &g2, a.clock().now_ms());
            })
        })),
    );
    let next = stats::refresh_job_handler(&t, &g).await.expect("next run");
    assert!(within(next - now_ms(), 50_000, 70_000), "next run ~60 s after the event ({} ms)", next - now_ms());
    t.reset_guilds(&[&g]).await;

    // An edit or a new counter during a run also caps the next run (pullRefresh / ensureRefreshJob while RUNNING).
    for (expected, ensure) in [(stats::EDIT_PULL_DELAY_MS, false), (stats::MIN_NEXT_RUN_MS, true)] {
        let Some((t, g, f)) = db(5, 0).await else { return };
        let (_, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;
        let (a, g2) = (t.app.clone(), g.clone());
        l(&f.renames).insert(
            ch.clone(),
            Rename::During(Arc::new(move || {
                let (a, g2) = (a.clone(), g2.clone());
                Box::pin(async move {
                    if ensure {
                        stats::ensure_refresh_job(&a, &g2, Some(a.clock().now_ms() + stats::MIN_NEXT_RUN_MS)).await;
                    } else {
                        stats::pull_refresh(&a, &g2, stats::EDIT_PULL_DELAY_MS).await;
                    }
                })
            })),
        );
        stats::ensure_refresh_job(&t, &g, Some(now_ms() - 1_000)).await;
        sqlx::query(r#"UPDATE "ScheduledJob" SET "status" = 'RUNNING' WHERE "dedupeKey" = $1"#).bind(stats::refresh_job_key(&g)).execute(t.db()).await.expect("u");
        let past = job_of(&t, &g).await.expect("job").run_at;
        let next = stats::refresh_job_handler(&t, &g).await.expect("next run");
        assert!(within(next - now_ms(), expected - 2_000, expected + 6_000), "~{expected} ms ({} ms)", next - now_ms());
        assert_eq!(job_of(&t, &g).await.expect("job").run_at, past, "the RUNNING job itself was not moved");
        t.reset_guilds(&[&g]).await;
    }
    // Outside a run nothing is remembered.
    let Some((t, g, _f)) = db(1, 0).await else { return };
    stats::pull_refresh(&t, &g, 5_000).await;
    assert!(stats::pending_pull_of(&t, &g).is_none(), "no pendingPull entry outside a run");

    // onGuildReady keeps the job alive for guilds with counters and primes an empty memo for the rest.
    let Some((t2, with_rows, f2)) = db(5, 0).await else { return };
    insert(&t2, &with_rows, &f2, seed(StatKind::Members)).await;
    stats::on_guild_ready(&t2, &with_rows).await;
    assert!(within(job_of(&t2, &with_rows).await.expect("job").run_at.timestamp_millis() - now_ms(), 25_000, 125_000));
    let Some((t4, without, _f4)) = db(1, 0).await else { return };
    t4.settings().ensure_guild_settings(&without).await.expect("settings");
    stats::on_guild_ready(&t4, &without).await;
    assert!(job_of(&t4, &without).await.is_none());
    assert!(!stats::member_event_filter(&t4, &without), "later member events cost nothing");
    t4.reset_guilds(&[&without]).await;
    sqlx::query(r#"UPDATE "ScheduledJob" SET "status" = 'FAILED' WHERE "dedupeKey" = $1"#).bind(stats::refresh_job_key(&with_rows)).execute(t2.db()).await.expect("u");
    stats::on_guild_ready(&t2, &with_rows).await;
    assert_eq!(job_of(&t2, &with_rows).await.expect("job").status, bot_db::JobStatus::Pending, "a FAILED job is repaired");

    // onMemberChange pulls once per minute and ignores guilds without member-driven counters.
    stats::ensure_refresh_job(&t2, &with_rows, Some(now_ms() + 10 * 60_000)).await;
    sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = $2 WHERE "dedupeKey" = $1"#)
        .bind(stats::refresh_job_key(&with_rows))
        .bind(chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms() + 10 * 60_000).unwrap_or_default().naive_utc())
        .execute(t2.db())
        .await
        .expect("u");
    stats::on_member_change(&t2, &with_rows).await;
    assert!(within(job_of(&t2, &with_rows).await.expect("job").run_at.timestamp_millis() - now_ms(), 50_000, 61_000));
    sqlx::query(r#"UPDATE "ScheduledJob" SET "runAt" = $2 WHERE "dedupeKey" = $1"#)
        .bind(stats::refresh_job_key(&with_rows))
        .bind(chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms() + 9 * 60_000).unwrap_or_default().naive_utc())
        .execute(t2.db())
        .await
        .expect("u");
    stats::on_member_change(&t2, &with_rows).await;
    assert!(job_of(&t2, &with_rows).await.expect("job").run_at.timestamp_millis() - now_ms() > 8 * 60_000, "coalesced");
    let Some((t3, other, f3)) = db(5, 0).await else { return };
    insert(&t3, &other, &f3, seed(StatKind::TextChannels)).await;
    stats::on_member_change(&t3, &other).await;
    assert!(job_of(&t3, &other).await.is_none());
    for (app, gid) in [(&t, &g), (&t2, &with_rows), (&t3, &other)] {
        app.reset_guilds(&[gid]).await;
    }
}

// ── /stats through the router ─────────────────────────────────────────────────

fn admin(sub: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash("stats").guild(g).user("700000000000000001").permissions(serenity::all::Permissions::MANAGE_GUILD).subcommand(sub)
}

fn last(t: &DbTestApp) -> Value {
    t.responses.calls().last().map(|c| c.body["embeds"][0].clone()).unwrap_or(Value::Null)
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_starboard_stats::register(&mut reg);
    reg
}

#[tokio::test]
async fn db_stats_command_validation_comes_before_discord() {
    use bot_core::command::option_type::{CHANNEL, ROLE, STRING};
    let Some((t, g, f)) = db(5, 0).await else { return };
    t.install(registry());
    let run = |i| bot_core::router::route_interaction(&t, i);
    run(admin("create", &g).option("kind", STRING, json!("ROLE_MEMBERS")).build()).await;
    assert_eq!(t.responses.names()[0], "deferReply");
    assert!(!t.responses.calls()[0].ephemeral());
    assert_eq!(last(&t)["title"], "❌  Role Required");
    f.add_role("700000000000000099", "R");
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).option("role", ROLE, json!("700000000000000099")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Role Not Used");
    run(admin("create", &g).option("kind", STRING, json!("ROLE_MEMBERS")).option("role", ROLE, json!(g)).build()).await;
    assert_eq!(last(&t)["title"], "❌  Invalid Role");
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).option("template", STRING, json!("no count here")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Invalid Template");
    l(&f.guild).member_count = stats::MEMBER_LIST_MAX_MEMBERS + 1;
    run(admin("create", &g).option("kind", STRING, json!("HUMANS")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Server Too Large");
    assert_eq!(
        last(&t)["description"],
        "**Humans** counters need the full member list, which I only load for servers with up to 100,000 members. Use **Members** instead."
    );
    l(&f.guild).member_count = 5;
    assert!(l(&f.created).is_empty());
    l(&f.no_perms).insert("guild".into());
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Missing Permissions");
    assert_eq!(last(&t)["description"], "I need **View Channel, Manage Channels, Connect** in this server to create counter channels.");
    l(&f.no_perms).clear();
    l(&f.guild).channels.push(StatsChannel { id: "830000000000000001".into(), name: "Cat".into(), kind: 4, is_thread: false, bot_permissions: None });
    l(&f.no_perms).insert("830000000000000001".into());
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).option("category", CHANNEL, json!("830000000000000001")).build()).await;
    assert_eq!(last(&t)["description"], "I need **View Channel, Manage Channels** in the **Cat** category.");
    assert!(l(&f.created).is_empty());
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_stats_command_end_to_end() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, ROLE, STRING};
    let Some((t, g, f)) = db(8, 1).await else { return };
    t.install(registry());
    t.settings().ensure_guild_settings(&g).await.expect("settings");
    let run = |i| bot_core::router::route_interaction(&t, i);

    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).build()).await;
    let e = last(&t);
    assert_eq!(e["title"], "✅  Counter Created");
    let channel = l(&f.created)[0].clone();
    assert_eq!(channel.name, "Members: 9");
    let members_ch = l(&f.guild).channels.iter().find(|c| c.name == "Members: 9").map(|c| c.id.clone()).expect("channel");
    assert_eq!(
        e["description"],
        format!("<#{members_ch}> now shows **Members**.\n**Template:** `Members: {{count}}`\nIt updates about every 10 minutes; Discord allows only 2 renames per channel every 10 minutes.")
    );
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Already Exists");
    assert_eq!(last(&t)["description"], format!("<#{members_ch}> already counts **Members**. Change its name with `/stats edit` instead."));

    f.add_role("700000000000000042", "Staff");
    run(admin("create", &g).option("kind", STRING, json!("ROLE_MEMBERS")).option("template", STRING, json!("{role} online: {count}")).option("role", ROLE, json!("700000000000000042")).build()).await;
    assert_eq!(last(&t)["title"], "✅  Counter Created");
    assert!(last(&t)["description"].as_str().unwrap_or("").contains("for <@&700000000000000042>"));
    assert_eq!(l(&f.created)[1].name, "Staff online: 0");

    run(admin("list", &g).build()).await;
    let list = last(&t);
    assert_eq!(list["title"], "ℹ️  Statistics Channels");
    assert_eq!(list["description"], "2/20 counters · 1/8 role counters\nNew counters go to: top level\nNames refresh about every 10 minutes.");
    let value = list["fields"][0]["value"].as_str().unwrap_or("").to_string();
    assert_eq!(list["fields"][0]["name"], "Counters");
    assert!(value.starts_with(&format!("**Members** · <#{members_ch}> · `Members: {{count}}` · showing **9** · not renamed yet\n")), "{value}");
    assert!(value.contains("**Role members (<@&700000000000000042>)**"));

    run(admin("edit", &g).option("channel", STRING, json!(members_ch)).option("template", STRING, json!("People: {count}")).build()).await;
    assert_eq!(last(&t)["title"], "✅  Counter Updated");
    assert_eq!(
        last(&t)["description"],
        format!("<#{members_ch}> will be renamed to **People: 9** on the next refresh (usually within a minute, or up to 5 minutes if it was renamed recently).")
    );
    run(admin("edit", &g).option("channel", STRING, json!(members_ch)).option("template", STRING, json!("nope")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Invalid Template");
    run(admin("edit", &g).option("channel", STRING, json!("123456789012345678")).option("template", STRING, json!("x {count}")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Not Found");

    run(admin("refresh", &g).build()).await;
    assert_eq!(last(&t)["title"], "✅  Counters Refreshed");
    assert!(last(&t)["description"].as_str().unwrap_or("").starts_with("✅ Renamed: 1"));

    l(&f.guild).channels.push(StatsChannel { id: "888000000000000001".into(), name: "Stats".into(), kind: 4, is_thread: false, bot_permissions: None });
    run(admin("category", &g).option("category", CHANNEL, json!("888000000000000001")).build()).await;
    assert_eq!(last(&t)["title"], "✅  Stats Category Set");
    assert_eq!(last(&t)["description"], "New counters will be created in **Stats**. Existing counters stay where they are.");
    assert_eq!(t.settings().get_settings_safe(&g).await.and_then(|s| s.stats_category_id.clone()).as_deref(), Some("888000000000000001"));
    run(admin("list", &g).build()).await;
    assert!(last(&t)["description"].as_str().unwrap_or("").contains("<#888000000000000001>"));
    run(admin("category", &g).build()).await;
    assert_eq!(last(&t)["title"], "✅  Stats Category Cleared");
    assert_eq!(t.settings().get_settings_safe(&g).await.and_then(|s| s.stats_category_id.clone()), None);

    run(admin("delete", &g).option("channel", STRING, json!(members_ch)).option("keep_channel", BOOLEAN, json!(true)).build()).await;
    assert_eq!(last(&t)["description"], format!("The **Members** counter was removed. <#{members_ch}> was kept and no longer updates."));
    let role_row = fresh(&t, &g).await.into_iter().find(|r| r.kind == StatKind::RoleMembers).expect("row");
    run(admin("delete", &g).option("channel", STRING, json!(role_row.channel_id)).build()).await;
    assert_eq!(last(&t)["description"], "The **Role members (<@&700000000000000042>)** counter was removed and its channel deleted.");
    run(admin("list", &g).build()).await;
    assert!(last(&t)["description"].as_str().unwrap_or("").ends_with("\n\nNo counters yet. Create one with `/stats create`."));
    assert!(job_of(&t, &g).await.is_none());

    // The module off note, and refresh refuses while the module is off.
    t.settings().update_settings(&g, SettingsPatch::new().set("statsEnabled", false)).await.expect("settings");
    run(admin("create", &g).option("kind", STRING, json!("BOOSTS")).build()).await;
    assert!(last(&t)["description"].as_str().unwrap_or("").ends_with(feat_starboard_stats::commands::stats::MODULE_OFF_NOTE));
    run(admin("refresh", &g).build()).await;
    assert_eq!(last(&t)["title"], "⚠️  Module Disabled");
    t.settings().update_settings(&g, SettingsPatch::new().set("statsEnabled", true)).await.expect("settings");

    // Refresh lists skipped counters and the rename limit wait.
    sqlx::query(r#"DELETE FROM "StatChannel" WHERE "guildId" = $1"#).bind(&g).execute(t.db()).await.expect("d");
    insert(&t, &g, &f, Seed { name: "x", last_renamed_at: Some(now_ms() - 60_000), ..seed(StatKind::Members) }).await;
    let (_, bots) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Bots) }).await;
    l(&f.no_perms).insert(bots.clone());
    run(admin("refresh", &g).build()).await;
    let d = last(&t)["description"].as_str().unwrap_or("").to_string();
    assert!(d.contains("⏳ Waiting for Discord's rename limit: 1 (next <t:"), "{d}");
    assert!(d.contains(&format!("⚠️ <#{bots}>: I need View Channel and Manage Channels on this channel")));
    l(&f.no_perms).clear();

    // Autocomplete: channel name, kind and role, filtered.
    sqlx::query(r#"DELETE FROM "StatChannel" WHERE "guildId" = $1"#).bind(&g).execute(t.db()).await.expect("d");
    f.add_role("700000000000000043", "Crew");
    let (_, a) = insert(&t, &g, &f, Seed { name: "Members: 3", ..seed(StatKind::Members) }).await;
    let (_, b) = insert(&t, &g, &f, Seed { name: "Crew: 1", role_id: Some("700000000000000043".into()), template: Some("{role}: {count}".into()), ..seed(StatKind::RoleMembers) }).await;
    let ac = |q: &str| FakeInteraction::autocomplete("stats").guild(&g).user("700000000000000001").permissions(serenity::all::Permissions::MANAGE_GUILD).subcommand("edit").focused("channel", q).build();
    t.responses.clear();
    run(ac("")).await;
    let all = t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(all, vec![json!({ "name": "Members: 3 — Members", "value": a }), json!({ "name": "Crew: 1 — Role members (@Crew)", "value": b })]);
    run(ac("CREW")).await;
    let filtered = t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(filtered, vec![json!({ "name": "Crew: 1 — Role members (@Crew)", "value": b })]);
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_stats_create_and_the_saved_category() {
    use bot_core::command::option_type::STRING;
    let Some((t, g, f)) = db(5, 0).await else { return };
    t.install(registry());
    t.settings().update_settings(&g, SettingsPatch::new().set("statsCategoryId", "444000000000000001")).await.expect("settings");
    let run = |i| bot_core::router::route_interaction(&t, i);
    *l(&f.fetch_default) = Some(DiscordError { status: Some(500), ..DiscordError::other("Server Error") });
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).build()).await;
    assert_eq!(last(&t)["title"], "❌  Try Again");
    assert_eq!(t.settings().get_settings_safe(&g).await.and_then(|s| s.stats_category_id.clone()).as_deref(), Some("444000000000000001"));
    assert!(l(&f.created).is_empty());
    *l(&f.fetch_default) = None;
    run(admin("create", &g).option("kind", STRING, json!("MEMBERS")).build()).await;
    assert_eq!(last(&t)["title"], "✅  Counter Created");
    assert!(last(&t)["description"].as_str().unwrap_or("").contains("The saved stats category no longer exists, so the channel was created at the top level."));
    assert_eq!(l(&f.created)[0].parent, None);
    let cleared = {
        let mut ok = false;
        for _ in 0..50 {
            t.settings().invalidate_settings(&g).await;
            if t.settings().get_settings_safe(&g).await.and_then(|s| s.stats_category_id.clone()).is_none() {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        ok
    };
    assert!(cleared, "the clear is fire-and-forget");
    // A saved category that exists is used.
    l(&f.guild).channels.push(StatsChannel { id: "444000000000000002".into(), name: "Stats".into(), kind: 4, is_thread: false, bot_permissions: None });
    t.settings().update_settings(&g, SettingsPatch::new().set("statsCategoryId", "444000000000000002")).await.expect("settings");
    run(admin("create", &g).option("kind", STRING, json!("ROLES")).build()).await;
    assert_eq!(last(&t)["title"], "✅  Counter Created");
    assert_eq!(l(&f.created)[1].parent.as_deref(), Some("444000000000000002"));
    t.reset_guilds(&[&g]).await;

    // The generic error when something unexpected fails (the database is unreachable).
    let offline = TestApp::new().with_guilds([g.clone()]);
    offline.install(registry());
    set_stats_io(&offline, f.clone());
    bot_core::router::route_interaction(&offline, admin("list", &g).build()).await;
    let calls = offline.responses.calls();
    assert_eq!(calls.last().and_then(|c| c.embed_title()), Some("❌  Something Went Wrong"));
}

// ── Cross-language ────────────────────────────────────────────────────────────

/// JS createStatChannel writes a row and a stats.refresh job; Rust reads and refreshes them. Rust writes a row and a
/// job; the JS reads them, and its refresh respects the Rust rename reservation.
#[tokio::test]
async fn js_and_rust_read_each_others_counters_and_jobs() {
    let Some((t, g, f)) = db(4, 0).await else { return };
    let js = run_node(JS, &["st-create", &g, &json!({ "kind": "HUMANS", "template": "Humans: {count}", "guild": { "humans": 4, "botId": BOT, "newChannelId": "955022019999999901" } }).to_string()]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["value"], 4);
    assert_eq!(js["created"][0]["name"], "Humans: 4");
    let ours = stats::build_overwrites(&g, BOT).iter().map(|o| o.to_json()).collect::<Vec<_>>();
    let theirs: Vec<Value> = js["created"][0]["permissionOverwrites"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| {
            let bits = |k: &str| o[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str()?.parse::<u64>().ok()).fold(0u64, |x, y| x | y)).unwrap_or(0);
            json!({ "id": o["id"], "type": o["type"], "allow": bits("allow").to_string(), "deny": bits("deny").to_string() })
        })
        .collect();
    assert_eq!(theirs, ours, "the same overwrites");
    let rows = fresh(&t, &g).await;
    assert_eq!(serde_json::to_value(&rows[0]).unwrap_or_default(), js["row"], "the row reads the same through Prisma and Rust");
    let job = job_of(&t, &g).await.expect("JS job");
    assert_eq!((job.r#type.as_str(), job.dedupe_key.clone(), job.payload.clone()), ("stats.refresh", Some(stats::refresh_job_key(&g)), json!({})));
    assert_eq!(job.status, bot_db::JobStatus::Pending);
    // Rust refreshes the JS counter.
    l(&f.guild).channels.push(StatsChannel { id: rows[0].channel_id.clone(), name: "Humans: 4".into(), kind: 2, is_thread: false, bot_permissions: None });
    l(&f.guild).members.push(mk(false, &[]));
    l(&f.guild).member_count = 5;
    let r = refresh(&t, &g).await;
    assert_eq!(r.renamed, 1);
    assert_eq!(f.name_of(&rows[0].channel_id).as_deref(), Some("Humans: 5"));

    // The JS reads the Rust rows: its refresh sees the reservation Rust just made and waits.
    let back = run_node(JS, &["st-refresh", &g, &json!({ "humans": 6, "botId": BOT, "channels": [{ "id": rows[0].channel_id, "name": "Humans: 5" }] }).to_string()]);
    assert!(back.get("error").is_none(), "{back}");
    assert_eq!(back["result"]["renamed"], 0);
    assert_eq!(back["result"]["waiting"].as_array().map(Vec::len), Some(1), "the Rust reservation holds in the JS");
    let ours = serde_json::to_value(fresh(&t, &g).await).unwrap_or_default();
    assert_eq!(ours, back["rows"]);

    // Rust writes a job; the JS reads it and keeps it ('earliest').
    t.reset_guilds(&[&g]).await;
    t.settings().ensure_guild_settings(&g).await.expect("settings");
    let at = now_ms() + 3 * 60_000;
    assert!(stats::ensure_refresh_job(&t, &g, Some(at)).await);
    let js_job = run_node(JS, &["st-job", &g]);
    assert_eq!(js_job["type"], "stats.refresh");
    assert_eq!(js_job["payload"], json!({}));
    assert_eq!(js_job["dedupeKey"], stats::refresh_job_key(&g));
    assert_eq!(js_job["status"], "PENDING");
    let later = run_node(JS, &["st-ensure", &g, &(at + 60 * 60_000).to_string()]);
    assert_eq!(later["ok"], true);
    assert_eq!(later["job"]["runAt"], js_job["runAt"], "the JS 'earliest' keeps the Rust run time");
    assert_eq!(serde_json::to_value(job_of(&t, &g).await.expect("job")).ok().map(|v| v["runAt"].clone()), Some(js_job["runAt"].clone()));
    t.reset_guilds(&[&g]).await;
}

// ── The plugin through the hook runner and the job conversion ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_the_plugin_maps_the_gateway_payloads_and_the_job() {
    let Some((t, g, f)) = db(5, 0).await else { return };
    t.install(registry());
    t.settings().get_settings(&g).await.expect("settings memoised for the module gate");
    let gid = serenity::all::GuildId::new(g.parse().unwrap_or(1));
    let (row, ch) = insert(&t, &g, &f, Seed { name: "x", ..seed(StatKind::Members) }).await;

    t.run_hooks(GuildReady { guild_id: gid, reason: GuildReadyReason::Startup }).await;
    let job = job_of(&t, &g).await.expect("guildReady keeps the job alive");
    assert!(within(job.run_at.timestamp_millis() - now_ms(), 25_000, 125_000));

    let user: serenity::all::User =
        serde_json::from_value(json!({ "id": "700000000000000005", "username": "new", "discriminator": "0", "avatar": null })).expect("user");
    let member: serenity::all::Member = serde_json::from_value(json!({
        "user": { "id": "700000000000000005", "username": "new", "discriminator": "0", "avatar": null },
        "guild_id": g, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    }))
    .expect("member");
    t.run_hooks(GuildMemberAdd { guild_id: gid, member, user }).await;
    // The pull keeps the earlier run time ('earliest'): guildReady's 30-120 s jitter may already be under 60 s.
    let before = job.run_at.timestamp_millis() - now_ms();
    let pulled = job_of(&t, &g).await.expect("job").run_at.timestamp_millis() - now_ms();
    assert!(within(pulled, before.min(50_000) - 2_000, 61_000), "pulled to ~60 s (or kept the earlier run): {pulled} ms, was {before} ms");

    // The stats.refresh job handler: a counter reschedules ~10 min out; no counter finishes the job.
    let j = bot_core::scheduler::Job::from_row(&job_of(&t, &g).await.expect("job"));
    let ctx = || JobCtx { app: t.app.clone(), guild_id: g.clone(), late_by_ms: 0, attempt: 1 };
    match feat_starboard_stats::plugins::stat_channels::refresh_job(j.clone(), ctx()).await.expect("ran") {
        JobOutcome::Reschedule { next_run_at, payload } => {
            assert!(within(next_run_at.timestamp_millis() - now_ms(), stats::REFRESH_INTERVAL_MS - 5_000, stats::REFRESH_INTERVAL_MS + 5_000));
            assert_eq!(payload, None, "the payload stays as it is");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(f.name_of(&ch).as_deref(), Some("Members: 5"));

    // A deleted voice counter (always, even with the module off) removes the row and, with the last one, the job.
    t.settings().update_settings(&g, SettingsPatch::new().set("statsEnabled", false)).await.expect("settings");
    let channel: serenity::all::GuildChannel = serde_json::from_value(json!({
        "id": ch, "guild_id": g, "type": 2, "name": "Members: 5", "position": 0, "permission_overwrites": [], "nsfw": false
    }))
    .expect("channel");
    t.run_hooks(ChannelDelete { guild_id: gid, channel }).await;
    assert!(row_of(&t, &row.id).await.is_none());
    assert!(job_of(&t, &g).await.is_none());
    assert_eq!(feat_starboard_stats::plugins::stat_channels::refresh_job(j, ctx()).await.expect("ran"), JobOutcome::Done);
    t.reset_guilds(&[&g]).await;
}

/// statsService.js createStatChannel: `guild.channels.create` caches the channel it created, so the new counter is
/// known (to `/stats list`, the refresh and inspectRow) before its CHANNEL_CREATE event arrives.
#[tokio::test]
async fn serenity_io_caches_the_created_counter_channel() {
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use feat_starboard_stats::stats_io::SerenityStatsIo;
    use serenity::all::Permissions;
    const G: &str = "956990000000000001";
    const OWNER: &str = "956990000000000002";
    const NEW: &str = "956990000000000010";
    let guild = FakeGuild::new(G, OWNER).everyone(Permissions::VIEW_CHANNEL).member(TEST_BOT_ID, &[]).channel("956990000000000003", 0, "general", &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
    rest.on(
        "POST",
        &format!("/guilds/{G}/channels"),
        201,
        json!({ "id": NEW, "type": 2, "name": "Members: 3", "guild_id": G, "position": 1, "permission_overwrites": [],
            "nsfw": false, "bitrate": 64000, "user_limit": 0, "rate_limit_per_user": 0, "parent_id": null }),
    );
    let io = SerenityStatsIo;
    assert!(io.guild(&t, G).expect("cached guild").channel(NEW).is_none());
    let req = CreateVoiceChannel { name: "Members: 3".into(), parent: None, overwrites: stats::build_overwrites(G, TEST_BOT_ID), reason: "r".into() };
    let made = io.create_channel(&t, G, &req).await.expect("created");
    assert_eq!(made.id, NEW);
    let cached = io.guild(&t, G).expect("cached guild");
    assert_eq!(cached.channel(NEW).map(|c| c.name.as_str()), Some("Members: 3"), "cached at once, as discord.js does");
}
