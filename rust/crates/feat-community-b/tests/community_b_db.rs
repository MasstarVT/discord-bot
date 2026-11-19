//! The database half of the feat-community-b tests: `ModmailThread` and `BoosterRole` access, the settings lists,
//! the `boosts.sweep` job, the hooks and commands that use them, and both cross-language directions against the JS
//! services (`rust/fixtures/gen/feat-community-b-js.mjs`). Every test returns early without `TEST_DATABASE_URL`.

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, database_url, offline_discord_as, run_node};
use bot_db::{BoosterRole, ModmailStatus, ModmailThread};
use feat_community_b::auto_thread_service as at;
use feat_community_b::boost_service as bs;
use feat_community_b::modmail_service as mm;
use feat_community_b::plugins::modmail as mmp;
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions as P};

const JS: &str = "rust/fixtures/gen/feat-community-b-js.mjs";
const OWNER: &str = "600000000000000099";
const ADMIN: &str = "600000000000000001";
const MEMBER: &str = "600000000000000002";
const ADMIN2: &str = "600000000000000003";
const STAFF_CH: &str = "810000000000000001";
const ANN_CH: &str = "810000000000000002";
const THREAD: &str = "810000000000000003";
const LOCKED_CH: &str = "810000000000000004";

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_community_b::register(&mut reg);
    reg
}

async fn app(guild: &'static str) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds([guild]);
    t.reset_guilds(&[guild]).await;
    Some(t)
}

fn gid(g: &str) -> GuildId {
    GuildId::new(g.parse().unwrap_or(1))
}

fn js(args: &[&str]) -> Value {
    let v = run_node(JS, args);
    assert!(v.get("error").is_none(), "JS {args:?} answered {v}");
    v
}

fn fake_guild(g: &str, bot_perms: P) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .everyone(P::VIEW_CHANNEL | P::SEND_MESSAGES)
        .role_with("820000000000000001", 1, P::empty(), |r| {
            r["name"] = json!("Pingable");
            r["mentionable"] = json!(true);
        })
        .role_with("820000000000000002", 2, P::empty(), |r| r["name"] = json!("Quiet"))
        .role_with("820000000000000009", 6, bot_perms, |r| r["name"] = json!("Bot"))
        .role("820000000000000010", 10, P::MANAGE_GUILD)
        .member(TEST_BOT_ID, &["820000000000000009"])
        .member(ADMIN, &["820000000000000010"])
        .member(ADMIN2, &["820000000000000010"])
        .member(MEMBER, &[])
        .channel(STAFF_CH, 0, "staff", &[])
        .channel(ANN_CH, 5, "news", &[])
        .channel(LOCKED_CH, 0, "locked", &[(g, false, P::empty(), P::CREATE_PRIVATE_THREADS | P::CREATE_PUBLIC_THREADS)])
        .thread(THREAD, 12, STAFF_CH)
        .thread(&thread_of(g), 12, STAFF_CH)
}

/// A thread of this guild only (`threadId` is unique across guilds, and tests run in parallel).
fn thread_of(g: &str) -> String {
    format!("8160000000000000{}", &g[g.len() - 2..])
}

fn with_discord(t: &DbTestApp, g: &str, bot_perms: P) {
    t.app.set_discord(offline_discord_as(&[fake_guild(g, bot_perms).json()]));
}

fn admin(name: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user(ADMIN).roles(&["820000000000000010"]).permissions(P::MANAGE_GUILD)
}

fn with_user(mut fi: FakeInteraction, id: &str, bot: bool) -> FakeInteraction {
    fi.json_mut()["data"]["resolved"] = json!({ "users": { id: { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": bot } } });
    fi
}

async fn route(t: &DbTestApp, fi: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    t.responses.clear();
    bot_core::router::route_interaction(t, fi.build()).await;
    t.responses.calls()
}

fn last(calls: &[bot_core::interaction::RecordedCall]) -> (String, String) {
    let c = calls.last().expect("an answer");
    (c.embed_title().unwrap_or("").to_string(), c.embed_description().unwrap_or("").to_string())
}

fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

// ── Modmail: the table ────────────────────────────────────────────────────────

#[tokio::test]
async fn reserve_open_keeps_one_open_row_and_replaces_a_stale_placeholder() {
    const G: &str = "940000000000000001";
    let Some(t) = app(G).await else { return };
    let first = match mm::reserve_open(&t, G, "U1").await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("expected a new row, got {other:?}"),
    };
    assert!(mm::is_pending_thread_id(&first.thread_id));
    assert_eq!(first.status, ModmailStatus::Open);
    match mm::reserve_open(&t, G, "U1").await.expect("reserve") {
        mm::Reserved::Existing(r) => assert_eq!(r.id, first.id, "the placeholder being created"),
        other => panic!("{other:?}"),
    }
    assert!(mm::find_open_for_user(t.db(), "U1", 25).await.expect("query").is_empty(), "placeholders are skipped");
    assert!(mm::find_open_in_guild(t.db(), G, "U1").await.expect("query").is_none());

    // A placeholder older than two minutes belongs to an attempt that died: the next open replaces it.
    bot_db::sqlx::query(r#"UPDATE "ModmailThread" SET "createdAt" = "createdAt" - interval '3 minutes' WHERE "id" = $1"#)
        .bind(&first.id)
        .execute(t.db())
        .await
        .expect("age the row");
    let second = match mm::reserve_open(&t, G, "U1").await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    assert_ne!(second.id, first.id);
    assert!(mm::find_in_guild(t.db(), G, &first.id).await.expect("query").is_none(), "the stale row is gone");

    let attached = mm::attach_thread(&t, &second.id, "811000000000000001").await.expect("attach");
    assert_eq!(attached.thread_id, "811000000000000001");
    assert_eq!(mm::find_by_thread(t.db(), "811000000000000001").await.expect("q").map(|r| r.id), Some(second.id.clone()));
    assert_eq!(mm::find_open_in_guild(t.db(), G, "U1").await.expect("q").map(|r| r.id), Some(second.id.clone()));
    assert_eq!(mm::find_open_for_user(t.db(), "U1", 25).await.expect("q").len(), 1);
    assert!(mm::find_in_guild(t.db(), "940000000000000999", &second.id).await.expect("q").is_none(), "another guild's id is not found");
    assert!(mm::find_for_user(t.db(), "U2", &second.id).await.expect("q").is_none(), "someone else's row");
    assert_eq!(mm::list_open_in_guild(t.db(), G, 200).await.expect("q").len(), 1);

    // OPEN -> CLOSED once.
    assert!(mm::close_row(&t, &second.id, Some("S1"), Some(&"r".repeat(600))).await.expect("close"));
    assert!(!mm::close_row(&t, &second.id, Some("S2"), None).await.expect("close"), "a double click closes once");
    let row = mm::find_by_thread(t.db(), "811000000000000001").await.expect("q").expect("row");
    assert_eq!((row.status, row.closed_by_id.as_deref()), (ModmailStatus::Closed, Some("S1")));
    assert_eq!(row.close_reason.as_deref().map(|r| r.chars().count()), Some(500), "the reason is cut to REASON_MAX");
    assert!(row.closed_at.is_some());

    // closeByThread and releaseReserved.
    let third = match mm::reserve_open(&t, G, "U1").await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &third.id, "811000000000000002").await.expect("attach");
    assert_eq!(mm::close_by_thread(&t, "811000000000000002", mm::THREAD_DELETED).await.expect("close"), 1);
    assert_eq!(mm::close_by_thread(&t, "811000000000000002", mm::THREAD_DELETED).await.expect("close"), 0);
    assert_eq!(mm::find_by_thread(t.db(), "811000000000000002").await.expect("q").and_then(|r| r.close_reason), Some("The thread was deleted.".into()));
    let fourth = match mm::reserve_open(&t, G, "U1").await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::release_reserved(t.db(), &fourth.id).await;
    assert!(mm::find_in_guild(t.db(), G, &fourth.id).await.expect("q").is_none());
    t.reset_guilds(&[G]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_reserves_create_one_row() {
    const G: &str = "940000000000000002";
    let Some(t) = app(G).await else { return };
    t.settings().ensure_guild_settings(G).await.expect("settings row");
    let futs: Vec<_> = (0..6)
        .map(|_| {
            let a = t.app.clone();
            tokio::spawn(async move { mm::reserve_open(&a, G, "U1").await })
        })
        .collect();
    let mut new_rows = 0;
    for f in futs {
        if let Ok(Ok(mm::Reserved::Row(_))) = f.await {
            new_rows += 1;
        }
    }
    assert_eq!(new_rows, 1, "the advisory lock lets one through");
    let n: (i64,) = bot_db::sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "ModmailThread" WHERE "guildId" = $1"#).bind(G).fetch_one(t.db()).await.expect("count");
    assert_eq!(n.0, 1);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn js_and_rust_share_modmail_rows() {
    const G: &str = "940000000000000003";
    let Some(t) = app(G).await else { return };
    // JS reserves and attaches; Rust finds and closes.
    let reserved = js(&["mm-reserve", G, "600000000000000005"]);
    let id = reserved["row"]["id"].as_str().expect("a JS row").to_string();
    js(&["mm-attach", &id, "812000000000000001"]);
    let row = mm::find_by_thread(t.db(), "812000000000000001").await.expect("q").expect("the JS row");
    assert_eq!((row.id.as_str(), row.user_id.as_str(), row.status), (id.as_str(), "600000000000000005", ModmailStatus::Open));
    match mm::reserve_open(&t, G, "600000000000000005").await.expect("reserve") {
        mm::Reserved::Existing(r) => assert_eq!(r.id, id, "the JS row is the open one"),
        other => panic!("{other:?}"),
    }
    assert!(mm::close_row(&t, &id, Some("600000000000000001"), Some("done")).await.expect("close"));
    let back = js(&["mm-by-thread", "812000000000000001"]);
    assert_eq!((back["status"].as_str(), back["closedById"].as_str(), back["closeReason"].as_str()), (Some("CLOSED"), Some("600000000000000001"), Some("done")));
    let ours = mm::find_by_thread(t.db(), "812000000000000001").await.expect("q").expect("row");
    assert_eq!(to_json(&ours), back, "the row reads the same through Prisma and Rust");
    assert_eq!(js(&["mm-close", &id, "", ""]), json!(false), "closed once, whichever side closed it");

    // Rust reserves and attaches; the JS finds it.
    let mine = match mm::reserve_open(&t, G, "600000000000000006").await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    match js(&["mm-reserve", G, "600000000000000006"]).get("existing") {
        Some(e) => assert_eq!(e["id"], json!(mine.id)),
        None => panic!("the JS must see the Rust placeholder"),
    }
    let attached: ModmailThread = mm::attach_thread(&t, &mine.id, "812000000000000002").await.expect("attach");
    let open = js(&["mm-open-user", "600000000000000006"]);
    assert_eq!(open, json!([to_json(&attached)]));
    assert_eq!(js(&["mm-open-guild", G, "600000000000000006"]), to_json(&attached));
    t.reset_guilds(&[G]).await;
}

// ── Boosts: the table, the job, the sweep ─────────────────────────────────────

#[tokio::test]
async fn booster_rows_and_raw_sql() {
    const G: &str = "940000000000000004";
    let Some(t) = app(G).await else { return };
    t.settings().ensure_guild_settings(G).await.expect("settings row");
    for (sql_name, sql) in bs::RAW_SQL {
        let mut tx = t.db().begin().await.expect("tx");
        let res = if sql_name == "countRows" {
            bot_db::sqlx::query(sql).bind(G).execute(&mut *tx).await
        } else {
            bot_db::sqlx::query(sql).bind(G).bind("U").bind(bot_db::now_utc().naive_utc()).execute(&mut *tx).await
        };
        assert!(res.is_ok(), "{sql_name}: {res:?}");
        tx.rollback().await.expect("rollback");
    }
    let row: BoosterRole = bs::insert_row(t.db(), G, "U1", "830400000000000001").await.expect("insert");
    bs::insert_row(t.db(), G, "U2", "830400000000000002").await.expect("insert");
    assert!(bs::insert_row(t.db(), G, "U1", "830400000000000003").await.err().is_some_and(|e| e.is_unique_violation()), "one per member");
    assert_eq!(bs::count_rows(t.db(), G).await.expect("count"), 2);
    let rows = bs::list_rows(t.db(), G).await.expect("list");
    assert_eq!(rows.iter().map(|r| r.user_id.as_str()).collect::<Vec<_>>(), vec!["U1", "U2"], "oldest first");
    assert_eq!(bs::load_row(t.db(), G, "U1").await.expect("load"), Some(row.clone()));

    let when = chrono::DateTime::from_timestamp_millis(1_790_000_000_123).expect("time");
    assert_eq!(bs::mark_ended(t.db(), G, "U1", when).await.expect("end"), 1);
    assert_eq!(bs::mark_ended(t.db(), G, "U1", chrono::Utc::now()).await.expect("end"), 0, "only when not set yet");
    assert_eq!(bs::load_row(t.db(), G, "U1").await.expect("load").and_then(|r| r.boost_ended_at), Some(when));
    assert_eq!(bs::clear_ended(t.db(), G, "U1").await.expect("clear"), 1);
    assert_eq!(bs::clear_ended(t.db(), G, "U1").await.expect("clear"), 0);
    assert_eq!(bs::delete_rows_by_role(t.db(), G, "830400000000000002").await.expect("delete"), 1);
    assert_eq!(bs::delete_row(t.db(), G, "U1").await.expect("delete"), 1);
    assert_eq!(bs::delete_row(t.db(), G, "U1").await.expect("delete"), 0, "a missing row is fine");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn js_and_rust_share_booster_rows() {
    const G: &str = "940000000000000005";
    let Some(t) = app(G).await else { return };
    let created = js(&["bs-insert", G, "600000000000000007", "830500000000000007"]);
    js(&["bs-end", G, "600000000000000007", "2026-03-04T05:06:07.089Z"]);
    let row = bs::load_row(t.db(), G, "600000000000000007").await.expect("load").expect("the JS row");
    assert_eq!(row.role_id, "830500000000000007");
    assert_eq!(to_json(&row)["boostEndedAt"], "2026-03-04T05:06:07.089Z");
    assert_eq!(to_json(&row)["createdAt"], created["createdAt"]);
    // Rust clears and writes; the JS reads.
    bs::clear_ended(t.db(), G, "600000000000000007").await.expect("clear");
    bs::insert_row(t.db(), G, "600000000000000008", "830500000000000008").await.expect("insert");
    let when = chrono::DateTime::from_timestamp_millis(1_800_000_000_456).expect("time");
    bs::mark_ended(t.db(), G, "600000000000000008", when).await.expect("end");
    let back = js(&["bs-rows", G, "600000000000000008"]);
    let ours: Vec<Value> = bs::list_rows(t.db(), G).await.expect("list").iter().map(to_json).collect();
    assert_eq!(back["rows"], Value::Array(ours), "every row reads the same");
    assert_eq!(back["count"], 2);
    assert_eq!(back["one"]["boostEndedAt"], "2027-01-15T08:00:00.456Z");
    assert_eq!(back["rows"][0]["boostEndedAt"], Value::Null);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn the_sweep_job_is_kept_live_from_both_sides() {
    const G: &str = "940000000000000006";
    let Some(t) = app(G).await else { return };
    t.install(registry());
    // guildReady seeds the sweep only for a guild with booster roles.
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", true)).await.expect("settings");
    t.run_hooks(GuildReady { guild_id: gid(G), reason: GuildReadyReason::Startup }).await;
    assert!(bs::sweep_job(&t, G).await.is_none(), "no rows, no sweep");
    bs::insert_row(t.db(), G, "U1", "830600000000000001").await.expect("insert");
    t.run_hooks(GuildReady { guild_id: gid(G), reason: GuildReadyReason::Startup }).await;
    let job = bs::sweep_job(&t, G).await.expect("seeded");
    let delay = job.run_at.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    assert!((50_000..=185_000).contains(&delay), "1 to 3 minutes after startup: {delay}");
    let js_job = js(&["bs-job", G]);
    assert_eq!((js_job["type"].as_str(), js_job["dedupeKey"].as_str(), &js_job["payload"]), (Some("boosts.sweep"), Some(format!("boosts.sweep:{G}").as_str()), &json!({})));
    assert_eq!(js_job["id"], json!(job.id));
    assert_eq!((js_job["maxAttempts"].as_i64(), js_job["status"].as_str(), js_job["attempts"].as_i64()), (Some(5), Some("PENDING"), Some(0)), "the row the JS writes");
    // A live job is kept by either side.
    assert!(bs::ensure_sweep_job(&t, G, None).await);
    let again = js(&["bs-ensure", G]);
    assert_eq!(again["job"]["id"], json!(job.id));
    assert_eq!(again["job"]["runAt"], to_json(&job)["runAt"], "kept, not moved");
    t.reset_guilds(&[G]).await;

    // The JS creates it; Rust reads it.
    let Some(t) = app(G).await else { return };
    let made = js(&["bs-ensure", G]);
    let read = bs::sweep_job(&t, G).await.expect("the JS job");
    assert_eq!(to_json(&read), made["job"], "the JS row reads the same in Rust");
    t.reset_guilds(&[G]).await;
    // And the Rust row is the row the JS would have written.
    let Some(t) = app(G).await else { return };
    bs::ensure_sweep_job(&t, G, None).await;
    let ours = bs::sweep_job(&t, G).await.expect("the Rust job");
    assert_eq!(js(&["bs-job", G]), to_json(&ours));
    assert_eq!((ours.max_attempts, ours.attempts, ours.payload.clone()), (5, 0, json!({})));
    t.reset_guilds(&[G]).await;
}

fn lookup_of(states: Vec<(&'static str, bs::MemberState)>, calls: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> bs::MemberLookup {
    std::sync::Arc::new(move |user: String| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let s = states.iter().find(|(u, _)| *u == user).map(|(_, s)| *s).unwrap_or(bs::MemberState::Unknown);
        Box::pin(async move { s })
    })
}

#[tokio::test]
async fn the_sweep_repairs_rows_and_never_acts_on_a_failed_lookup() {
    use bs::MemberState::*;
    const G: &str = "940000000000000007";
    let Some(t) = app(G).await else { return };
    t.settings().update_settings(G, SettingsPatch::new().set("boostRoleGraceDays", 2)).await.expect("settings");
    let day = bs::consts::DAY_MS;
    for (u, r) in [("U1", "1"), ("U2", "2"), ("U3", "3"), ("U4", "4"), ("U5", "5")] {
        bs::insert_row(t.db(), G, u, &format!("83070000000000000{r}")).await.expect("insert");
    }
    let now = chrono::Utc::now();
    bs::mark_ended(t.db(), G, "U3", now - chrono::Duration::milliseconds(day)).await.expect("end");
    bs::mark_ended(t.db(), G, "U5", now - chrono::Duration::milliseconds(5 * day)).await.expect("end");
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let lookup = lookup_of(
        vec![("U1", Left), ("U2", Present { boosting: false }), ("U3", Present { boosting: true }), ("U4", Unknown), ("U5", Present { boosting: false })],
        calls.clone(),
    );
    let rows = bs::list_rows(t.db(), G).await.expect("list");
    let settings = t.settings().get_settings(G).await.expect("settings");
    let out = bs::sweep_guild(&t, gid(G), &rows, Some(&settings), bs::SweepOptions { now_ms: now.timestamp_millis(), budget_ms: 35_000, lookup: Some(lookup) }).await;
    // Offline the role deletes fail (no Discord connection): the rows stay, so the next run tries again.
    assert_eq!(out, bs::SweepOut { deleted: 0, ended: 1, resumed: 1, failed: 2, more: false });
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5, "every uncached member was looked up");
    let by = |u: &str| rows.iter().find(|r| r.user_id == u).cloned();
    assert!(by("U1").is_some() && bs::load_row(t.db(), G, "U1").await.expect("load").is_some(), "a failed delete keeps the row");
    assert!(bs::load_row(t.db(), G, "U2").await.expect("load").and_then(|r| r.boost_ended_at).is_some(), "the grace period starts now");
    assert!(bs::load_row(t.db(), G, "U3").await.expect("load").and_then(|r| r.boost_ended_at).is_none(), "boosts again");
    assert!(bs::load_row(t.db(), G, "U4").await.expect("load").and_then(|r| r.boost_ended_at).is_none(), "a failed lookup is never acted on");

    // The job: rescheduled in an hour, or in a minute when there was more to do.
    let outcome = bs::sweep_job_run(&t, G, Some(lookup_of(vec![], calls.clone()))).await;
    let JobOutcome::Reschedule { next_run_at, payload: None } = outcome else { panic!("{outcome:?}") };
    let ms = next_run_at.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    assert!((3_500_000..=3_600_000).contains(&ms), "{ms}");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn the_sweep_is_bounded_per_run_and_ends_itself_without_rows() {
    const G: &str = "940000000000000008";
    let Some(t) = app(G).await else { return };
    bs::reset_sweep_state(&t);
    t.settings().ensure_guild_settings(G).await.expect("settings");
    for i in 0..60 {
        bs::insert_row(t.db(), G, &format!("U{i:02}"), &format!("8308000000000001{i:02}")).await.expect("insert");
    }
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outcome = bs::sweep_job_run(&t, G, Some(lookup_of(vec![], calls.clone()))).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), bs::consts::LOOKUPS_PER_RUN, "at most 50 lookups a run");
    let JobOutcome::Reschedule { next_run_at, .. } = outcome else { panic!("{outcome:?}") };
    let ms = next_run_at.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    assert!((50_000..=60_000).contains(&ms), "comes back in a minute: {ms}");
    // The next run starts where this one stopped.
    calls.store(0, std::sync::atomic::Ordering::SeqCst);
    bs::sweep_job_run(&t, G, Some(lookup_of(vec![], calls.clone()))).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), bs::consts::LOOKUPS_PER_RUN);

    // No rows: the job ends itself, also through the scheduler.
    bot_db::sqlx::query(r#"DELETE FROM "BoosterRole" WHERE "guildId" = $1"#).bind(G).execute(t.db()).await.expect("delete");
    assert_eq!(bs::sweep_job_run(&t, G, None).await, JobOutcome::Done);
    t.install(registry());
    bs::ensure_sweep_job(&t, G, Some(chrono::Utc::now() - chrono::Duration::seconds(1))).await;
    assert!(bs::sweep_job(&t, G).await.is_some());
    t.scheduler().tick(&t, false).await;
    for _ in 0..50 {
        if bs::sweep_job(&t, G).await.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(bs::sweep_job(&t, G).await.is_none(), "DONE jobs are deleted");
    // An unavailable guild waits five minutes.
    let other = "940000000000000098";
    let JobOutcome::Reschedule { next_run_at, .. } = bs::sweep_job_run(&t, other, None).await else { panic!("reschedule") };
    let ms = next_run_at.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    assert!((290_000..=300_000).contains(&ms), "{ms}");
    t.reset_guilds(&[G]).await;
}

fn member_json(g: &str, id: &str, boosting: bool) -> serenity::all::Member {
    let since = if boosting { json!("2026-01-01T00:00:00.000000+00:00") } else { Value::Null };
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0,
        "premium_since": since, "guild_id": g,
    }))
    .unwrap_or_else(|e| panic!("member: {e}"))
}

fn channel_json(g: &str, id: &str, kind: u8) -> serenity::all::GuildChannel {
    serde_json::from_value(json!({ "id": id, "type": kind, "guild_id": g, "name": "gone", "position": 0, "permission_overwrites": [] }))
        .unwrap_or_else(|e| panic!("channel: {e}"))
}

#[tokio::test]
async fn boost_hooks_end_start_leave_and_cleanup() {
    const G: &str = "940000000000000009";
    let Some(t) = app(G).await else { return };
    t.install(registry());
    let patch = SettingsPatch::new().set("boostsEnabled", true).set("boostRoleGraceDays", 2).set("boostChannelId", ANN_CH).set("boostRoleAnchorId", "830900000000000002");
    t.settings().update_settings(G, patch).await.expect("settings");
    bs::insert_row(t.db(), G, MEMBER, "830900000000000001").await.expect("insert");
    let update = |before: bool, after: bool, known: bool| GuildMemberUpdate {
        guild_id: gid(G),
        old_member: Some(member_json(G, MEMBER, before)),
        member: member_json(G, MEMBER, after),
        old_known: known,
    };
    // A partial old member is never diffed.
    t.run_hooks(update(true, false, false)).await;
    assert!(bs::load_row(t.db(), G, MEMBER).await.expect("load").and_then(|r| r.boost_ended_at).is_none());
    // The boost ends with a grace period: the end is stamped and the sweep seeded.
    t.run_hooks(update(true, false, true)).await;
    assert!(bs::load_row(t.db(), G, MEMBER).await.expect("load").and_then(|r| r.boost_ended_at).is_some());
    assert!(bs::sweep_job(&t, G).await.is_some(), "the sweep removes the role later");
    // A returning booster keeps the role and the end is cleared.
    t.run_hooks(update(false, true, true)).await;
    assert!(bs::load_row(t.db(), G, MEMBER).await.expect("load").and_then(|r| r.boost_ended_at).is_none());
    // With no grace the role goes at once (offline the delete fails, so the row stays for the sweep).
    t.settings().update_settings(G, SettingsPatch::new().set("boostRoleGraceDays", 0)).await.expect("settings");
    t.run_hooks(update(true, false, true)).await;
    assert!(bs::load_row(t.db(), G, MEMBER).await.expect("load").and_then(|r| r.boost_ended_at).is_some());

    // roleDelete drops the row of that role and forgets a deleted anchor; it runs while the module is off.
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", false)).await.expect("settings");
    t.run_hooks(RoleDelete { guild_id: gid(G), role_id: serenity::all::RoleId::new(830900000000000001), role: None }).await;
    assert!(bs::load_row(t.db(), G, MEMBER).await.expect("load").is_none());
    t.run_hooks(RoleDelete { guild_id: gid(G), role_id: serenity::all::RoleId::new(830900000000000002), role: None }).await;
    assert_eq!(t.settings().get_settings_with(G, true).await.expect("s").boost_role_anchor_id, None);
    // channelDelete forgets the thank-you channel.
    t.run_hooks(ChannelDelete { guild_id: gid(G), channel: channel_json(G, ANN_CH, 5) }).await;
    assert_eq!(t.settings().get_settings_with(G, true).await.expect("s").boost_channel_id, None);
    t.reset_guilds(&[G]).await;
}

// ── Auto Threads: the lists ───────────────────────────────────────────────────

#[tokio::test]
async fn auto_thread_lists_change_and_read_the_same_in_js() {
    const G: &str = "940000000000000010";
    let Some(t) = app(G).await else { return };
    let change = |name: Option<&str>, archive: Option<&str>, close: Option<bool>| at::ThreadChange {
        channel_id: STAFF_CH.into(),
        name: name.map(String::from),
        archive_minutes: archive.map(String::from),
        close_button: close,
    };
    let added = at::set_thread_channel(&t, G, change(None, None, None)).await.expect("set");
    assert!(added.created);
    assert_eq!(to_json(&added.entry), json!({ "channelId": STAFF_CH, "name": "{content}", "archiveMinutes": "1440", "closeButton": false }));
    let updated = at::set_thread_channel(&t, G, change(Some("{user}"), None, Some(true))).await.expect("set");
    assert!(!updated.created);
    assert_eq!(to_json(&updated.entry), json!({ "channelId": STAFF_CH, "name": "{user}", "archiveMinutes": "1440", "closeButton": true }), "only the options given change");
    assert!(at::add_publish_channel(&t, G, ANN_CH).await.expect("add").added);
    assert!(!at::add_publish_channel(&t, G, ANN_CH).await.expect("add").added, "already there");
    // The JS reads what Rust wrote.
    let back = js(&["at-read", G]);
    assert_eq!(back["entries"], json!([to_json(&updated.entry)]));
    assert_eq!(back["publish"], json!([ANN_CH]));

    // The JS writes; Rust reads (other entries kept exactly as stored).
    js(&["at-set", G, &json!({ "channelId": "810000000000000009", "archiveMinutes": "60" }).to_string()]);
    js(&["at-publish", G, "810000000000000008"]);
    // The JS wrote the row (and its own cache copy): drop ours, as the JS does on a foreign write.
    t.settings().invalidate_settings(G).await;
    let s = t.settings().get_settings(G).await.expect("settings");
    let index = at::channel_index(Some(&s));
    assert_eq!(index.threads.get("810000000000000009").map(|e| e.archive_minutes.as_str()), Some("60"));
    assert_eq!(index.threads.get(STAFF_CH).map(|e| e.name.as_str()), Some("{user}"));
    assert!(index.publish.contains("810000000000000008") && index.publish.contains(ANN_CH));

    // Removing, dropping a deleted channel, and the caps.
    assert!(at::remove_thread_channel(&t, G, "810000000000000009").await.expect("remove"));
    assert!(!at::remove_thread_channel(&t, G, "810000000000000009").await.expect("remove"));
    assert!(at::remove_publish_channel(&t, G, "810000000000000008").await.expect("remove"));
    assert!(!at::remove_publish_channel(&t, G, "810000000000000008").await.expect("remove"));
    assert!(!at::drop_channel(&t, G, "810000000000000077").await.expect("drop"), "in neither list: nothing written");
    t.install(registry());
    t.run_hooks(ChannelDelete { guild_id: gid(G), channel: channel_json(G, STAFF_CH, 0) }).await;
    assert_eq!(js(&["at-read", G])["entries"], json!([]), "a deleted channel leaves the list (the module is off)");
    assert!(at::drop_channel(&t, G, ANN_CH).await.expect("drop"));
    assert_eq!(js(&["at-drop", G, ANN_CH]), json!(false));
    for i in 0..bot_config::limits::AUTOTHREAD_CHANNELS {
        at::set_thread_channel(&t, G, at::ThreadChange { channel_id: format!("8150000000000000{i:02}"), ..Default::default() }).await.expect("set");
    }
    let full = at::set_thread_channel(&t, G, change(None, None, None)).await.expect_err("full");
    let e = full.downcast_ref::<at::AutoThreadError>().expect("an AutoThreadError");
    assert_eq!((e.title.as_str(), e.message.as_str()), ("Limit Reached", "A server can have at most **25** auto-thread channels. Remove one with `/autothread remove` first."));
    t.reset_guilds(&[G]).await;
}

// ── Modmail hooks ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn modmail_cleanup_hooks_and_the_not_modmail_memo() {
    const G: &str = "940000000000000011";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modmailEnabled", true).set("modmailChannelId", STAFF_CH)).await.expect("settings");
    let row = match mm::reserve_open(&t, G, MEMBER).await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &row.id, "813000000000000001").await.expect("attach");
    // threadDelete closes the conversation of the deleted thread.
    t.run_hooks(ThreadDelete { guild_id: gid(G), thread_id: serenity::all::ChannelId::new(813000000000000001), parent_id: None, thread: None }).await;
    let closed = mm::find_by_thread(t.db(), "813000000000000001").await.expect("q").expect("row");
    assert_eq!((closed.status, closed.close_reason.as_deref()), (ModmailStatus::Closed, Some("The thread was deleted.")));
    // channelDelete of the staff channel clears the setting (threads Discord cannot confirm gone stay open).
    let open = match mm::reserve_open(&t, G, MEMBER).await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &open.id, "813000000000000002").await.expect("attach");
    t.run_hooks(ChannelDelete { guild_id: gid(G), channel: channel_json(G, STAFF_CH, 0) }).await;
    assert_eq!(t.settings().get_settings_with(G, true).await.expect("s").modmail_channel_id, None);
    assert_eq!(mm::find_by_thread(t.db(), "813000000000000002").await.expect("q").map(|r| r.status), Some(ModmailStatus::Open));

    // A thread that is not a modmail thread is remembered and skipped.
    let mut thread = bot_core::interaction_helpers::cached_channel(&t, gid(G), serenity::all::ChannelId::new(THREAD.parse().unwrap_or(1))).expect("thread");
    thread.owner_id = Some(serenity::all::UserId::new(TEST_BOT_ID.parse().unwrap_or(1)));
    let message = bot_core::testing::fake_message("700000000000000001", G, THREAD, MEMBER, "hello");
    assert!(!mmp::known_not_modmail(&t, THREAD));
    t.run_hooks(MessageCreate { guild_id: gid(G), message, member: None, channel_id: thread.id, channel: Some(thread) }).await;
    assert!(mmp::known_not_modmail(&t, THREAD), "the next message in it costs no query");

    // The member's usable conversations: the guild is available, modmail on, not blocked.
    let convs = mmp::list_open_conversations(&t, MEMBER).await.expect("list");
    assert_eq!(convs.iter().map(|c| c.row.id.clone()).collect::<Vec<_>>(), vec![open.id.clone()]);
    t.settings().update_settings(G, SettingsPatch::new().set("modmailBlockedUserIds", json!([MEMBER]))).await.expect("settings");
    assert!(mmp::list_open_conversations(&t, MEMBER).await.expect("list").is_empty(), "blocked");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn the_close_button_moderators_only_closes_once() {
    const G: &str = "940000000000000012";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modmailEnabled", true).set("modmailChannelId", STAFF_CH)).await.expect("settings");
    let row = match mm::reserve_open(&t, G, MEMBER).await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &row.id, &thread_of(G)).await.expect("attach");
    let click = || FakeInteraction::button(&format!("modmail:close:{}", row.id)).guild(G).user(ADMIN).roles(&["820000000000000010"]).channel(&thread_of(G));
    let calls = route(&t, click()).await;
    let names: Vec<&str> = calls.iter().map(|c| c.method).collect();
    assert_eq!(names, vec!["deferUpdate", "editReply", "followUp"], "{calls:?}");
    assert_eq!(calls[1].body["components"], json!([]), "the button goes");
    assert_eq!(last(&calls), ("✅  Conversation Closed".into(), format!("Closed by <@{ADMIN}>.\nThe member could not be notified (DMs closed).")));
    let r = mm::find_by_thread(t.db(), &thread_of(G)).await.expect("q").expect("row");
    assert_eq!((r.status, r.closed_by_id.as_deref()), (ModmailStatus::Closed, Some(ADMIN)));
    let calls = route(&t, click()).await;
    assert_eq!(last(&calls), ("❌  Already Closed".into(), "This conversation is already closed.".into()));
    assert!(calls.last().is_some_and(|c| c.ephemeral()));
    t.reset_guilds(&[G]).await;
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn autothread_command_add_list_remove_and_publish() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, STRING};
    const G: &str = "940000000000000013";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    let add = |ch: &str| admin("autothread", G).subcommand("add").option("channel", CHANNEL, json!(ch));
    let calls = route(&t, add(STAFF_CH).option("name", STRING, json!("{user} asks")).option("archive", STRING, json!("60")).option("close_button", BOOLEAN, json!(true))).await;
    assert_eq!(calls[0].method, "deferReply");
    assert!(!calls[0].ephemeral(), "admin config commands answer publicly");
    let off = feat_community_b::commands::autothread::OFF_NOTE;
    assert_eq!(
        last(&calls),
        (
            "✅  Auto Thread Added".into(),
            format!("Every member message in <#{STAFF_CH}> gets its own thread.\nName: `{{user}} asks` · archives after 1 hour · close button on\n\n{off}")
        )
    );
    assert_eq!(js(&["at-read", G])["entries"], json!([{ "channelId": STAFF_CH, "name": "{user} asks", "archiveMinutes": "60", "closeButton": true }]));
    let calls = route(&t, add(STAFF_CH).option("archive", STRING, json!("10080"))).await;
    assert_eq!(last(&calls).0, "✅  Auto Thread Updated");
    assert!(last(&calls).1.contains("Name: `{user} asks` · archives after 1 week · close button on"), "{:?}", last(&calls));
    let calls = route(&t, add(STAFF_CH).option("name", STRING, json!("{nope} {content}"))).await;
    assert_eq!(last(&calls), ("❌  Unknown Placeholder".into(), "`{nope}` is not a placeholder. Use `{user}`, `{content}`, `{channel}` or `{date}`.".into()));
    let calls = route(&t, add(THREAD)).await;
    assert_eq!(last(&calls), ("❌  Wrong Channel Type".into(), "Pick a text or announcement channel.".into()));

    let publish = |sub: &str, ch: &str| admin("autothread", G).group("publish", sub).option("channel", CHANNEL, json!(ch));
    let calls = route(&t, publish("add", ANN_CH)).await;
    assert_eq!(last(&calls).0, "✅  Auto Publish Added");
    assert!(last(&calls).1.starts_with(&format!("New posts in <#{ANN_CH}> (members, bots and webhooks) are published to the servers that follow it. Discord allows 10 per channel per hour; the rest stay unpublished.")));
    assert_eq!(last(&route(&t, publish("add", ANN_CH)).await).0, "ℹ️  Already Publishing");
    assert_eq!(last(&route(&t, publish("add", STAFF_CH)).await), ("❌  Wrong Channel Type".into(), "Pick an announcement channel.".into()));

    let calls = route(&t, admin("autothread", G).subcommand("list")).await;
    let (title, text) = last(&calls);
    assert_eq!(title, "🧵 Auto Threads");
    assert_eq!(text, format!("{off}\n\n**🧵 Threads (1/25)**\n<#{STAFF_CH}> · `{{user}} asks` · 1 week · 🔒 close button\n\n**📣 Publishing (1/25)**\n<#{ANN_CH}>"));

    assert_eq!(last(&route(&t, admin("autothread", G).subcommand("remove").option("channel", CHANNEL, json!(STAFF_CH))).await).0, "✅  Auto Thread Removed");
    assert_eq!(
        last(&route(&t, admin("autothread", G).subcommand("remove").option("channel", CHANNEL, json!(STAFF_CH))).await),
        ("❌  Not In The List".into(), format!("<#{STAFF_CH}> is not in that list."))
    );
    assert_eq!(last(&route(&t, publish("remove", ANN_CH)).await).0, "✅  Auto Publish Removed");
    let calls = route(&t, admin("autothread", G).subcommand("list")).await;
    assert!(last(&calls).1.ends_with("Nothing is set up yet. Add a channel with `/autothread add`, or `/autothread publish add` for an announcement channel."));
    // A member may not use it.
    let calls = route(&t, FakeInteraction::slash("autothread").guild(G).user(MEMBER).subcommand("list")).await;
    assert!(last(&calls).0.contains("Permission"), "{:?}", last(&calls));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn modmail_command_setup_block_unblock_blocked() {
    use bot_core::command::option_type::{CHANNEL, ROLE, STRING, USER};
    const G: &str = "940000000000000014";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all() & !P::ADMINISTRATOR);
    t.install(registry());
    let setup = |ch: &str| admin("modmail", G).subcommand("setup").option("channel", CHANNEL, json!(ch));
    let calls = route(&t, setup(LOCKED_CH)).await;
    assert_eq!(
        last(&calls),
        ("❌  Cannot Use That Channel".into(), format!("I need **Create Private Threads** in <#{LOCKED_CH}> to open conversations.\nPick another channel, or fix my permissions there."))
    );
    assert_eq!(last(&route(&t, setup(ANN_CH)).await).1, "Pick a text channel: conversations open as threads in it.");
    let calls = route(&t, setup(STAFF_CH).option("ping_role", ROLE, json!("820000000000000002"))).await;
    assert_eq!(
        last(&calls),
        ("❌  Cannot Ping That Role".into(), "<@&820000000000000002> is not mentionable and you cannot mention every role. Make it mentionable, or pick another role.".into())
    );
    assert_eq!(last(&route(&t, setup(STAFF_CH).option("ping_role", ROLE, json!(G))).await).1, "@everyone cannot be the ping role.");
    let calls = route(&t, setup(STAFF_CH)).await;
    assert_eq!(
        last(&calls),
        (
            "✅  Modmail Set Up".into(),
            format!(
                "Modmail is on. Members can DM me; each conversation opens as a thread in <#{STAFF_CH}>.\nNo role is pinged yet.\nKeep the channel staff-only; members never need access to it.\n⚠️ No role is pinged, so only members who can manage threads will see new private threads. Set one with `ping_role`."
            )
        )
    );
    let calls = route(&t, setup(STAFF_CH).option("ping_role", ROLE, json!("820000000000000001"))).await;
    assert!(last(&calls).1.contains("Pinged on a new conversation: <@&820000000000000001>"), "{:?}", last(&calls));
    let s = t.settings().get_settings_with(G, true).await.expect("settings");
    assert_eq!((s.modmail_channel_id.as_deref(), s.modmail_enabled, s.modmail_ping_role_ids.clone()), (Some(STAFF_CH), true, vec!["820000000000000001".to_string()]));

    // reply and close only work in an open modmail thread.
    let calls = route(&t, admin("modmail", G).subcommand("close")).await;
    assert_eq!(last(&calls), ("❌  Not a Modmail Thread".into(), "Use this command inside a modmail conversation thread.".into()));
    let calls = route(&t, admin("modmail", G).subcommand("reply").option("text", STRING, json!("hi")).channel(THREAD)).await;
    assert_eq!(last(&calls).0, "❌  Not a Modmail Thread", "a thread with no row");

    let block = |user: &str, bot: bool| with_user(admin("modmail", G).subcommand("block").option("user", USER, json!(user)), user, bot);
    let calls = route(&t, admin("modmail", G).subcommand("blocked")).await;
    assert!(calls[0].ephemeral(), "the list is private");
    assert_eq!(last(&calls), ("🚫 Blocked Members".into(), "Nobody is blocked.".into()));
    assert_eq!(last(&route(&t, block(MEMBER, false)).await), ("✅  Member Blocked".into(), format!("<@{MEMBER}> can no longer open modmail conversations.")));
    assert_eq!(last(&route(&t, block(MEMBER, false)).await), ("❌  Already Blocked".into(), format!("<@{MEMBER}> is already blocked.")));
    assert_eq!(last(&route(&t, block(ADMIN, false)).await).1, "You cannot block yourself.");
    assert_eq!(last(&route(&t, block(ADMIN2, false)).await).1, "Staff cannot be blocked.", "a cached member at the Moderator level");
    assert_eq!(last(&route(&t, block("600000000000000050", true)).await).1, "Bots cannot open conversations anyway.");
    let calls = route(&t, admin("modmail", G).subcommand("blocked")).await;
    assert_eq!(last(&calls).1, format!("<@{MEMBER}> `{MEMBER}`"));
    assert_eq!(calls.last().and_then(|c| c.body["embeds"][0]["footer"]["text"].as_str().map(String::from)), Some("1 of 500".into()));
    let unblock = |user: &str| admin("modmail", G).subcommand("unblock").option("user", USER, json!(user));
    assert_eq!(last(&route(&t, unblock(MEMBER)).await), ("✅  Member Unblocked".into(), format!("<@{MEMBER}> can open modmail conversations again.")));
    assert_eq!(last(&route(&t, unblock(MEMBER)).await), ("❌  Not Blocked".into(), format!("<@{MEMBER}> is not blocked.")));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn modmail_block_closes_an_open_conversation_and_reply_refuses_while_off() {
    use bot_core::command::option_type::{STRING, USER};
    const G: &str = "940000000000000015";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modmailEnabled", true).set("modmailChannelId", STAFF_CH)).await.expect("settings");
    let row = match mm::reserve_open(&t, G, MEMBER).await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &row.id, &thread_of(G)).await.expect("attach");
    // reply in the thread: offline the member cannot be reached.
    let calls = route(&t, admin("modmail", G).subcommand("reply").option("text", STRING, json!("hello")).channel(&thread_of(G))).await;
    assert_eq!(last(&calls), ("❌  Not Delivered".into(), "I couldn't deliver that reply. Please try again in a moment.".into()));
    let calls = route(&t, with_user(admin("modmail", G).subcommand("block").option("user", USER, json!(MEMBER)).option("reason", STRING, json!("spam")), MEMBER, false)).await;
    assert_eq!(last(&calls).1, format!("<@{MEMBER}> can no longer open modmail conversations. Their open conversation was closed."));
    let r = mm::find_by_thread(t.db(), &thread_of(G)).await.expect("q").expect("row");
    assert_eq!((r.status, r.close_reason.as_deref(), r.closed_by_id.as_deref()), (ModmailStatus::Closed, Some("Blocked: spam"), Some(ADMIN)));
    // close in a closed thread.
    assert_eq!(last(&route(&t, admin("modmail", G).subcommand("close").channel(&thread_of(G))).await), ("❌  Conversation Closed".into(), "This conversation is already closed.".into()));
    t.settings().update_settings(G, SettingsPatch::new().set("modmailEnabled", false)).await.expect("settings");
    let calls = route(&t, admin("modmail", G).subcommand("reply").option("text", STRING, json!("hello")).channel(&thread_of(G))).await;
    assert_eq!(last(&calls).0, "⚠️  Module Disabled");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn boost_command_config_list_remove_test_and_member_refusals() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, INTEGER, ROLE, STRING, USER};
    const G: &str = "940000000000000016";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    let config = || admin("boost", G).subcommand("config");
    let calls = route(&t, config()).await;
    let (title, text) = last(&calls);
    assert_eq!(title, "💎 Server Boosts");
    assert_eq!(
        text,
        "Module: ❌ Off, turn it on with `/module enable boosts`\nThank-you channel: Not set (no thank-yous)\nBooster roles: ❌ Off (0 made)\nBooster roles go: at the bottom of the role list\nKept after the boost ends: removed at once"
    );
    assert_eq!(calls.last().and_then(|c| c.body["embeds"][0]["footer"]["text"].as_str().map(String::from)), Some("Role icons need Boost level 2. The message text is set on the dashboard.".into()));
    let calls = route(
        &t,
        config()
            .option("channel", CHANNEL, json!(ANN_CH))
            .option("custom_roles", BOOLEAN, json!(true))
            .option("anchor", ROLE, json!("820000000000000002"))
            .option("grace_days", INTEGER, json!(3)),
    )
    .await;
    let (title, text) = last(&calls);
    assert_eq!(title, "✅ Server Boosts Updated");
    assert!(text.contains(&format!("Thank-you channel: <#{ANN_CH}>")) && text.contains("Booster roles: ✅ On (0 made)"), "{text}");
    assert!(text.contains("Booster roles go: just below <@&820000000000000002>") && text.ends_with("Kept after the boost ends: 3 days"), "{text}");
    let s = t.settings().get_settings_with(G, true).await.expect("settings");
    assert_eq!((s.boost_channel_id.as_deref(), s.boost_custom_roles, s.boost_role_anchor_id.as_deref(), s.boost_role_grace_days), (Some(ANN_CH), true, Some("820000000000000002"), 3));
    assert_eq!(last(&route(&t, config().option("anchor", ROLE, json!(G))).await), ("❌  Invalid Role".into(), "@everyone cannot be used here. Pick another role.".into()));
    assert_eq!(last(&route(&t, config().option("anchor", ROLE, json!("820000000000000010"))).await).0, "❌  Role Too High");

    // test: a preview (offline the post fails, and the module is off).
    let calls = route(&t, admin("boost", G).subcommand("test")).await;
    let (title, text) = last(&calls);
    assert_eq!(title, "❌  Could Not Post");
    assert!(text.ends_with(feat_community_b::commands::boost::PREVIEW_OFF), "{text}");
    t.settings().update_settings(G, SettingsPatch::new().set("boostChannelId", Value::Null).set("boostsEnabled", true)).await.expect("settings");
    let calls = route(&t, admin("boost", G).subcommand("test")).await;
    assert_eq!(last(&calls), ("ℹ️  No Thank-you Channel".into(), format!("Set one with `/boost config channel:`. This is what members will see:\n\n💎 <@{ADMIN}> just boosted **Test Guild**! Thank you! We now have 1 boosts.")));

    // list and remove.
    assert_eq!(last(&route(&t, admin("boost", G).subcommand("list")).await), ("ℹ️  No Booster Roles".into(), "Nobody has made a booster role yet.".into()));
    bs::insert_row(t.db(), G, MEMBER, "831600000000000001").await.expect("insert");
    bs::mark_ended(t.db(), G, MEMBER, chrono::DateTime::from_timestamp_millis(1_790_000_000_999).expect("t")).await.expect("end");
    let (title, text) = last(&route(&t, admin("boost", G).subcommand("list")).await);
    assert_eq!((title.as_str(), text.as_str()), ("💎 Booster Roles (1)", format!("deleted role `831600000000000001` · <@{MEMBER}> · boost ended <t:1790000000:R>").as_str()));
    assert!(bs::sweep_job(&t, G).await.is_some(), "listing seeds the sweep");
    let remove = |u: &str| admin("boost", G).subcommand("remove").option("user", USER, json!(u));
    assert_eq!(last(&route(&t, remove(ADMIN)).await), ("ℹ️  Nothing To Remove".into(), format!("<@{ADMIN}> has no booster role.")));
    assert_eq!(last(&route(&t, remove(MEMBER)).await).0, "❌  Could Not Save", "offline the role delete fails, the row stays");

    // Members: the module, booster roles and boosting are checked first, privately.
    let role = |sub: &str| FakeInteraction::slash("boost").guild(G).user(MEMBER).group("role", sub).option("name", STRING, json!("Shiny"));
    t.settings().update_settings(G, SettingsPatch::new().set("boostCustomRoles", false)).await.expect("settings");
    let calls = route(&t, role("create")).await;
    assert!(calls[0].ephemeral());
    assert_eq!(last(&calls), ("❌  Booster Roles Are Off".into(), "An admin has not turned on booster roles on this server yet.".into()));
    t.settings().update_settings(G, SettingsPatch::new().set("boostCustomRoles", true)).await.expect("settings");
    assert_eq!(last(&route(&t, role("create")).await), ("❌  Boosters Only".into(), "You need to be boosting this server to do that.".into()));
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", false)).await.expect("settings");
    assert_eq!(last(&route(&t, role("edit")).await).0, "⚠️  Module Disabled");
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", true)).await.expect("settings");
    // delete works after the boost ended; offline the role delete fails.
    let calls = route(&t, FakeInteraction::slash("boost").guild(G).user(MEMBER).group("role", "delete")).await;
    assert_eq!(last(&calls).0, "❌  Could Not Save");
    let calls = route(&t, FakeInteraction::slash("boost").guild(G).user(ADMIN).group("role", "delete")).await;
    assert_eq!(last(&calls), ("❌  No Booster Role".into(), "You do not have a booster role. Make one with `/boost role create`.".into()));
    // Admin subcommands refuse a member.
    assert!(last(&route(&t, FakeInteraction::slash("boost").guild(G).user(MEMBER).subcommand("config")).await).0.contains("Permission"));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn boost_role_create_and_edit_refuse_before_touching_discord() {
    use bot_core::command::option_type::STRING;
    const G: &str = "940000000000000017";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", true).set("boostCustomRoles", true)).await.expect("settings");
    let booster = |fi: FakeInteraction| {
        let mut fi = fi;
        fi.json_mut()["member"]["premium_since"] = json!("2026-01-01T00:00:00.000000+00:00");
        fi
    };
    let create = |name: &str| booster(FakeInteraction::slash("boost").guild(G).user(MEMBER).group("role", "create").option("name", STRING, json!(name)));
    assert_eq!(last(&route(&t, create("pingable")).await), ("❌  Invalid Name".into(), "A role with that name already exists. Pick a different name.".into()));
    assert_eq!(last(&route(&t, create("join x.com")).await).1, "A role name cannot contain a link or an invite.");
    assert_eq!(last(&route(&t, create("ok").option("color", STRING, json!("red"))).await).0, "❌  Invalid Colour");
    assert_eq!(last(&route(&t, create("ok").option("icon_emoji", STRING, json!("💎"))).await).0, "❌  Role Icons Locked");
    assert_eq!(bs::cooldown_left_ms(&t, G, MEMBER), 0, "refusals do not spend the cooldown");
    // An existing row whose role is still there: "already".
    bs::insert_row(t.db(), G, MEMBER, "820000000000000002").await.expect("insert");
    assert_eq!(last(&route(&t, create("Fresh")).await).0, "❌  You Already Have A Role");
    // edit: nothing to change, a bad colour; a row whose role is gone is dropped.
    let edit = || booster(FakeInteraction::slash("boost").guild(G).user(MEMBER).group("role", "edit"));
    assert_eq!(last(&route(&t, edit()).await), ("❌  Nothing To Change".into(), "Give at least one of `name`, `color`, `icon_emoji` or `icon_image`.".into()));
    // Its own name does not collide with itself; offline Discord then refuses the edit.
    assert_eq!(last(&route(&t, edit().option("name", STRING, json!("quiet"))).await).0, "❌  Discord Refused");
    assert!(bs::cooldown_left_ms(&t, G, MEMBER) > 0, "a Discord call spends the cooldown");
    assert_eq!(last(&route(&t, edit().option("name", STRING, json!("again"))).await).0, "❌  Slow Down");
    bs::reset_cooldowns(&t);
    bot_db::sqlx::query(r#"UPDATE "BoosterRole" SET "roleId" = '831700000000000099' WHERE "guildId" = $1"#).bind(G).execute(t.db()).await.expect("update");
    assert_eq!(last(&route(&t, edit().option("name", STRING, json!("x"))).await).0, "❌  No Booster Role");
    assert!(bs::load_row(t.db(), G, MEMBER).await.expect("load").is_none(), "a deleted role drops the row");
    t.reset_guilds(&[G]).await;
}

// ── Audit: behaviour checked against discord.js ───────────────────────────────

fn role_json(id: &str, name: &str, position: u16, tags: Option<Value>) -> Value {
    let mut r = json!({
        "id": id, "name": name, "color": 0, "hoist": false, "position": position, "permissions": "0", "managed": false, "mentionable": false,
        "flags": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    });
    if let Some(t) = tags {
        r["tags"] = t;
    }
    r
}

#[tokio::test]
async fn a_new_booster_role_moves_just_above_the_nitro_booster_role_with_the_whole_list() {
    // boostService.moveToSpot: `role.setPosition(target, { reason })` PATCHes every role of the freshly fetched,
    // discord.js-sorted list with its new index (Util.setPosition), not one entry.
    use bot_core::command::option_type::STRING;
    const G: &str = "940000000000000018";
    const NITRO: &str = "820000000000000005";
    const NEW: &str = "831800000000000001";
    let Some(t) = app(G).await else { return };
    let rest = bot_core::testing::FakeRest::new();
    let guild = fake_guild(G, P::all()).role_with(NITRO, 3, P::empty(), |r| {
        r["name"] = json!("Nitro Booster");
        r["tags"] = json!({ "premium_subscriber": null });
    });
    t.app.set_discord(rest.discord(&[guild.json()]));
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", true).set("boostCustomRoles", true)).await.expect("settings");
    rest.on("POST", "/guilds/*/roles", 200, role_json(NEW, "Shiny", 1, None));
    // Discord put the new role at 1 and moved the rest up by one.
    let mut bot_role = role_json("820000000000000009", "Bot", 7, None);
    bot_role["permissions"] = json!(P::all().bits().to_string());
    rest.on("GET", "/guilds/*/roles", 200, json!([
        role_json(G, "@everyone", 0, None),
        role_json(NEW, "Shiny", 1, None),
        role_json("820000000000000001", "Pingable", 2, None),
        role_json("820000000000000002", "Quiet", 3, None),
        role_json(NITRO, "Nitro Booster", 4, Some(json!({ "premium_subscriber": null }))),
        bot_role,
        role_json("820000000000000010", "Admin", 11, None),
    ]));
    let mut fi = FakeInteraction::slash("boost").guild(G).user(MEMBER).group("role", "create").option("name", STRING, json!("Shiny"));
    fi.json_mut()["member"]["premium_since"] = json!("2026-01-01T00:00:00.000000+00:00");
    let calls = route(&t, fi).await;
    assert_eq!(last(&calls).0, "✅  Booster Role Created", "{calls:?}");
    let patches = rest.calls_to("PATCH", &format!("/guilds/{G}/roles"));
    assert_eq!(patches.len(), 1, "{:?}", rest.calls());
    assert_eq!(patches[0].reason.as_deref(), Some("Server Boosts: placing a booster role"));
    let order = [G, "820000000000000001", "820000000000000002", NITRO, NEW, "820000000000000009", "820000000000000010"];
    let expected: Vec<Value> = order.iter().enumerate().map(|(i, id)| json!({ "id": id, "position": i })).collect();
    assert_eq!(patches[0].body, Value::Array(expected));
    assert_eq!(bs::load_row(t.db(), G, MEMBER).await.expect("load").map(|r| r.role_id), Some(NEW.to_string()));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn a_finish_failure_without_a_discord_code_is_could_not_save() {
    // createBoosterRole: `err?.code && Number.isInteger(err.code) ? DISCORD : FAILED`, and the role is deleted again.
    use bot_core::command::option_type::STRING;
    const G: &str = "940000000000000019";
    const NEW: &str = "831900000000000001";
    let Some(t) = app(G).await else { return };
    let rest = bot_core::testing::FakeRest::new();
    t.app.set_discord(rest.discord(&[fake_guild(G, P::all()).json()]));
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("boostsEnabled", true).set("boostCustomRoles", true)).await.expect("settings");
    rest.on("POST", "/guilds/*/roles", 200, role_json(NEW, "Shiny", 1, None));
    rest.on("PUT", "/guilds/*/members/*/roles/*", 500, json!({ "message": "Internal Server Error" }));
    let mut fi = FakeInteraction::slash("boost").guild(G).user(MEMBER).group("role", "create").option("name", STRING, json!("Shiny"));
    fi.json_mut()["member"]["premium_since"] = json!("2026-01-01T00:00:00.000000+00:00");
    let calls = route(&t, fi).await;
    assert_eq!(last(&calls), ("❌  Could Not Save".into(), "Something went wrong. Please try again in a moment.".into()));
    assert_eq!(rest.calls_to("DELETE", &format!("/guilds/{G}/roles/{NEW}")).len(), 1, "cleaned up");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn staff_replies_show_the_discord_js_default_avatar_and_server_icon() {
    // staffAuthor / staffMessageEmbed: `member.displayAvatarURL()` and `guild.iconURL()` with no options (webp, gif
    // when animated, no size).
    const G: &str = "940000000000000020";
    let Some(t) = app(G).await else { return };
    let rest = bot_core::testing::FakeRest::new();
    let thread = thread_of(G);
    let guild = fake_guild(G, P::all())
        .field("icon", json!("a_fedcba9876543210fedcba9876543210"))
        .member_with(ADMIN2, &["820000000000000010"], |m| m["avatar"] = json!("00112233445566778899aabbccddeeff"))
        .thread_owner(&thread, TEST_BOT_ID);
    t.app.set_discord(rest.discord(&[guild.json()]));
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modmailEnabled", true).set("modmailChannelId", STAFF_CH)).await.expect("settings");
    let row = match mm::reserve_open(&t, G, MEMBER).await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &row.id, &thread).await.expect("attach");
    rest.on("GET", &format!("/users/{MEMBER}"), 200, json!({ "id": MEMBER, "username": "member", "discriminator": "0", "avatar": null }));
    let cached = |id: &str| {
        let g = bot_core::interaction_helpers::cached_guild(&t, gid(G)).expect("guild");
        g.members.get(&serenity::all::UserId::new(id.parse().unwrap_or(1))).cloned()
    };
    let member = cached(ADMIN2);
    let channel = bot_core::interaction_helpers::cached_channel(&t, gid(G), serenity::all::ChannelId::new(thread.parse().unwrap_or(1)));
    let message = bot_core::testing::fake_message("700000000000000021", G, &thread, ADMIN2, "We are on it.");
    t.run_hooks(MessageCreate { guild_id: gid(G), message, member, channel_id: serenity::all::ChannelId::new(thread.parse().unwrap_or(1)), channel }).await;
    let dm: Vec<_> = rest.calls_to("POST", "/channels/*/messages").into_iter().filter(|c| c.body["embeds"][0]["author"].is_object()).collect();
    assert_eq!(dm.len(), 1, "{:?}", rest.calls());
    let embed = &dm[0].body["embeds"][0];
    assert_eq!(embed["author"]["icon_url"], json!(format!("https://cdn.discordapp.com/guilds/{G}/users/{ADMIN2}/avatars/00112233445566778899aabbccddeeff.webp")));
    assert_eq!(embed["footer"]["icon_url"], json!(format!("https://cdn.discordapp.com/icons/{G}/a_fedcba9876543210fedcba9876543210.gif")));
    assert_eq!(embed["description"], json!("We are on it."));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn modmail_close_works_in_an_uncached_thread_like_discord_js_partials() {
    // discord.js (Partials.Channel) caches the interaction's own channel, so `interaction.channel.isThread()` is true in
    // an archived thread the cache did not hold; the command must not answer "Not a Modmail Thread" there.
    const G: &str = "940000000000000031";
    const UNCACHED: &str = "817000000000000031";
    let Some(t) = app(G).await else { return };
    with_discord(&t, G, P::all());
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("modmailEnabled", true).set("modmailChannelId", STAFF_CH)).await.expect("settings");
    let row = match mm::reserve_open(&t, G, MEMBER).await.expect("reserve") {
        mm::Reserved::Row(r) => r,
        other => panic!("{other:?}"),
    };
    mm::attach_thread(&t, &row.id, UNCACHED).await.expect("attach");
    let mut fi = admin("modmail", G).subcommand("close").channel(UNCACHED);
    fi.json_mut()["channel"] = json!({ "id": UNCACHED, "type": 12, "name": "modmail-member", "parent_id": STAFF_CH, "guild_id": G });
    let calls = route(&t, fi).await;
    assert_eq!(last(&calls).0, "✅  Conversation Closed");
    let r = mm::find_by_thread(t.db(), UNCACHED).await.expect("q").expect("row");
    assert_eq!(r.status, ModmailStatus::Closed);
    t.reset_guilds(&[G]).await;
}

fn modmail_user(id: &str) -> serenity::all::User {
    serde_json::from_value(json!({ "id": id, "username": "Member Name", "discriminator": "0", "avatar": null, "bot": false })).expect("user")
}

async fn modmail_ready(g: &'static str) -> Option<(DbTestApp, bot_core::testing::FakeRest)> {
    let t = app(g).await?;
    let rest = bot_core::testing::FakeRest::new();
    t.app.set_discord(rest.discord(&[fake_guild(g, P::all()).json()]));
    t.settings()
        .update_settings(g, SettingsPatch::new().set("modmailEnabled", true).set("modmailChannelId", STAFF_CH).set("modmailPingRoleIds", json!(["820000000000000001"])))
        .await
        .expect("settings");
    Some((t, rest))
}

#[tokio::test]
async fn opening_falls_back_to_a_public_thread_and_says_so_and_pings_the_role() {
    // openConversation: a private thread Discord refuses becomes a public thread; the intro says why, pings only the
    // configured role, and the member's first message is relayed into it.
    const G: &str = "940000000000000032";
    const NEW: &str = "817000000000000032";
    let Some((t, rest)) = modmail_ready(G).await else { return };
    let thread = FakeGuild::new(G, OWNER).thread(NEW, 11, STAFF_CH).json()["threads"][0].clone();
    rest.on_with("POST", &format!("/channels/{STAFF_CH}/threads"), move |call| {
        if call.body["type"] == json!(12) { (403, json!({ "code": 50001, "message": "Missing Access" })) } else { (201, thread.clone()) }
    });
    let settings = t.settings().get_settings_safe(G).await.expect("settings");
    let opened = mmp::open_conversation(&t, gid(G), None, modmail_user(MEMBER), settings, mm::RelayMessage::text("hello staff")).await;
    let mmp::Opened::Ok { row, relayed, notes, .. } = opened else { panic!("{opened:?}") };
    assert!(relayed);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(row.thread_id, NEW);
    let creates = rest.calls_to("POST", &format!("/channels/{STAFF_CH}/threads"));
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0].body, json!({ "name": "modmail-member-name", "type": 12, "invitable": false, "auto_archive_duration": 10080 }));
    assert_eq!(creates[0].reason.as_deref(), Some(format!("Modmail conversation with Member Name ({MEMBER})").as_str()));
    assert_eq!(creates[1].body, json!({ "name": "modmail-member-name", "type": 11, "auto_archive_duration": 10080 }));
    let posts = rest.calls_to("POST", &format!("/channels/{NEW}/messages"));
    assert_eq!(posts.len(), 2, "the intro, then the relayed message");
    assert_eq!(posts[0].body["content"], json!("<@&820000000000000001>"));
    assert_eq!(posts[0].body["allowed_mentions"]["roles"], json!(["820000000000000001"]));
    let intro = posts[0].body["embeds"][0]["description"].as_str().unwrap_or_default().to_string();
    assert!(intro.contains("⚠️ Private threads could not be created here, so this conversation is in a **public** thread."), "{intro}");
    assert_eq!(posts[1].body["embeds"][0]["description"], json!("hello staff"));
    assert_eq!(mm::find_by_thread(t.db(), NEW).await.expect("q").map(|r| r.status), Some(ModmailStatus::Open));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn a_week_archive_refused_falls_back_to_a_day_and_a_failed_intro_deletes_the_thread() {
    // createThread: 50035 (the week needs a boost level) retries with one day. A failing intro deletes the thread and
    // drops the reserved row: CREATE_FAILED, and the member can try again.
    const G: &str = "940000000000000033";
    const NEW: &str = "817000000000000033";
    let Some((t, rest)) = modmail_ready(G).await else { return };
    let thread = FakeGuild::new(G, OWNER).thread(NEW, 12, STAFF_CH).json()["threads"][0].clone();
    rest.on_with("POST", &format!("/channels/{STAFF_CH}/threads"), move |call| {
        if call.body["auto_archive_duration"] == json!(10080) { (400, json!({ "code": 50035, "message": "Invalid Form Body" })) } else { (201, thread.clone()) }
    });
    rest.on("POST", &format!("/channels/{NEW}/messages"), 500, json!({ "message": "Internal Server Error" }));
    let settings = t.settings().get_settings_safe(G).await.expect("settings");
    let opened = mmp::open_conversation(&t, gid(G), None, modmail_user(MEMBER), settings, mm::RelayMessage::text("hello")).await;
    assert!(matches!(opened, mmp::Opened::Refused { code: "CREATE_FAILED", .. }), "{opened:?}");
    let creates = rest.calls_to("POST", &format!("/channels/{STAFF_CH}/threads"));
    assert_eq!(creates.iter().map(|c| c.body["auto_archive_duration"].clone()).collect::<Vec<_>>(), vec![json!(10080), json!(1440)]);
    let deletes = rest.calls_to("DELETE", &format!("/channels/{NEW}"));
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].reason.as_deref(), Some("Modmail conversation could not be saved"));
    assert!(mm::find_open_in_guild(t.db(), G, MEMBER).await.expect("q").is_none());
    assert!(matches!(mm::reserve_open(&t, G, MEMBER).await.expect("reserve"), mm::Reserved::Row(_)), "the reserved row was dropped");
    t.reset_guilds(&[G]).await;
}
