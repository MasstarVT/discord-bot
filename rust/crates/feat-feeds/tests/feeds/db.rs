//! The database cases of `test/feeds.test.js` (TEST_DATABASE_URL): the updatedAt-guarded claim, failure counting and
//! auto-disable, the unique index, the cap under the lock, the one-notice disable, the channelDelete cleanup, the job
//! repair; then the `/feed` command through the router, and both directions of the JS round trip.

use std::sync::Arc;

use super::*;
use network::{REDDIT, bsky_json};

pub(crate) struct H {
    pub t: DbTestApp,
    pub g: &'static str,
    pub ch: &'static str,
    pub rest: Arc<RecRest>,
}

pub(crate) async fn harness(g: &'static str, ch: &'static str, denied: Permissions) -> Option<H> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    t.set_discord(offline_discord_as(&[fake_guild(g, ch, denied).json()]));
    let rest = RecRest::install(&t);
    Some(H { t, g, ch, rest })
}

impl H {
    async fn feed(&self, extra: impl FnOnce(&mut svc::NewFeed)) -> Feed {
        let mut n = svc::NewFeed {
            guild_id: self.g.into(),
            feed_type: FeedType::Rss,
            source: format!("https://feed.test/{}.xml", bot_db::new_cuid()),
            display_name: "Test feed".into(),
            fetch_url: "https://feed.test/f.xml".into(),
            channel_id: self.ch.into(),
            ping_role_id: None,
            message_template: None,
            include_reposts: false,
            created_by: "u1".into(),
            probe: None,
        };
        extra(&mut n);
        svc::create_feed_row(&self.t, n).await.expect("created")
    }

    async fn poll(&self, feed: &Feed) -> JobOutcome {
        svc::poll_feed_job(job(self.g, json!({ "feedId": feed.id })), self.t.app.clone()).await.expect("handled")
    }

    async fn row(&self, feed: &Feed) -> Feed {
        svc::get_feed(self.t.db(), self.g, &feed.id).await.expect("read").expect("row")
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

pub(crate) fn job(guild: &str, payload: Value) -> Job {
    Job {
        id: "job".into(),
        guild_id: guild.into(),
        job_type: "feed.poll".into(),
        payload,
        run_at: Utc::now(),
        attempts: 1,
        max_attempts: 10,
        dedupe_key: None,
        version: 0,
    }
}

fn seeded(ids: &[&str]) -> Option<svc::Probe> {
    Some(svc::Probe::Ok { items: ids.iter().map(|i| FeedItem::new(*i, *i)).collect(), etag: None, last_modified: None })
}

/// An RSS document whose items are fresh (inside the stale window); `(id, minutes ago)`.
pub(crate) fn rss_xml(items: &[(&str, i64)]) -> String {
    let body: String = items
        .iter()
        .map(|(id, ago)| {
            let at = (Utc::now() - chrono::Duration::minutes(*ago)).to_rfc2822();
            format!("<item><title>{id}</title><guid isPermaLink=\"false\">{id}</guid><link>https://example.com/{id}</link><pubDate>{at}</pubDate></item>")
        })
        .collect();
    format!("<rss version=\"2.0\"><channel><title>Example</title>{body}</channel></rss>")
}

fn within(outcome: &JobOutcome, from_ms: i64, to_ms: i64) -> bool {
    match outcome {
        JobOutcome::Reschedule { next_run_at, .. } => {
            let d = next_run_at.timestamp_millis() - Utc::now().timestamp_millis();
            d >= from_ms - 2_000 && d <= to_ms
        }
        JobOutcome::Done => false,
    }
}

fn titles(sent: &[(serenity::all::ChannelId, Value)]) -> Vec<String> {
    sent.iter().map(|(_, b)| b["embeds"][0]["title"].as_str().unwrap_or("").to_string()).collect()
}

const G_CREATE: &str = "955130000000000001";
const G_CAP: &str = "955130000000000002";
const G_FIND: &str = "955130000000000003";
const G_POLL: &str = "955130000000000004";
const G_RACE: &str = "955130000000000005";
const G_FAIL: &str = "955130000000000006";
const G_REDDIT: &str = "955130000000000007";
const G_CHAN: &str = "955130000000000008";
const G_MODOFF: &str = "955130000000000009";
const G_DEL: &str = "955130000000000010";
const G_JOBS: &str = "955130000000000011";
const G_MISC: &str = "955130000000000012";
const G_CMD: &str = "955130000000000013";
const G_XL: &str = "955130000000000014";
const G_SPARE: &str = "955130000000000015";
const G_DENY: &str = "955130000000000016";
const G_LIST: &str = "955130000000000017";
const CH: &str = "810000000000000001";

#[tokio::test]
async fn create_feed_row_seeds_from_a_good_probe_and_maps_the_unique_index() {
    let Some(h) = harness(G_CREATE, CH, Permissions::empty()).await else { return };
    let row = h
        .feed(|n| {
            n.source = "https://s.test/1".into();
            n.display_name = "x".repeat(300);
            n.probe = Some(svc::Probe::Ok { items: vec![FeedItem::new("a", "a"), FeedItem::new("b", "b"), FeedItem::new("b", "b")], etag: Some("\"e\"".into()), last_modified: Some("lm".into()) });
        })
        .await;
    assert!(row.initialized);
    assert_eq!(row.seen_ids, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(row.etag.as_deref(), Some("\"e\""));
    assert_eq!(row.display_name.chars().count(), 100);
    assert!(row.last_success_at.is_some() && row.last_checked_at.is_some());
    assert_eq!(row.last_error, None);

    let mut e = svc::FetchError::new("HTTP", "HTTP 404");
    e.status = Some(404);
    let failed = h
        .feed(|n| {
            n.feed_type = FeedType::Youtube;
            n.source = "UCx".into();
            n.probe = Some(svc::Probe::Failed(e));
        })
        .await;
    assert!(!failed.initialized && failed.seen_ids.is_empty() && failed.last_success_at.is_none());
    assert!(failed.last_checked_at.is_some());
    assert!(failed.last_error.as_deref().unwrap_or("").contains("HTTP 404"));
    assert_eq!(failed.failure_count, 0);

    let dup = svc::NewFeed {
        guild_id: G_CREATE.into(),
        feed_type: FeedType::Rss,
        source: "https://s.test/1".into(),
        display_name: "dup".into(),
        fetch_url: "x".into(),
        channel_id: CH.into(),
        ping_role_id: None,
        message_template: None,
        include_reposts: false,
        created_by: "u".into(),
        probe: None,
    };
    let err = svc::create_feed_row(&h.t, dup.clone()).await.expect_err("duplicate");
    assert_eq!(err.downcast_ref::<svc::FeedUserError>().map(|u| u.title.as_str()), Some("Already Following"));
    let rb = svc::create_feed_row(&h.t, svc::NewFeed { source: "https://s.test/9".into(), include_reposts: true, ..dup }).await.expect("created");
    assert!(!rb.include_reposts, "includeReposts is Bluesky-only");
    h.done().await;
}

#[tokio::test]
async fn the_per_guild_cap_holds_under_concurrent_adds() {
    let Some(h) = harness(G_CAP, CH, Permissions::empty()).await else { return };
    let limit = bot_config::limits::FEEDS;
    for i in 0..limit - 2 {
        h.feed(|n| n.source = format!("bulk-{i}")).await;
    }
    let mut futs = Vec::new();
    for i in 0..6 {
        let app = h.t.app.clone();
        futs.push(tokio::spawn(async move {
            svc::create_feed_row(
                &app,
                svc::NewFeed {
                    guild_id: G_CAP.into(),
                    feed_type: FeedType::Rss,
                    source: format!("race-{i}"),
                    display_name: format!("r{i}"),
                    fetch_url: "x".into(),
                    channel_id: CH.into(),
                    ping_role_id: None,
                    message_template: None,
                    include_reposts: false,
                    created_by: "u".into(),
                    probe: None,
                },
            )
            .await
        }));
    }
    let mut ok = 0;
    let mut refused = 0;
    for f in futs {
        match f.await.expect("joined") {
            Ok(_) => ok += 1,
            Err(e) => {
                assert_eq!(e.downcast_ref::<svc::FeedUserError>().map(|u| u.title.as_str()), Some("Feed Limit Reached"));
                refused += 1;
            }
        }
    }
    assert_eq!((ok, refused), (2, 4));
    assert_eq!(svc::count_feeds(h.t.db(), G_CAP).await.expect("count"), limit as i64);
    h.done().await;
}

#[tokio::test]
async fn find_feed_for_guild_by_id_by_unique_name_and_guild_scoped() {
    let Some(h) = harness(G_FIND, CH, Permissions::empty()).await else { return };
    let Some(other) = harness(G_SPARE, CH, Permissions::empty()).await else { return };
    let a = h.feed(|n| n.display_name = "Alpha Feed".into()).await;
    h.feed(|n| n.display_name = "Twin".into()).await;
    h.feed(|n| n.display_name = "twin".into()).await;
    let o = other.feed(|n| n.display_name = "Other".into()).await;
    assert_eq!(svc::find_feed_for_guild(h.t.db(), G_FIND, &a.id).await.expect("q").map(|f| f.id), Some(a.id.clone()));
    assert_eq!(svc::find_feed_for_guild(h.t.db(), G_FIND, "alpha feed").await.expect("q").map(|f| f.id), Some(a.id.clone()));
    assert_eq!(svc::find_feed_for_guild(h.t.db(), G_FIND, "twin").await.expect("q"), None, "ambiguous names match nothing");
    assert_eq!(svc::find_feed_for_guild(h.t.db(), G_FIND, &o.id).await.expect("q"), None, "another guild's id never matches");
    assert_eq!(svc::find_feed_for_guild(h.t.db(), G_FIND, "nope").await.expect("q"), None);
    other.done().await;
    h.done().await;
}

#[tokio::test]
async fn poll_seeds_first_then_posts_only_new_items_and_honours_304() {
    let Some(h) = harness(G_POLL, CH, Permissions::empty()).await else { return };
    let feed = h.feed(|_| {}).await;
    let calls = StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("p1", 60), ("p2", 61)]), &XML_HEADERS));
    let out = h.poll(&feed).await;
    assert_eq!(h.rest.count(), 0, "no backlog dump");
    assert!(within(&out, 13 * 60_000, 17 * 60_000), "{out:?}");
    assert_eq!(calls.header(0, "if-none-match"), None, "no conditional headers before the first success");
    let row = h.row(&feed).await;
    assert!(row.initialized);
    let mut seen = row.seen_ids.clone();
    seen.sort();
    assert_eq!(seen, vec!["p1".to_string(), "p2".to_string()]);
    assert_eq!((row.failure_count, row.last_error.clone(), row.etag.clone()), (0, None, Some("\"abc\"".into())));
    assert!(row.last_success_at.is_some() && row.last_posted_at.is_none());

    h.t.cache().del_one(&svc::fetch_cache_key("https://feed.test/f.xml")).await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("p3", 10), ("p1", 60), ("p2", 61)]), &XML_HEADERS));
    let out = h.poll(&feed).await;
    let sent = h.rest.sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1["content"].as_str().unwrap_or("").contains("Test feed"));
    assert_eq!(sent[0].1["embeds"][0]["title"], "p3");
    assert_eq!(sent[0].1["allowed_mentions"], json!({ "parse": [], "roles": [] }));
    assert_eq!(sent[0].0.to_string(), CH);
    assert!(within(&out, 13 * 60_000, 17 * 60_000));
    let row = h.row(&feed).await;
    assert!(row.seen_ids.contains(&"p3".to_string()) && row.last_posted_at.is_some());
    h.poll(&feed).await;
    assert_eq!(h.rest.count(), 1, "the same state again posts nothing");

    let c2 = StubFetcher::install(&h.t, |_, _| resp(304, "", &[]));
    h.t.cache().del_one(&svc::fetch_cache_key("https://feed.test/f.xml")).await;
    bot_db::UpdateBuilder::new(&bot_db::FEED_INFO).set("failureCount", json!(3)).and_then(|b| b.set("lastError", json!("HTTP 500"))).and_then(|b| b.filter("id", json!(feed.id))).and_then(|b| b.without_returning().build()).expect("sql").execute(h.t.db()).await.expect("update");
    let out = h.poll(&feed).await;
    assert_eq!(c2.header(0, "if-none-match").as_deref(), Some("\"abc\""));
    assert!(within(&out, 13 * 60_000, 17 * 60_000), "a 304 is a success: no backoff");
    let row = h.row(&feed).await;
    assert_eq!((row.failure_count, row.last_error), (0, None));
    h.done().await;
}

#[tokio::test]
async fn poll_the_updated_at_claim_stops_a_parallel_or_stale_run_from_posting() {
    let Some(h) = harness(G_RACE, CH, Permissions::empty()).await else { return };
    let feed = h.feed(|n| n.probe = seeded(&["q1"])).await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("q2", 5), ("q1", 60)]), &XML_HEADERS));
    // Both runs read the row as it was before either claimed.
    let snapshot = h.row(&feed).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let first = svc::poll_feed_row(&h.t, &snapshot).await.expect("ok");
    let second = svc::poll_feed_row(&h.t, &snapshot).await.expect("ok");
    assert_eq!(h.rest.count(), 1, "exactly one post");
    assert!(within(&first, 13 * 60_000, 17 * 60_000));
    assert!(within(&second, 50_000, 70_000), "the second claim lost and retries in a minute: {second:?}");
    let row = h.row(&feed).await;
    assert!(row.updated_at > snapshot.updated_at, "the claim bumped updatedAt");
    assert!(row.seen_ids.contains(&"q2".to_string()));

    // A run that read the row before an admin edited it posts and records nothing.
    bot_db::UpdateBuilder::new(&bot_db::FEED_INFO).set("seenIds", json!(["q1"])).and_then(|b| b.filter("id", json!(feed.id))).and_then(|b| b.without_returning().build()).expect("sql").execute(h.t.db()).await.expect("update");
    let stale = h.row(&feed).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    bot_db::UpdateBuilder::new(&bot_db::FEED_INFO).set("messageTemplate", json!("edited {title}")).and_then(|b| b.filter("id", json!(feed.id))).and_then(|b| b.without_returning().build()).expect("sql").execute(h.t.db()).await.expect("update");
    h.t.cache().del_one(&svc::fetch_cache_key("https://feed.test/f.xml")).await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("q9", 5), ("q1", 60)]), &XML_HEADERS));
    let out = svc::poll_feed_row(&h.t, &stale).await.expect("ok");
    assert_eq!(h.rest.count(), 1, "no new post");
    assert!(within(&out, 50_000, 70_000));
    assert!(!h.row(&feed).await.seen_ids.contains(&"q9".to_string()), "nothing was recorded either");
    h.done().await;
}

#[tokio::test]
async fn poll_posts_at_most_five_oldest_first_and_records_everything() {
    let Some(h) = harness(G_MISC, CH, Permissions::empty()).await else { return };
    let feed = h.feed(|n| n.probe = seeded(&["old"])).await;
    let items: Vec<(String, i64)> = (1..=8).rev().map(|i| (format!("n{i}"), 120 - i)).collect();
    let mut list: Vec<(&str, i64)> = items.iter().map(|(a, b)| (a.as_str(), *b)).collect();
    list.push(("old", 180));
    let xml = rss_xml(&list);
    StubFetcher::install(&h.t, move |_, _| resp(200, &xml, &XML_HEADERS));
    h.poll(&feed).await;
    assert_eq!(titles(&h.rest.sent()), vec!["n4", "n5", "n6", "n7", "n8"]);
    let row = h.row(&feed).await;
    for i in 1..=8 {
        assert!(row.seen_ids.contains(&format!("n{i}")));
    }
    h.done().await;
}

#[tokio::test]
async fn poll_a_failed_send_unrecords_the_item_and_50035_skips_it_for_good() {
    let Some(h) = harness(G_FAIL, CH, Permissions::empty()).await else { return };
    let feed = h.feed(|n| n.probe = seeded(&["base"])).await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("r1", 5), ("base", 60)]), &XML_HEADERS));
    h.rest.fail_with(Some(feat_feeds::net::RestError::code(50013, "Missing Permissions")));
    assert!(within(&h.poll(&feed).await, 10 * 60_000, 20 * 60_000));
    let row = h.row(&feed).await;
    assert!(!row.seen_ids.contains(&"r1".to_string()), "un-recorded");
    assert!(row.last_error.as_deref().unwrap_or("").contains("Can't post in #news: Missing Permissions"));
    assert_eq!((row.failure_count, row.enabled), (0, true));
    h.rest.fail_with(None);
    h.t.cache().del_one(&svc::fetch_cache_key("https://feed.test/f.xml")).await;
    h.poll(&feed).await;
    assert_eq!(h.rest.count(), 1, "retried next poll");
    let row = h.row(&feed).await;
    assert!(row.seen_ids.contains(&"r1".to_string()));
    assert_eq!(row.last_error, None, "a successful poll clears the channel problem");

    h.t.cache().del_one(&svc::fetch_cache_key("https://feed.test/f.xml")).await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("bad1", 5), ("r1", 6), ("base", 60)]), &XML_HEADERS));
    h.rest.fail_with(Some(feat_feeds::net::RestError::code(50035, "Invalid Form Body")));
    h.poll(&feed).await;
    let row = h.row(&feed).await;
    assert!(row.seen_ids.contains(&"bad1".to_string()), "a permanently rejected item stays recorded");
    assert_eq!(row.last_error, None);
    h.rest.fail_with(None);

    // Source failures back off, count, and the 20th disables with exactly one notice.
    bot_db::UpdateBuilder::new(&bot_db::FEED_INFO).set("failureCount", json!(18)).and_then(|b| b.filter("id", json!(feed.id))).and_then(|b| b.without_returning().build()).expect("sql").execute(h.t.db()).await.expect("update");
    h.t.cache().del_one(&svc::fetch_cache_key("https://feed.test/f.xml")).await;
    StubFetcher::install(&h.t, |_, _| resp(500, "", &[]));
    let before = h.rest.count();
    let out = h.poll(&feed).await;
    let row = h.row(&feed).await;
    assert_eq!(row.failure_count, 19);
    assert!(row.last_error.as_deref().unwrap_or("").contains("HTTP 500 — the site is having problems"));
    assert!(within(&out, 7 * 3_600_000, 9 * 3_600_000), "capped exponential backoff (8 h at ×32): {out:?}");
    assert_eq!(h.poll(&feed).await, JobOutcome::Done, "the job is dropped");
    let row = h.row(&feed).await;
    assert!(!row.enabled);
    assert!(row.disabled_reason.as_deref().unwrap_or("").starts_with("20 failed checks in a row: HTTP 500"));
    let sent = h.rest.sent();
    assert_eq!(sent.len(), before + 1, "exactly one notice");
    assert_eq!(sent[before].1["embeds"][0]["title"], "⚠️  Feed Disabled");
    assert!(sent[before].1["embeds"][0]["description"].as_str().unwrap_or("").contains("/feed enable"));
    assert_eq!(h.poll(&feed).await, JobOutcome::Done);
    assert_eq!(h.rest.count(), before + 1, "a disabled feed is never polled again");

    let feed2 = h.feed(|_| {}).await;
    let (a, b) = tokio::join!(svc::disable_feed(&h.t, &feed2, "x", None, false), svc::disable_feed(&h.t, &feed2, "x", None, false));
    let mut r = vec![a.expect("ok"), b.expect("ok")];
    r.sort();
    assert_eq!(r, vec![false, true], "the conditional update gives one winner");
    h.done().await;
}

#[tokio::test]
async fn poll_reddit_429_waits_without_counting_and_the_budget_touches_nothing() {
    let Some(h) = harness(G_REDDIT, CH, Permissions::empty()).await else { return };
    let reddit = h
        .feed(|n| {
            n.feed_type = FeedType::Reddit;
            n.source = "aww".into();
            n.display_name = "r/aww".into();
            n.fetch_url = format!("https://www.reddit.com/r/aww{}/new/.rss", bot_db::new_cuid());
            n.probe = seeded(&["t3_a"]);
        })
        .await;
    h.t.cache().del_one(svc::REDDIT_BLOCK_KEY).await;
    StubFetcher::install(&h.t, |_, _| resp(429, "slow", &[("retry-after", "1800")]));
    let out = h.poll(&reddit).await;
    assert!(within(&out, 29 * 60_000, 32 * 60_000), "Retry-After 30 min is honoured: {out:?}");
    let row = h.row(&reddit).await;
    assert_eq!(row.failure_count, 0);
    let err = row.last_error.unwrap_or_default();
    assert!(err.starts_with("HTTP 429 — Reddit rate limit — all Reddit feeds wait until <t:") && err.ends_with(":t>"), "{err}");
    h.t.cache().del_one(svc::REDDIT_BLOCK_KEY).await;
    StubFetcher::install(&h.t, |_, _| resp(403, "blocked", &[]));
    let out = h.poll(&reddit).await;
    assert_eq!(h.row(&reddit).await.failure_count, 1);
    assert!(within(&out, 54 * 60_000, 66 * 60_000), "a Reddit 403 backs off at least an hour");
    let _ = REDDIT;
    h.done().await;
}

#[tokio::test]
async fn poll_channel_problems_never_count_as_failures() {
    let Some(h) = harness(G_DENY, CH, Permissions::SEND_MESSAGES).await else { return };
    let feed = h.feed(|n| n.probe = seeded(&["c1"])).await;
    let calls = StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("c2", 5), ("c1", 60)]), &XML_HEADERS));
    let out = h.poll(&feed).await;
    assert_eq!(calls.len(), 0, "nothing is fetched that cannot be posted");
    let row = h.row(&feed).await;
    assert_eq!(row.failure_count, 0);
    assert!(row.last_error.as_deref().unwrap_or("").contains("Can't post in #news: missing SendMessages"), "{:?}", row.last_error);
    assert!(within(&out, 25 * 60_000, 35 * 60_000), "one backoff step");
    h.done().await;

    let Some(h) = harness(G_CHAN, CH, Permissions::VIEW_CHANNEL).await else { return };
    let feed = h.feed(|n| n.probe = seeded(&["c1"])).await;
    assert_eq!(h.poll(&feed).await, JobOutcome::Done);
    let row = h.row(&feed).await;
    assert!(!row.enabled);
    assert!(row.disabled_reason.as_deref().unwrap_or("").contains("lost access to #news (View Channel)"));
    // A channel missing from the cache is confirmed with a REST fetch before anything is disabled.
    let feed2 = h.feed(|n| n.channel_id = "810000000000000555".into()).await;
    let out = h.poll(&feed2).await;
    assert_eq!(*h.rest.fetches.lock().expect("lock"), 1);
    assert!(within(&out, 4 * 60_000, 6 * 60_000));
    assert!(h.row(&feed2).await.enabled, "a transient error disables nothing");
    h.rest.on_fetch(Some(Err(feat_feeds::net::RestError::code(10003, "Unknown Channel"))));
    assert_eq!(h.poll(&feed2).await, JobOutcome::Done);
    let row = h.row(&feed2).await;
    assert!(!row.enabled && row.disabled_reason.as_deref().unwrap_or("").contains("channel was deleted or I lost access"));
    let feed3 = h.feed(|n| n.channel_id = "810000000000000078".into()).await;
    assert_eq!(h.poll(&feed3).await, JobOutcome::Done);
    assert!(h.row(&feed3).await.disabled_reason.as_deref().unwrap_or("").contains("can no longer receive posts"));
    h.done().await;
}

/// `guild.channels.fetch(id)` refuses another guild's channel (GuildChannelUnowned, no code): the poll waits 5 minutes
/// and neither posts there nor disables; a channel of this guild found over REST is posted in.
#[tokio::test]
async fn poll_never_posts_in_another_guilds_channel_found_over_rest() {
    let Some(h) = harness("955130000000000031", CH, Permissions::empty()).await else { return };
    let feed = h.feed(|n| {
        n.channel_id = "810000000000000556".into();
        n.probe = seeded(&["a1"]);
    })
    .await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("a1", 5), ("a2", 3)]), &XML_HEADERS));
    h.rest.on_fetch(Some(Ok(Some(rest_channel(&h.t, h.g, "955130000000000099", "810000000000000556")))));
    let out = h.poll(&feed).await;
    assert!(within(&out, 4 * 60_000, 6 * 60_000), "{out:?}");
    assert_eq!(h.rest.count(), 0, "nothing is posted in another guild");
    assert!(h.row(&feed).await.enabled);
    h.rest.on_fetch(Some(Ok(None)));
    assert!(within(&h.poll(&feed).await, 4 * 60_000, 6 * 60_000), "a channel that is not a guild channel is the same error");
    assert!(h.row(&feed).await.enabled);
    h.rest.on_fetch(Some(Ok(Some(rest_channel(&h.t, h.g, h.g, "810000000000000556")))));
    let _ = h.poll(&feed).await;
    assert_eq!(h.rest.count(), 1, "this guild's channel found over REST is posted in");
    assert_eq!(h.rest.sent()[0].0.to_string(), "810000000000000556");
    h.done().await;
}

#[tokio::test]
async fn poll_module_off_paused_removed_and_malformed_jobs_do_no_work() {
    let Some(h) = harness(G_MODOFF, CH, Permissions::empty()).await else { return };
    let feed = h.feed(|n| n.probe = seeded(&["m1"])).await;
    let calls = StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("m2", 5), ("m1", 60)]), &XML_HEADERS));
    h.t.settings().update_settings(G_MODOFF, SettingsPatch::new().set("feedsEnabled", false)).await.expect("settings");
    let out = h.poll(&feed).await;
    assert_eq!(calls.len(), 0, "no fetch while the module is off");
    assert!(within(&out, 29 * 60_000, 31 * 60_000));
    h.t.settings().update_settings(G_MODOFF, SettingsPatch::new().set("feedsEnabled", true)).await.expect("settings");
    svc::disable_feed(&h.t, &feed, "Paused by an admin", None, false).await.expect("ok");
    assert_eq!(h.poll(&feed).await, JobOutcome::Done);
    let mut gone = feed.clone();
    gone.id = "does-not-exist".into();
    assert_eq!(h.poll(&gone).await, JobOutcome::Done);
    assert_eq!(svc::poll_feed_job(job(G_MODOFF, json!({})), h.t.app.clone()).await.expect("ok"), JobOutcome::Done);
    assert_eq!(svc::poll_feed_job(job(G_SPARE, json!({ "feedId": feed.id })), h.t.app.clone()).await.expect("ok"), JobOutcome::Done, "another guild's job never reaches the row");
    assert_eq!(calls.len(), 0);
    h.done().await;
}

#[tokio::test]
async fn poll_refreshes_display_names_and_follows_include_reposts() {
    let Some(h) = harness(G_XL, CH, Permissions::empty()).await else { return };
    let yt_id = format!("UC{}", "c".repeat(22));
    let yt = h
        .feed(|n| {
            n.feed_type = FeedType::Youtube;
            n.source = yt_id.clone();
            n.display_name = yt_id.clone();
            n.fetch_url = "https://y.test/f".into();
        })
        .await;
    StubFetcher::install(&h.t, |_, _| resp(200, network::YT, &XML_HEADERS));
    h.poll(&yt).await;
    assert_eq!(h.row(&yt).await.display_name, "Chan");
    assert_eq!(h.rest.count(), 0);

    let now = bot_db::format_js_date(&Utc::now());
    let body = bsky_json(&now);
    StubFetcher::install(&h.t, move |_, _| resp(200, &body, &[]));
    let bsky = h
        .feed(|n| {
            n.feed_type = FeedType::Bluesky;
            n.source = "did:plc:abc".into();
            n.display_name = "@old.bsky.social".into();
            n.fetch_url = "https://b.test/f".into();
            n.probe = seeded(&[]);
        })
        .await;
    h.poll(&bsky).await;
    let row = h.row(&bsky).await;
    assert_eq!(row.display_name, "@alice.bsky.social", "the handle is refreshed from the posts");
    assert_eq!(h.rest.count(), 1, "only the original post; the repost is recorded but not posted");
    assert!(row.seen_ids.iter().any(|id| id.starts_with("repost:")));
    let with = h
        .feed(|n| {
            n.feed_type = FeedType::Bluesky;
            n.source = "did:plc:abc2".into();
            n.display_name = "@alice.bsky.social".into();
            n.fetch_url = "https://b.test/f2".into();
            n.include_reposts = true;
            n.probe = seeded(&[]);
        })
        .await;
    h.poll(&with).await;
    let sent = h.rest.sent();
    assert_eq!(sent.len(), 3);
    assert!(sent[2].1["content"].as_str().unwrap_or("").contains("**Alice** (@alice.bsky.social)"));
    h.done().await;
}

#[tokio::test]
async fn channel_delete_disables_and_keeps_the_rows_with_one_notice_in_the_mod_log() {
    let Some(h) = harness(G_DEL, CH, Permissions::empty()).await else { return };
    let gone = "810000000000000003";
    let a = h.feed(|n| n.channel_id = gone.into()).await;
    let b = h.feed(|n| n.channel_id = gone.into()).await;
    let keep = h.feed(|_| {}).await;
    for f in [&a, &b, &keep] {
        svc::schedule_feed_job(&h.t, G_DEL, &f.id, Utc::now() + chrono::Duration::minutes(1)).await.expect("scheduled");
    }
    let mod_log = "810000000000000077";
    assert_eq!(svc::disable_feeds_for_channel(&h.t, G_DEL, gone, Some(mod_log)).await.expect("ok"), 2);
    let row = h.row(&a).await;
    assert_eq!((row.enabled, row.disabled_reason.as_deref()), (false, Some("Its channel was deleted")));
    assert!(h.row(&keep).await.enabled);
    assert!(h.t.scheduler().get_job(&svc::feed_job_key(&a.id)).await.is_none());
    assert!(h.t.scheduler().get_job(&svc::feed_job_key(&keep.id)).await.is_some(), "other channels' jobs are untouched");
    let sent = h.rest.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0.to_string(), mod_log);
    assert_eq!(sent[0].1["embeds"][0]["title"], "⚠️  Feeds Disabled");
    assert!(sent[0].1["embeds"][0]["description"].as_str().unwrap_or("").contains("edit channel:"));
    assert_eq!(svc::disable_feeds_for_channel(&h.t, G_DEL, gone, Some(mod_log)).await.expect("ok"), 0, "idempotent");
    assert_eq!(h.rest.count(), 1);
    let _ = b;
    // Through the hook, with the channel filter.
    h.done().await;
}

#[tokio::test]
async fn guild_ready_repairs_missing_and_failed_jobs_and_leaves_healthy_ones() {
    let Some(h) = harness(G_JOBS, CH, Permissions::empty()).await else { return };
    let healthy = h.feed(|_| {}).await;
    let missing = h.feed(|_| {}).await;
    let failed = h.feed(|_| {}).await;
    let paused = h.feed(|_| {}).await;
    svc::disable_feed(&h.t, &paused, "x", None, false).await.expect("ok");
    let run_at = bot_db::sqlx::types::chrono::DateTime::from_timestamp_millis((Utc::now() + chrono::Duration::hours(5)).timestamp_millis()).unwrap_or_default();
    svc::schedule_feed_job(&h.t, G_JOBS, &healthy.id, run_at).await.expect("ok");
    svc::schedule_feed_job(&h.t, G_JOBS, &failed.id, run_at).await.expect("ok");
    bot_db::sqlx::query(r#"UPDATE "ScheduledJob" SET "status" = 'FAILED' WHERE "dedupeKey" = $1"#).bind(svc::feed_job_key(&failed.id)).execute(h.t.db()).await.expect("sql");
    svc::ensure_guild_feed_jobs(&h.t, G_JOBS).await.expect("ok");
    let jh = h.t.scheduler().get_job(&svc::feed_job_key(&healthy.id)).await.expect("job");
    assert_eq!(jh.run_at.timestamp_millis(), run_at.timestamp_millis(), "a healthy job is not touched");
    let jm = h.t.scheduler().get_job(&svc::feed_job_key(&missing.id)).await.expect("job");
    assert_eq!(jm.status, bot_db::JobStatus::Pending);
    assert!(jm.run_at.timestamp_millis() >= Utc::now().timestamp_millis() + 29_000);
    assert_eq!(jm.max_attempts, 10);
    assert_eq!(jm.payload, json!({ "feedId": missing.id }));
    assert_eq!(jm.r#type, "feed.poll");
    assert_eq!(h.t.scheduler().get_job(&svc::feed_job_key(&failed.id)).await.map(|j| j.status), Some(bot_db::JobStatus::Pending), "a FAILED job is replaced");
    assert!(h.t.scheduler().get_job(&svc::feed_job_key(&paused.id)).await.is_none());
    h.done().await;
}

// ── /feed through the router ───────────────────────────────────────────────────

fn admin(sub: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash("feed").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).channel(CH).subcommand(sub)
}

fn add(sub: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash("feed").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).channel(CH).group("add", sub)
}

const STRING: u8 = bot_core::command::option_type::STRING;
const CHANNEL: u8 = bot_core::command::option_type::CHANNEL;
const ROLE: u8 = bot_core::command::option_type::ROLE;
const BOOLEAN: u8 = bot_core::command::option_type::BOOLEAN;

async fn route(h: &H, i: FakeInteraction) -> Vec<bot_core::interaction::RecordedCall> {
    h.t.responses.clear();
    bot_core::router::route_interaction(&h.t, i.build()).await;
    h.t.responses.calls()
}

fn last_title(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.last().and_then(|c| c.embed_title()).unwrap_or("").to_string()
}

fn last_desc(calls: &[bot_core::interaction::RecordedCall]) -> String {
    calls.last().and_then(|c| c.embed_description()).unwrap_or("").to_string()
}

#[tokio::test]
async fn feed_add_validates_probes_seeds_and_schedules() {
    let Some(h) = harness(G_CMD, CH, Permissions::empty()).await else { return };
    h.t.install(super::registry());
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("a1", 5), ("a2", 6)]), &XML_HEADERS));
    let calls = route(&h, add("rss", G_CMD).option("url", STRING, json!("ftp://x")).option("channel", CHANNEL, json!(CH))).await;
    assert_eq!(last_title(&calls), "❌  Invalid URL");
    assert!(calls[0].ephemeral());
    let calls = route(&h, add("rss", G_CMD).option("url", STRING, json!("https://feed.test/f.xml")).option("channel", CHANNEL, json!(CH)).option("ping_role", ROLE, json!(G_CMD))).await;
    assert_eq!(last_title(&calls), "❌  Invalid Role");
    let calls = route(&h, add("rss", G_CMD).option("url", STRING, json!("https://feed.test/f.xml")).option("channel", CHANNEL, json!(CH)).option("ping_role", ROLE, json!(ROLE_NO))).await;
    assert_eq!(last_title(&calls), "❌  Role Not Pingable");
    assert!(last_desc(&calls).contains(&format!("<@&{ROLE_NO}> is not mentionable and you lack **Mention @everyone")));
    let calls = route(&h, add("rss", G_CMD).option("url", STRING, json!("https://feed.test/f.xml")).option("channel", CHANNEL, json!("810000000000000078"))).await;
    assert_eq!(last_title(&calls), "❌  Invalid Channel");

    let calls = route(
        &h,
        add("rss", G_CMD)
            .option("url", STRING, json!("https://feed.test/f.xml"))
            .option("channel", CHANNEL, json!(CH))
            .option("ping_role", ROLE, json!(ROLE_OK))
            .option("message", STRING, json!("New: {title} {youtube_channel}")),
    )
    .await;
    assert_eq!(calls[0].method, "deferReply");
    assert!(!calls[0].ephemeral(), "admin results are public");
    assert_eq!(last_title(&calls), "✅  Feed Added");
    let d = last_desc(&calls);
    assert!(d.contains(&format!("📰 **Example** → <#{CH}>")), "{d}");
    assert!(d.contains("RSS feed · checked every 15 minutes · <https://feed.test/f.xml>"), "{d}");
    assert!(d.contains(&format!("Ping: <@&{ROLE_OK}>")));
    assert!(d.contains("✅ 2 existing item(s) recorded — only new ones will be posted."));
    assert!(d.contains("Variables: `{title}`"));
    assert!(d.contains("don't work for RSS feeds and will appear"));
    assert!(!d.contains("module is off"));
    let rows = svc::list_feeds(h.t.db(), G_CMD).await.expect("rows");
    assert_eq!(rows.len(), 1, "only the valid add created a row");
    let row = &rows[0];
    assert!(row.initialized);
    assert_eq!(row.ping_role_id.as_deref(), Some(ROLE_OK));
    assert_eq!(row.created_by, ADMIN);
    let j = h.t.scheduler().get_job(&svc::feed_job_key(&row.id)).await.expect("job");
    let d = j.run_at.timestamp_millis() - Utc::now().timestamp_millis();
    assert!((13 * 60_000..=16 * 60_000).contains(&d), "a seeded feed first runs after one interval");
    assert_eq!(h.rest.count(), 0, "adding a feed posts nothing");
    let calls = route(&h, add("rss", G_CMD).option("url", STRING, json!("https://feed.test/f.xml")).option("channel", CHANNEL, json!(CH))).await;
    assert_eq!(last_title(&calls), "❌  Already Following");

    StubFetcher::install(&h.t, |_, _| resp(404, "", &[]));
    let calls = route(&h, add("youtube", G_CMD).option("youtube_channel", STRING, json!(format!("UC{}", "d".repeat(22)))).option("channel", CHANNEL, json!(CH))).await;
    assert_eq!(last_title(&calls), "✅  Feed Added");
    assert!(last_desc(&calls).contains("couldn't fetch it yet: HTTP 404 — YouTube"));
    assert!(last_desc(&calls).contains("unreliable since late 2025"));
    let yt = svc::list_feeds(h.t.db(), G_CMD).await.expect("rows").into_iter().find(|f| f.r#type == FeedType::Youtube).expect("yt");
    assert!(!yt.initialized);
    let d = h.t.scheduler().get_job(&svc::feed_job_key(&yt.id)).await.expect("job").run_at.timestamp_millis() - Utc::now().timestamp_millis();
    assert!((100_000..=140_000).contains(&d));

    for (sub, opt, val, title) in [
        ("youtube", "youtube_channel", "@mkbhd", "❌  Channel ID Needed"),
        ("reddit", "subreddit", "u/spez", "❌  Invalid Subreddit"),
        ("bluesky", "handle", "not a handle!", "❌  Invalid Handle"),
        ("podcast", "podcast", "a", "❌  Invalid Search"),
    ] {
        let calls = route(&h, add(sub, G_CMD).option(opt, STRING, json!(val)).option("channel", CHANNEL, json!(CH))).await;
        assert_eq!(last_title(&calls), title, "{sub}");
    }
    StubFetcher::install(&h.t, |_, _| resp(200, r#"{"results":[]}"#, &[]));
    let calls = route(&h, add("podcast", G_CMD).option("podcast", STRING, json!("zzzz")).option("channel", CHANNEL, json!(CH))).await;
    assert_eq!(last_title(&calls), "❌  Podcast Not Found");

    h.t.settings().update_settings(G_CMD, SettingsPatch::new().set("feedsEnabled", false)).await.expect("settings");
    let calls = route(&h, admin("list", G_CMD)).await;
    assert!(last_desc(&calls).contains("module is off") && last_desc(&calls).contains("/module enable feeds"));
    h.done().await;
}

#[tokio::test]
async fn feed_list_test_edit_disable_enable_remove_and_autocomplete() {
    let Some(h) = harness(G_LIST, CH, Permissions::empty()).await else { return };
    h.t.install(super::registry());
    let calls = route(&h, admin("list", G_LIST)).await;
    assert!(last_desc(&calls).contains("No feeds yet"));
    let feed = h
        .feed(|n| {
            n.display_name = "My Blog".into();
            n.probe = seeded(&["l1"]);
            n.ping_role_id = Some(ROLE_OK.into());
        })
        .await;
    svc::schedule_feed_job(&h.t, G_LIST, &feed.id, Utc::now() + chrono::Duration::minutes(10)).await.expect("ok");
    let bad = h
        .feed(|n| {
            n.feed_type = FeedType::Youtube;
            n.source = "UCzzz".into();
            n.display_name = "Broken".into();
            n.fetch_url = "https://y.test".into();
        })
        .await;
    bot_db::UpdateBuilder::new(&bot_db::FEED_INFO).set("failureCount", json!(3)).and_then(|b| b.set("lastError", json!("HTTP 404 — YouTube"))).and_then(|b| b.filter("id", json!(bad.id))).and_then(|b| b.without_returning().build()).expect("sql").execute(h.t.db()).await.expect("update");
    let calls = route(&h, admin("list", G_LIST)).await;
    assert_eq!(last_title(&calls), "📰 Social Feeds");
    let d = last_desc(&calls);
    assert!(d.contains(&format!("`1.` 📰 **My Blog** → <#{CH}> · <@&{ROLE_OK}>\n↳ ✅ OK · next check <t:")), "{d}");
    assert!(d.contains("`2.` 📺 **Broken**") && d.contains("↳ ⚠️ Failing 3× — HTTP 404 — YouTube"), "{d}");
    assert_eq!(calls.last().map(|c| c.body["embeds"][0]["footer"]["text"].clone()), Some(json!(format!("2/{} feeds", bot_config::limits::FEEDS))));

    // Autocomplete.
    h.t.responses.clear();
    let ac = FakeInteraction::autocomplete("feed").guild(G_LIST).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::MANAGE_GUILD).subcommand("remove").focused("feed", "blog");
    bot_core::router::route_interaction(&h.t, ac.build()).await;
    let choices = h.t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(choices, vec![json!({ "name": "📰 My Blog → #news", "value": feed.id })]);

    // test: ephemeral, throttled, never writes the row.
    let before = h.row(&feed).await;
    StubFetcher::install(&h.t, |_, _| resp(200, &rss_xml(&[("l2", 5), ("l1", 60)]).replace("<title>l2</title>", "<title>Newest</title>"), &XML_HEADERS));
    let calls = route(&h, admin("test", G_LIST).option("feed", STRING, json!(feed.id))).await;
    assert!(calls[0].ephemeral());
    let last = calls.last().expect("answer");
    assert_eq!(last.body["embeds"][0]["title"], "Newest");
    assert!(last.content().unwrap_or("").starts_with(&format!("<@&{ROLE_OK}> ")));
    assert_eq!(last.body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(last.body["allowed_mentions"]["roles"], json!([]), "the preview pings nobody");
    assert!(last.body["embeds"][1]["description"].as_str().unwrap_or("").contains("Fetched 2 item(s) in"));
    assert_eq!(h.row(&feed).await.updated_at, before.updated_at, "test changes nothing");
    let calls = route(&h, admin("test", G_LIST).option("feed", STRING, json!(feed.id))).await;
    assert_eq!(last_title(&calls), "⚠️  Slow Down");
    StubFetcher::install(&h.t, |_, _| resp(500, "", &[]));
    let calls = route(&h, admin("test", G_LIST).option("feed", STRING, json!(bad.id))).await;
    assert_eq!(last_title(&calls), "❌  Fetch Failed");
    assert!(last_desc(&calls).contains("Nothing was changed"));
    let calls = route(&h, admin("test", G_LIST).option("feed", STRING, json!("missing"))).await;
    assert_eq!(last_title(&calls), "❌  Feed Not Found");

    // edit.
    let e = |extra: &[(&str, u8, Value)]| {
        let mut i = admin("edit", G_LIST).option("feed", STRING, json!(feed.id));
        for (n, k, v) in extra {
            i = i.option(n, *k, v.clone());
        }
        i
    };
    assert_eq!(last_title(&route(&h, e(&[])).await), "❌  Nothing to Change");
    assert_eq!(last_title(&route(&h, e(&[("ping_role", ROLE, json!(ROLE_OK)), ("clear_ping_role", BOOLEAN, json!(true))])).await), "❌  Conflicting Options");
    assert_eq!(last_title(&route(&h, e(&[("message", STRING, json!("x")), ("reset_message", BOOLEAN, json!(true))])).await), "❌  Conflicting Options");
    assert_eq!(last_title(&route(&h, e(&[("include_reposts", BOOLEAN, json!(true))])).await), "❌  Not a Bluesky Feed");
    assert_eq!(last_title(&route(&h, e(&[("message", STRING, json!("   "))])).await), "❌  Invalid Message");
    let other = "810000000000000077";
    let calls = route(&h, e(&[("channel", CHANNEL, json!(other)), ("message", STRING, json!("Hi {title}\\n{link}")), ("clear_ping_role", BOOLEAN, json!(true))])).await;
    assert_eq!(last_title(&calls), "✅  Feed Updated");
    assert!(last_desc(&calls).contains(&format!("→ <#{other}>")) && last_desc(&calls).contains("Ping: none"));
    let row = h.row(&feed).await;
    assert_eq!((row.channel_id.as_str(), row.ping_role_id.clone(), row.message_template.as_deref()), (other, None, Some("Hi {title}\n{link}")));
    route(&h, e(&[("reset_message", BOOLEAN, json!(true))])).await;
    assert_eq!(h.row(&feed).await.message_template, None);
    h.feed(|n| {
        n.channel_id = CH.into();
        n.source = row.source.clone();
        n.display_name = "twin".into();
    })
    .await;
    assert_eq!(last_title(&route(&h, e(&[("channel", CHANNEL, json!(CH))])).await), "❌  Already Following");

    // disable / enable / remove.
    let calls = route(&h, admin("disable", G_LIST).option("feed", STRING, json!(feed.id))).await;
    assert_eq!(last_title(&calls), "✅  Feed Paused");
    let row = h.row(&feed).await;
    assert_eq!((row.enabled, row.disabled_reason.as_deref()), (false, Some("Paused by an admin")));
    assert!(h.t.scheduler().get_job(&svc::feed_job_key(&feed.id)).await.is_none());
    let calls = route(&h, admin("disable", G_LIST).option("feed", STRING, json!(feed.id))).await;
    assert_eq!(last_title(&calls), "❌  Already Paused");
    assert_eq!(calls.len(), 1);
    let calls = route(&h, admin("enable", G_LIST).option("feed", STRING, json!(feed.id))).await;
    assert_eq!(last_title(&calls), "✅  Feed Enabled");
    assert!(last_desc(&calls).contains("Items published while it was off are skipped"));
    let row = h.row(&feed).await;
    assert_eq!((row.enabled, row.initialized, row.failure_count, row.last_error.clone(), row.disabled_reason.clone()), (true, false, 0, None, None));
    let d = h.t.scheduler().get_job(&svc::feed_job_key(&feed.id)).await.expect("job").run_at.timestamp_millis() - Utc::now().timestamp_millis();
    assert!((5_000..=15_000).contains(&d));
    let calls = route(&h, admin("enable", G_LIST).option("feed", STRING, json!(bad.id))).await;
    let row = h.row(&bad).await;
    assert_eq!((row.failure_count, row.last_error.clone()), (0, None));
    assert!(!last_desc(&calls).contains("skipped"));
    let calls = route(&h, admin("remove", G_LIST).option("feed", STRING, json!("my blog"))).await;
    assert_eq!(last_title(&calls), "✅  Feed Removed");
    assert!(svc::get_feed(h.t.db(), G_LIST, &feed.id).await.expect("q").is_none());
    assert!(h.t.scheduler().get_job(&svc::feed_job_key(&feed.id)).await.is_none());
    h.done().await;
}

// ── Cross-language ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_read_each_others_feeds_jobs_and_cache() {
    let Some(url) = database_url() else { return };
    let Some(redis) = bot_core::testing::redis_url() else { return };
    let g = "955130000000000020";
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;

    // JS writes, Rust reads.
    let input = json!({
        "type": "BLUESKY", "source": "did:plc:js", "displayName": "@js.bsky.social", "fetchUrl": "https://b.test/js", "channelId": CH,
        "pingRoleId": ROLE_OK, "messageTemplate": "{bluesky_name}: {title}", "includeReposts": true, "createdBy": "js",
        "probe": { "ok": true, "items": [{ "id": "at://x/1" }, { "id": "repost:at://x/2:2026-10-09T00:00:00.000Z" }], "etag": "\"e\"", "lastModified": "lm" },
    });
    let js = run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["feed-create", g, &input.to_string()]);
    assert!(js.get("error").is_none(), "{js}");
    let id = js["row"]["id"].as_str().unwrap_or("").to_string();
    let row = svc::find_feed_for_guild(t.db(), g, &id).await.expect("q").expect("row");
    assert_eq!(serde_json::to_value(&row).unwrap_or(Value::Null), js["row"], "the row reads the same through Prisma and Rust");
    assert_eq!(svc::find_feed_for_guild(t.db(), g, "@JS.bsky.social").await.expect("q").map(|f| f.id), Some(id.clone()));
    let job = t.scheduler().get_job(&svc::feed_job_key(&id)).await.expect("job");
    assert_eq!((job.r#type.as_str(), job.payload.clone(), job.max_attempts), ("feed.poll", json!({ "feedId": id }), 10));
    assert_eq!(js["job"]["dedupeKey"], svc::feed_job_key(&id));

    // A cache entry the JS wrote is a hit for Rust (no network), and the reverse.
    let feed_url = format!("https://cache.test/{}.xml", bot_db::new_cuid());
    let xml = rss_xml(&[("c1", 5), ("c2", 6)]);
    let w = run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["cache-write", &feed_url, "RSS", &xml]);
    assert_eq!(w["ok"], true, "{w}");
    let calls = StubFetcher::install(&t, |_, _| Err(feat_feeds::net::FetchFailure::Other("the network must not be used".into())));
    let svc::SourceResult::Ok { from_cache, items, title, .. } = svc::fetch_feed_source(&t, &network::feed_row(FeedType::Rss, &feed_url, false), true, true).await else {
        panic!("a cache hit")
    };
    assert!(from_cache);
    assert_eq!(calls.len(), 0);
    assert_eq!(title.as_deref(), Some("Example"));
    assert_eq!(serde_json::to_value(&items).unwrap_or(Value::Null), w["items"]);

    let rust_url = format!("https://cache.test/{}.xml", bot_db::new_cuid());
    let xml2 = rss_xml(&[("r1", 5)]);
    StubFetcher::install(&t, move |_, _| resp(200, &xml2, &XML_HEADERS));
    let svc::SourceResult::Ok { items: ours, .. } = svc::fetch_feed_source(&t, &network::feed_row(FeedType::Rss, &rust_url, false), false, true).await else { panic!("ok") };
    let back = run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["cache-read", &rust_url, "RSS"]);
    assert_eq!((back["ok"].clone(), back["fromCache"].clone()), (json!(true), json!(true)), "{back}");
    assert_eq!(back["items"], serde_json::to_value(&ours).unwrap_or(Value::Null));
    assert_eq!(back["title"], "Example");

    // Rust writes a feed and its job; the JS finds both.
    let mine = svc::create_feed_row(
        &t,
        svc::NewFeed {
            guild_id: g.into(),
            feed_type: FeedType::Reddit,
            source: "aww".into(),
            display_name: "r/Aww".into(),
            fetch_url: "https://www.reddit.com/r/Aww/new/.rss".into(),
            channel_id: CH.into(),
            ping_role_id: None,
            message_template: Some("{subreddit}".into()),
            include_reposts: false,
            created_by: "rust".into(),
            probe: seeded(&["t3_1", "t3_2"]),
        },
    )
    .await
    .expect("created");
    svc::schedule_feed_job(&t, g, &mine.id, Utc::now() + chrono::Duration::minutes(10)).await.expect("ok");
    let back = run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["feed-read", g, "r/aww"]);
    assert_eq!(back["row"], serde_json::to_value(&mine).unwrap_or(Value::Null), "{back}");
    assert_eq!(back["job"]["payload"], json!({ "feedId": mine.id }));
    assert_eq!((back["job"]["type"].clone(), back["job"]["maxAttempts"].clone(), back["job"]["status"].clone()), (json!("feed.poll"), json!(10), json!("PENDING")));

    // A Reddit block the JS set stops a Rust fetch.
    let until = bot_db::format_js_date(&(Utc::now() + chrono::Duration::minutes(5)));
    run_node("rust/fixtures/gen/feat-feeds-js.mjs", &["reddit-block", &until]);
    let blocked = svc::fetch_feed_source(&t, &mine, false, true).await;
    assert!(matches!(blocked, svc::SourceResult::Err { budget: true, .. }), "{blocked:?}");
    t.cache().del_one(svc::REDDIT_BLOCK_KEY).await;
    t.reset_guilds(&[g]).await;
}
