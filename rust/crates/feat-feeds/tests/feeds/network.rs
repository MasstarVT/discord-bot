//! The fetch paths through the fetch seam (no sockets): `fetchAndParse`, `fetchFeedSource` with the shared cache and the
//! Reddit budget, and `resolveFeedSource` for every type.

use super::*;
use bot_util::safe_fetch::FetchResponse;
use feat_feeds::net::FetchFailure;

pub(crate) const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><channel>
<title>Example &amp; Co</title><link>https://example.com/</link>
<item><title><![CDATA[Hello <b>World</b>]]></title><link>https://example.com/2</link><guid isPermaLink="false">post-2</guid>
<pubDate>Fri, 09 Oct 2026 10:00:00 GMT</pubDate><dc:creator>Ann</dc:creator></item>
<item><title>First</title><link>https://example.com/1</link><pubDate>Thu, 08 Oct 2026 10:00:00 GMT</pubDate></item>
</channel></rss>"#;
pub(crate) const YT: &str = r#"<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom"><title>Chan</title><entry><id>yt:video:abc123</id><title>Vid</title><link rel="alternate" href="https://www.youtube.com/watch?v=abc123"/><author><name>Chan</name></author><published>2026-10-09T12:00:00+00:00</published></entry></feed>"#;
pub(crate) const REDDIT: &str = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>newest submissions : aww</title><entry><author><name>/u/kit</name></author><id>t3_xyz</id><link href="https://www.reddit.com/r/aww/comments/xyz/cat/"/><published>2026-10-09T12:00:00+00:00</published><title>Cat</title></entry></feed>"#;
const POD: &str = r#"<rss><channel><title>Pod</title><itunes:image href="https://p.example/art.jpg"/><item><title>Ep 1</title><enclosure url="https://p.example/1.mp3" type="audio/mpeg"/><guid isPermaLink="false">ep-1</guid></item></channel></rss>"#;
const HTML_PAGE: &str = r#"<!DOCTYPE html><html><head><title>Blog</title><link rel="alternate" type="application/atom+xml" href="/atom.xml"></head><body>hello</body></html>"#;

pub(crate) fn bsky_json(at: &str) -> String {
    json!({ "feed": [
        { "post": { "uri": "at://did:plc:abc/app.bsky.feed.post/3k2", "author": { "did": "did:plc:abc", "handle": "alice.bsky.social", "displayName": "Alice" },
            "record": { "text": "Hello world" }, "indexedAt": at } },
        { "post": { "uri": "at://did:plc:zzz/app.bsky.feed.post/9x", "author": { "did": "did:plc:zzz", "handle": "bob.bsky.social", "displayName": "Bob" },
            "record": { "text": "Bob post" }, "indexedAt": "2026-10-08T00:00:00Z" },
          "reason": { "$type": "app.bsky.feed.defs#reasonRepost", "by": { "did": "did:plc:abc", "handle": "alice.bsky.social", "displayName": "Alice" }, "indexedAt": at } },
    ] })
    .to_string()
}

pub(crate) fn feed_row(t: FeedType, url: &str, initialized: bool) -> Feed {
    let mut f = service::feed_of(&json!({ "type": t.as_str(), "fetchUrl": url }));
    f.initialized = initialized;
    f.etag = Some("\"abc\"".into());
    f.last_modified = Some("Fri, 09 Oct 2026 10:00:00 GMT".into());
    f
}

#[tokio::test]
async fn fetch_and_parse_request_options_validators_and_window() {
    let t = TestApp::new();
    let calls = StubFetcher::install(&t, |_, _| resp(200, RSS, &XML_HEADERS));
    let r = svc::fetch_and_parse(&t, "https://feed.test/f.xml", FeedType::Rss, &[("if-none-match".into(), "\"old\"".into())]).await;
    let svc::FetchOutcome::Parsed { parsed, etag, last_modified, .. } = r else { panic!("parsed") };
    assert_eq!(parsed.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["post-2", "https://example.com/1"], "newest first");
    assert_eq!(etag.as_deref(), Some("\"abc\""));
    assert_eq!(last_modified.as_deref(), Some("Fri, 09 Oct 2026 10:00:00 GMT"));
    let (_, opts) = calls.call(0);
    assert_eq!((opts.timeout_ms, opts.max_bytes, opts.max_redirects), (10_000, 2 * 1_048_576, 5));
    assert_eq!(calls.header(0, "if-none-match").as_deref(), Some("\"old\""));
    assert!(calls.header(0, "user-agent").unwrap_or_default().contains("DiscordBot"));
    assert!(calls.header(0, "accept").unwrap_or_default().contains("rss+xml"));
    svc::fetch_and_parse(&t, "https://feed.test/p.xml", FeedType::Podcast, &[]).await;
    assert_eq!(calls.call(1).1.max_bytes, 10 * 1_048_576);
    svc::fetch_and_parse(&t, "https://api.test/x", FeedType::Bluesky, &[]).await;
    assert_eq!(calls.header(2, "accept").as_deref(), Some("application/json"));
}

async fn fail_of(t: &TestApp, r: Result<FetchResponse, FetchFailure>, ty: FeedType) -> svc::FetchError {
    StubFetcher::install(t, move |_, _| r.clone());
    match svc::fetch_and_parse(t, "https://feed.test/f.xml", ty, &[]).await {
        svc::FetchOutcome::Failed(e) => e,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_and_parse_failures() {
    let t = TestApp::new();
    StubFetcher::install(&t, |_, _| resp(304, "", &[]));
    let cond = svc::fetch_and_parse(&t, "https://feed.test/f.xml", FeedType::Rss, &[("if-none-match".into(), "\"v1\"".into())]).await;
    assert_eq!(cond, svc::FetchOutcome::NotModified);
    assert_eq!(fail_of(&t, resp(304, "", &[]), FeedType::Rss).await.code, "UNEXPECTED_304", "an unprompted 304 carries no feed");
    let e = fail_of(&t, resp(429, "slow down", &[("retry-after", "120")]), FeedType::Reddit).await;
    assert_eq!((e.code.as_str(), e.status, e.retry_after_ms), ("HTTP", Some(429), Some(120_000.0)));
    let e = fail_of(&t, resp(404, "", &[]), FeedType::Youtube).await;
    assert_eq!((e.status, e.retry_after_ms), (Some(404), None));
    assert_eq!(fail_of(&t, resp(400, r#"{"error":"InvalidRequest"}"#, &[]), FeedType::Bluesky).await.api_error.as_deref(), Some("InvalidRequest"));
    assert_eq!(fail_of(&t, safe_err(SafeFetchCode::TooLarge, None), FeedType::Rss).await.code, "TOO_LARGE");
    assert_eq!(fail_of(&t, safe_err(SafeFetchCode::Timeout, None), FeedType::Rss).await.code, "TIMEOUT");
    let e = fail_of(&t, safe_err(SafeFetchCode::HttpError, Some(502)), FeedType::Rss).await;
    assert_eq!((e.code.as_str(), e.status), ("HTTP", Some(502)));
    assert_eq!(fail_of(&t, Err(FetchFailure::Other("boom".into())), FeedType::Rss).await.code, "NETWORK");
    let e = fail_of(&t, resp_at(200, HTML_PAGE, &[("content-type", "text/html; charset=utf-8")], "https://blog.test/"), FeedType::Rss).await;
    assert_eq!(e.code, "HTML_PAGE");
    assert_eq!(e.links, vec!["https://blog.test/atom.xml".to_string()]);
    assert_eq!(fail_of(&t, resp(200, "<html><body>hi</body></html>", &[("content-type", "application/xml")]), FeedType::Rss).await.code, "HTML_PAGE");
    assert_eq!(fail_of(&t, resp(200, "<note>hi</note>", &[("content-type", "application/xml")]), FeedType::Rss).await.code, "NOT_A_FEED");
    assert_eq!(fail_of(&t, resp(200, r#"{"feed": 1}"#, &[]), FeedType::Bluesky).await.code, "NOT_A_FEED");
    assert_eq!(fail_of(&t, resp(200, "not json", &[]), FeedType::Bluesky).await.code, "BAD_JSON");
    let deep = format!("<rss><channel><title>t</title>{}{}</channel></rss>", "<x>".repeat(45), "</x>".repeat(45));
    assert_eq!(fail_of(&t, resp(200, &deep, &[]), FeedType::Rss).await.code, "INVALID_XML", "too deep is a handled failure");
}

#[tokio::test]
async fn the_default_fetcher_is_the_ssrf_safe_one() {
    let t = TestApp::new();
    for url in ["http://127.0.0.1:1/feed", "http://[::1]/feed", "http://169.254.169.254/latest", "http://localhost/feed"] {
        match svc::fetch_and_parse(&t, url, FeedType::Rss, &[]).await {
            svc::FetchOutcome::Failed(e) => assert_eq!(e.code, "BLOCKED_ADDRESS", "{url}"),
            other => panic!("{url}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn fetch_feed_source_conditional_headers_cache_and_reddit_budget() {
    let t = TestApp::new();
    let calls = StubFetcher::install(&t, |_, _| resp(200, RSS, &XML_HEADERS));
    let feed = feed_row(FeedType::Rss, "https://feed.test/f.xml", true);
    let svc::SourceResult::Ok { from_cache, items, etag, .. } = svc::fetch_feed_source(&t, &feed, true, true).await else { panic!("ok") };
    assert!(!from_cache);
    assert_eq!((items.len(), etag.as_deref()), (2, Some("\"abc\"")));
    assert_eq!(calls.header(0, "if-none-match").as_deref(), Some("\"abc\""));
    assert_eq!(calls.header(0, "if-modified-since").as_deref(), Some("Fri, 09 Oct 2026 10:00:00 GMT"));
    // The shared cache answers the second guild; dates come back as dates; no validators from the cache.
    let raw = t.cache().get(&svc::fetch_cache_key("https://feed.test/f.xml")).await.expect("cached");
    assert_eq!(raw["v"], 1);
    assert_eq!(raw["items"][0]["publishedAt"], "2026-10-09T10:00:00.000Z");
    assert!(!raw.to_string().contains("<rss"), "the raw body is never cached");
    let ttl = t.cache_backend.pttl(&svc::fetch_cache_key("https://feed.test/f.xml"));
    assert!(ttl > 869_000 && ttl <= 870_000, "RSS cache TTL is 870 s, got {ttl} ms");
    let svc::SourceResult::Ok { from_cache, items, etag, .. } = svc::fetch_feed_source(&t, &feed, true, true).await else { panic!("ok") };
    assert!(from_cache);
    assert_eq!(calls.len(), 1, "a second guild reuses the fetch");
    assert!(items[0].published_at.is_some());
    assert_eq!(etag, None);
    svc::fetch_feed_source(&t, &feed_row(FeedType::Rss, "https://feed.test/f.xml", false), false, true).await;
    assert_eq!(calls.header(1, "if-none-match"), None, "no validators before the first success");
    svc::fetch_feed_source(&t, &feed, false, false).await;
    assert_eq!(calls.header(2, "if-none-match"), None);
    StubFetcher::install(&t, |_, _| resp(304, "", &[]));
    assert!(matches!(svc::fetch_feed_source(&t, &feed, false, true).await, svc::SourceResult::Ok { not_modified: true, .. }));
    StubFetcher::install(&t, |_, _| resp(500, "", &[]));
    assert!(matches!(svc::fetch_feed_source(&t, &feed, false, true).await, svc::SourceResult::Err { error, budget: false } if error.status == Some(500)));

    // The bot-wide Reddit budget: 8 slot keys through the cache, then refused without touching the network.
    let reddit = feed_row(FeedType::Reddit, "https://www.reddit.com/r/aww/new/.rss", true);
    let calls = StubFetcher::install(&t, |_, _| resp(200, REDDIT, &XML_HEADERS));
    for _ in 0..svc::REDDIT_MAX_PER_MIN {
        assert!(matches!(svc::fetch_feed_source(&t, &reddit, false, true).await, svc::SourceResult::Ok { .. }));
    }
    assert!(t.cache_backend.raw("feed:rl:reddit:7").is_some());
    let over = svc::fetch_feed_source(&t, &reddit, false, true).await;
    assert!(matches!(&over, svc::SourceResult::Err { budget: true, .. }), "{over:?}");
    assert_eq!(calls.len(), svc::REDDIT_MAX_PER_MIN);
    assert!(matches!(svc::fetch_feed_source(&t, &reddit, true, true).await, svc::SourceResult::Ok { from_cache: true, .. }), "a cache hit costs no budget");
    t.clock.advance(61_000);
    assert!(matches!(svc::fetch_feed_source(&t, &reddit, false, true).await, svc::SourceResult::Ok { .. }), "a slot expiring frees a fetch");
}

#[tokio::test]
async fn the_in_process_reddit_budget_holds_while_redis_is_down() {
    let t = TestApp::new();
    t.cache_backend.set_available(false);
    svc::reset_local_state(&t);
    let calls = StubFetcher::install(&t, |_, _| resp(200, REDDIT, &XML_HEADERS));
    let reddit = feed_row(FeedType::Reddit, "https://www.reddit.com/r/aww/new/.rss", true);
    for _ in 0..svc::REDDIT_MAX_PER_MIN {
        assert!(matches!(svc::fetch_feed_source(&t, &reddit, true, true).await, svc::SourceResult::Ok { from_cache: false, .. }), "fail-open to the network");
    }
    assert!(matches!(svc::fetch_feed_source(&t, &reddit, true, true).await, svc::SourceResult::Err { budget: true, .. }));
    assert_eq!(calls.len(), svc::REDDIT_MAX_PER_MIN);
    let rss = feed_row(FeedType::Rss, "https://feed.test/f.xml", true);
    assert!(matches!(svc::fetch_feed_source(&t, &rss, true, true).await, svc::SourceResult::Ok { .. }), "other sources are unaffected");
    svc::reset_local_state(&t);
    assert!(matches!(svc::fetch_feed_source(&t, &reddit, true, true).await, svc::SourceResult::Ok { .. }));
}

#[tokio::test]
async fn a_reddit_429_block_stops_polls_adds_and_tests_without_a_request() {
    let t = TestApp::new();
    let calls = StubFetcher::install(&t, |_, _| resp(200, REDDIT, &XML_HEADERS));
    let until = bot_db::format_js_date(&(t.clock().now() + chrono::Duration::minutes(20)));
    t.cache().set(svc::REDDIT_BLOCK_KEY, &json!({ "until": until }), 600).await;
    let reddit = feed_row(FeedType::Reddit, "https://www.reddit.com/r/aww/new/.rss", true);
    assert!(matches!(svc::fetch_feed_source(&t, &reddit, false, true).await, svc::SourceResult::Err { budget: true, .. }));
    let added = svc::resolve_feed_source(&t, FeedType::Reddit, &svc::SourceInput::Name("aww".into())).await.expect("accepted");
    assert!(!added.probe.is_ok());
    assert_eq!(calls.len(), 0);
    let rss = feed_row(FeedType::Rss, "https://feed.test/f.xml", true);
    assert!(matches!(svc::fetch_feed_source(&t, &rss, true, true).await, svc::SourceResult::Ok { .. }));
    assert_eq!(calls.len(), 1);
}

#[tokio::test]
async fn resolve_rss_with_autodiscovery_and_refusals() {
    let t = TestApp::new();
    StubFetcher::install(&t, |url, _| match url {
        "https://blog.test/" => resp_at(200, HTML_PAGE, &[("content-type", "text/html")], url),
        "https://blog.test/atom.xml" => resp_at(200, YT, &XML_HEADERS, url),
        "https://plain.test/" => resp_at(200, "<html><body>no feeds</body></html>", &[("content-type", "text/html")], url),
        "https://loop.test/" => resp_at(200, r#"<html><head><link rel="alternate" type="application/rss+xml" href="/again"></head></html>"#, &[("content-type", "text/html")], url),
        "https://loop.test/again" => resp_at(200, "<html></html>", &[("content-type", "text/html")], url),
        "https://big.test/" => safe_err(SafeFetchCode::TooLarge, None),
        "https://bad.test/" => resp(200, "<note/>", &[]),
        "https://gone.test/" => resp(404, "", &[]),
        "https://notitle.test/rss" => resp(200, "<rss><channel><item><guid>1</guid></item></channel></rss>", &[]),
        "https://nl.test/" => resp(200, "<rss><channel><title>Line\nTwo</title></channel></rss>", &[]),
        _ => resp_at(200, RSS, &XML_HEADERS, url),
    });
    let rss = |u: &str| svc::SourceInput::Url(u.into());
    let r = svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://feed.test/f.xml")).await.expect("ok");
    assert_eq!((r.source.as_str(), r.fetch_url.as_str(), r.display_name.as_str()), ("https://feed.test/f.xml", "https://feed.test/f.xml", "Example & Co"));
    assert!(matches!(&r.probe, svc::Probe::Ok { items, etag, .. } if items.len() == 2 && etag.as_deref() == Some("\"abc\"")));
    let r = svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://blog.test/")).await.expect("ok");
    assert_eq!((r.fetch_url.as_str(), r.display_name.as_str()), ("https://blog.test/atom.xml", "Chan"), "one autodiscovery hop");
    let e = svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://plain.test/")).await.expect_err("refused");
    assert_eq!(e.title, "Not a Feed");
    assert!(e.message.contains("website, not an RSS/Atom feed"));
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://loop.test/")).await.expect_err("refused").title, "Not a Feed");
    let e = svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://big.test/")).await.expect_err("refused");
    assert!(e.title == "Feed Too Large" && e.message.contains("2 MB"));
    assert!(svc::resolve_feed_source(&t, FeedType::Podcast, &rss("https://big.test/")).await.expect_err("refused").message.contains("10 MB"));
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://bad.test/")).await.expect_err("refused").title, "Not a Feed");
    let e = svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://gone.test/")).await.expect_err("refused");
    assert!(e.title == "Could Not Fetch" && e.message.contains("HTTP 404"));
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://notitle.test/rss")).await.expect("ok").display_name, "notitle.test");
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Rss, &svc::SourceInput::Name("x".into())).await.expect_err("bad").title, "Unknown Feed Type");
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://nl.test/")).await.expect("ok").display_name, "Line Two", "stored as one line");
    StubFetcher::install(&t, |_, _| Err(FetchFailure::Other("kaboom".into())));
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Rss, &rss("https://x.test/")).await.expect_err("x").title, "Could Not Fetch");
}

#[tokio::test]
async fn resolve_youtube_reddit_bluesky_and_podcasts() {
    let t = TestApp::new();
    let id = format!("UC{}", "a".repeat(22));
    StubFetcher::install(&t, |_, _| resp(200, YT, &XML_HEADERS));
    let r = svc::resolve_feed_source(&t, FeedType::Youtube, &svc::SourceInput::ChannelId(id.clone())).await.expect("ok");
    assert_eq!((r.source.as_str(), r.display_name.as_str()), (id.as_str(), "Chan"));
    assert_eq!(r.fetch_url, format!("https://www.youtube.com/feeds/videos.xml?channel_id={id}"));
    assert!(r.warnings.is_empty());
    StubFetcher::install(&t, |_, _| resp(404, "nope", &[]));
    let r = svc::resolve_feed_source(&t, FeedType::Youtube, &svc::SourceInput::ChannelId(id.clone())).await.expect("a 404 at add time is accepted");
    assert_eq!(r.display_name, id);
    assert!(matches!(&r.probe, svc::Probe::Failed(e) if e.status == Some(404)));
    assert!(r.warnings[0].contains("unreliable since late 2025"));

    svc::reset_local_state(&t);
    StubFetcher::install(&t, |_, _| resp(200, REDDIT, &XML_HEADERS));
    let r = svc::resolve_feed_source(&t, FeedType::Reddit, &svc::SourceInput::Name("Aww".into())).await.expect("ok");
    assert_eq!((r.source.as_str(), r.display_name.as_str(), r.fetch_url.as_str()), ("aww", "r/Aww", "https://www.reddit.com/r/Aww/new/.rss"));
    StubFetcher::install(&t, |_, _| resp(404, "", &[]));
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Reddit, &svc::SourceInput::Name("nothere".into())).await.expect_err("404").title, "Subreddit Not Found");
    StubFetcher::install(&t, |_, _| resp(403, "blocked", &[]));
    let r = svc::resolve_feed_source(&t, FeedType::Reddit, &svc::SourceInput::Name("aww".into())).await.expect("a 403 is accepted");
    assert!(!r.probe.is_ok() && r.warnings[0].contains("best-effort"));
    StubFetcher::install(&t, |_, _| resp(429, "slow", &[("retry-after", "30")]));
    let r = svc::resolve_feed_source(&t, FeedType::Reddit, &svc::SourceInput::Name("aww".into())).await.expect("ok");
    assert!(matches!(&r.probe, svc::Probe::Failed(e) if e.status == Some(429)));
    assert!(t.cache().get(svc::REDDIT_BLOCK_KEY).await.is_some(), "a 429 blocks every Reddit feed");

    let did = "did:plc:abc123";
    let body = bsky_json("2026-10-09T12:00:01Z");
    let calls = StubFetcher::install(&t, move |url, _| {
        if url.contains("getProfile") {
            if url.contains("missing") {
                return resp(400, r#"{"error":"InvalidRequest"}"#, &[]);
            }
            if url.contains("weird") {
                return resp(200, r#"{"did":"not-a-did"}"#, &[]);
            }
            if url.contains("down") {
                return resp(502, "", &[]);
            }
            return resp(200, r#"{"did":"did:plc:abc123","handle":"alice.bsky.social"}"#, &[]);
        }
        resp(200, &body, &[])
    });
    let r = svc::resolve_feed_source(&t, FeedType::Bluesky, &svc::SourceInput::Actor("alice.bsky.social".into())).await.expect("ok");
    assert_eq!((r.source.as_str(), r.display_name.as_str()), (did, "@alice.bsky.social"));
    assert_eq!(r.fetch_url, format!("{}/xrpc/app.bsky.feed.getAuthorFeed?actor=did%3Aplc%3Aabc123&filter=posts_no_replies&limit=30", svc::bsky_appview()));
    assert!(matches!(&r.probe, svc::Probe::Ok { items, .. } if items.len() == 2));
    assert_eq!(calls.call(0).1.max_bytes, 256 * 1024);
    let actor = |a: &str| svc::SourceInput::Actor(a.into());
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Bluesky, &actor("missing.bsky.social")).await.expect_err("x").title, "Account Not Found");
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Bluesky, &actor("weird.bsky.social")).await.expect_err("x").title, "Bluesky Unavailable");
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Bluesky, &actor("down.bsky.social")).await.expect_err("x").title, "Bluesky Unavailable");
    StubFetcher::install(&t, |url, _| if url.contains("getProfile") { resp(200, r#"{"did":"did:plc:abc123","handle":"handle.invalid"}"#, &[]) } else { resp(503, "", &[]) });
    let r = svc::resolve_feed_source(&t, FeedType::Bluesky, &actor(did)).await.expect("ok");
    assert_eq!(r.display_name, format!("@{did}"));
    assert!(r.warnings[0].contains("couldn't load its posts yet"));

    let calls = StubFetcher::install(&t, |url, _| {
        if url.starts_with("https://itunes.apple.com/search") {
            if url.contains("nothing") {
                return resp(200, r#"{"results":[]}"#, &[]);
            }
            if url.contains("broken") {
                return resp(500, "", &[]);
            }
            return resp(200, r#"{"results":[{"collectionName":"The Pod","artistName":"Host","feedUrl":"https://pod.test/rss","artworkUrl600":"https://pod.test/a.jpg"}]}"#, &[]);
        }
        resp_at(200, POD, &XML_HEADERS, url)
    });
    let q = |s: &str| svc::SourceInput::Query(s.into());
    let r = svc::resolve_feed_source(&t, FeedType::Podcast, &q("the pod")).await.expect("ok");
    assert_eq!((r.display_name.as_str(), r.source.as_str()), ("The Pod", "https://pod.test/rss"));
    assert!(r.warnings[0].contains("Matched **The Pod** by Host"));
    assert_eq!(calls.call(0).0, "https://itunes.apple.com/search?media=podcast&entity=podcast&limit=5&term=the%20pod");
    assert_eq!((calls.call(0).1.max_bytes, calls.call(1).1.max_bytes), (512 * 1024, 10 * 1_048_576));
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Podcast, &q("nothing")).await.expect_err("x").title, "Podcast Not Found");
    assert_eq!(svc::resolve_feed_source(&t, FeedType::Podcast, &q("broken")).await.expect_err("x").title, "Podcast Search Failed");
    let r = svc::resolve_feed_source(&t, FeedType::Podcast, &svc::SourceInput::Url("https://pod.test/rss".into())).await.expect("ok");
    assert_eq!(r.display_name, "Pod");
    assert!(r.warnings.is_empty());
}

/// The production `SerenityRest` calls, through real serenity HTTP against bot-core's `FakeRest`: the create-message
/// body goes out as built (nonce, enforce_nonce, allowed_mentions), Discord errors keep their code and status, a missing
/// channel is 10003, and the history read asks for `after` / `limit` and reads bot, webhook and system messages.
#[tokio::test]
async fn serenity_rest_speaks_discord() {
    use bot_core::testing::FakeRest;
    use feat_feeds::net::{DiscordRest, SerenityRest};
    use serenity::all::{ChannelId, MessageId};

    let rest = FakeRest::new();
    let t = TestApp::new().with_rest(&rest);
    let app = t.app.clone();
    let ch = ChannelId::new(970100000000000001);

    let body = json!({ "content": "hi", "embeds": [], "components": [], "allowed_mentions": { "parse": ["users"], "roles": ["970100000000000009"] }, "nonce": "abc", "enforce_nonce": true });
    let id = SerenityRest.send_message(app.clone(), ch, body.clone()).await.expect("the echo answers");
    assert!(!id.is_empty());
    let sent = rest.calls_to("POST", "/channels/970100000000000001/messages");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body, body);

    rest.fail("POST", "/channels/970100000000000002/messages", 403, 50013, "Missing Permissions");
    let e = SerenityRest.send_message(app.clone(), ChannelId::new(970100000000000002), json!({ "content": "x" })).await.expect_err("refused");
    assert_eq!((e.code, e.status, e.timeout), (Some(50013), Some(403), false));
    assert!(!feat_feeds::reminder_service::is_transient_send_error(&e));

    rest.fail("POST", "/channels/970100000000000003/messages", 500, 0, "Internal Server Error");
    let e = SerenityRest.send_message(app.clone(), ChannelId::new(970100000000000003), json!({ "content": "x" })).await.expect_err("5xx");
    assert!(feat_feeds::reminder_service::is_transient_send_error(&e), "a 5xx is retried: {e:?}");

    let e = SerenityRest.fetch_channel(app.clone(), ChannelId::new(970100000000000004)).await.expect_err("unknown channel");
    assert_eq!((e.code, e.status), (Some(10003), Some(404)));

    rest.on(
        "GET",
        "/channels/970100000000000005",
        200,
        json!({ "id": "970100000000000005", "type": 0, "guild_id": "970100000000000000", "name": "news", "position": 0, "permission_overwrites": [] }),
    );
    let got = SerenityRest.fetch_channel(app.clone(), ChannelId::new(970100000000000005)).await.expect("found");
    assert_eq!(got.map(|c| c.name), Some("news".to_string()));

    let msg = |id: &str, kind: u8, bot: bool, webhook: bool| {
        let mut m = json!({ "id": id, "channel_id": "970100000000000006", "type": kind, "content": "", "timestamp": "2026-10-09T12:00:00.000000+00:00",
            "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
            "author": { "id": "970100000000000777", "username": "u", "discriminator": "0", "avatar": null, "bot": bot } });
        if webhook {
            m["webhook_id"] = json!("970100000000000888");
        }
        m
    };
    rest.on(
        "GET",
        "/channels/970100000000000006/messages",
        200,
        json!([msg("970100000000000011", 0, false, false), msg("970100000000000012", 0, true, false), msg("970100000000000013", 0, false, true), msg("970100000000000014", 7, false, false), msg("970100000000000015", 19, false, false)]),
    );
    let lites = SerenityRest.messages_after(app.clone(), ChannelId::new(970100000000000006), MessageId::new(970100000000000010), 50).await.expect("history");
    let got: Vec<(bool, bool)> = lites.iter().map(|m| (m.bot, m.system)).collect();
    assert_eq!(got, vec![(false, false), (true, false), (true, false), (false, true), (false, false)]);
    let call = rest.calls_to("GET", "/channels/970100000000000006/messages").pop().expect("one read");
    assert_eq!((call.query_param("after"), call.query_param("limit")), (Some("970100000000000010"), Some("50")));
}
