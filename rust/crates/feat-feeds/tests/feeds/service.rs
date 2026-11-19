//! The feed service: pure helpers against the JS fixtures, and the network paths through the fetch seam.

use super::*;

pub(crate) fn ftype(v: &Value) -> FeedType {
    FeedType::parse(v.as_str().unwrap_or("RSS")).unwrap_or(FeedType::Rss)
}

pub(crate) fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc))
}

pub(crate) fn item_of(v: &Value) -> FeedItem {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    FeedItem {
        id: s("id"),
        title: s("title"),
        url: v["url"].as_str().map(String::from),
        author: s("author"),
        published_at: date(&v["publishedAt"]),
        image: v["image"].as_str().map(String::from),
        summary: s("summary"),
        is_repost: v["isRepost"].as_bool().unwrap_or(false),
        actor_name: s("actorName"),
        actor_handle: s("actorHandle"),
    }
}

/// A Feed row for the pure helpers (the JS tests pass partial objects).
pub(crate) fn feed_of(v: &Value) -> Feed {
    let now = Utc::now();
    let s = |k: &str, d: &str| v[k].as_str().unwrap_or(d).to_string();
    Feed {
        id: s("id", "f1"),
        guild_id: s("guildId", "g"),
        r#type: ftype(&v["type"]),
        source: s("source", "src"),
        display_name: s("displayName", "Feed"),
        fetch_url: s("fetchUrl", "https://feed.test/f.xml"),
        channel_id: s("channelId", "c"),
        ping_role_id: v["pingRoleId"].as_str().map(String::from),
        message_template: v["messageTemplate"].as_str().map(String::from),
        include_reposts: v["includeReposts"].as_bool().unwrap_or(false),
        enabled: v["enabled"].as_bool().unwrap_or(true),
        initialized: v["initialized"].as_bool().unwrap_or(false),
        seen_ids: Vec::new(),
        etag: None,
        last_modified: None,
        last_checked_at: None,
        last_success_at: None,
        last_posted_at: None,
        failure_count: v["failureCount"].as_i64().unwrap_or(0) as i32,
        last_error: v["lastError"].as_str().map(String::from),
        disabled_reason: v["disabledReason"].as_str().map(String::from),
        created_by: "u".into(),
        created_at: now,
        updated_at: now,
    }
}

fn fetch_error_of(v: &Value) -> svc::FetchError {
    svc::FetchError {
        code: v["code"].as_str().unwrap_or("").into(),
        status: v["status"].as_u64().map(|s| s as u16),
        message: v["message"].as_str().unwrap_or("").into(),
        retry_after_ms: None,
        api_error: v["apiError"].as_str().map(String::from),
        links: Vec::new(),
    }
}

#[test]
fn keys_and_constants_match_the_js() {
    let g = golden("service.json");
    let k = &g["keys"];
    assert_eq!(svc::feed_job_key("clx0feed0001"), k["feedJobKey"]);
    for c in k["fetchCacheKey"].as_array().into_iter().flatten() {
        assert_eq!(svc::fetch_cache_key(c[0].as_str().unwrap_or("")), c[1]);
    }
    for t in svc::FEED_TYPES {
        assert_eq!(json!(svc::cache_ttl_seconds(t)), k["cacheTtlSeconds"][t.as_str()]);
        assert_eq!(svc::var_help(t), k["varHelp"][t.as_str()]);
    }
    assert_eq!(svc::REDDIT_BLOCK_KEY, k["redditBlockKey"]);
    assert_eq!(svc::ITUNES_RL_KEY, k["itunesKey"]);
    let slots: Vec<String> = (0..svc::REDDIT_MAX_PER_MIN).map(|i| format!("{}:{i}", svc::REDDIT_RL_KEY)).collect();
    assert_eq!(json!(slots), k["redditSlots"]);
    assert_eq!(svc::REDDIT_MAX_PER_MIN, 8);
    assert_eq!(svc::max_bytes(FeedType::Podcast), 10 * 1_048_576);
    assert_eq!(svc::max_bytes(FeedType::Rss), 2 * 1_048_576);
    assert_eq!(svc::FEED_CHANNEL_TYPES, [0, 5]);
}

#[test]
fn describe_fetch_error_matches_the_js() {
    let g = golden("service.json");
    for c in g["describe"].as_array().into_iter().flatten() {
        assert_eq!(svc::describe_fetch_error(ftype(&c[0]), &fetch_error_of(&c[1])), c[2].as_str().unwrap_or(""), "{c}");
    }
}

#[test]
fn template_warnings_and_message_option_match_the_js() {
    let g = golden("service.json");
    for c in g["templateWarnings"].as_array().into_iter().flatten() {
        assert_eq!(json!(svc::template_warnings(ftype(&c[0]), c[1].as_str())), c[2], "{c}");
    }
    for c in g["parseMessageOption"].as_array().into_iter().flatten() {
        let ours = match svc::parse_message_option(c[0].as_str()) {
            Ok(t) => json!({ "template": t }),
            Err(e) => json!({ "error": e, "template": null }),
        };
        assert_eq!(ours, c[1], "{c}");
    }
}

#[test]
fn next_poll_window_and_selection_match_the_js() {
    let g = golden("service.json");
    for c in g["computeNextPollAt"].as_array().into_iter().flatten() {
        let p = svc::NextPoll {
            feed_type: ftype(&c[0]),
            failure_count: c[1].as_i64().unwrap_or(0) as i32,
            status: c[2].as_u64().map(|s| s as u16),
            retry_after_ms: c[3].as_f64(),
            now: 1_791_590_400_000,
            rand: c[4].as_f64().unwrap_or(0.0),
        };
        assert_eq!(json!(svc::compute_next_poll_at(p).timestamp_millis()), c[5], "{c}");
    }
    for c in g["windowItems"].as_array().into_iter().flatten() {
        let items: Vec<FeedItem> = c[0].as_array().into_iter().flatten().map(item_of).collect();
        let ids: Vec<String> = svc::window_items(items).into_iter().map(|i| i.id).collect();
        assert_eq!(json!(ids), c[1]);
    }
    for c in g["selectNewItems"].as_array().into_iter().flatten() {
        let i = &c[0];
        let items: Vec<FeedItem> = i["items"].as_array().into_iter().flatten().map(item_of).collect();
        let seen: Vec<String> = i["seenIds"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect();
        let r = svc::select_new_items(svc::SelectInput {
            items: &items,
            seen_ids: &seen,
            initialized: i["initialized"].as_bool().unwrap_or(false),
            baseline: date(&i["baseline"]),
            include_reposts: i["includeReposts"].as_bool().unwrap_or(false),
            max_post: svc::MAX_POSTS_PER_POLL,
        });
        let ours = json!({
            "toPost": r.to_post.iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
            "nextSeenIds": r.next_seen_ids, "reseed": r.reseed, "skippedOld": r.skipped_old, "overflow": r.overflow,
        });
        assert_eq!(ours, c[1], "{}", c[0]);
    }
}

/// The parts of an embed the JS and serenity both write.
pub(crate) fn embed_core(e: &Value) -> Value {
    let ts = e["timestamp"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp_millis());
    json!({
        "title": e["title"], "description": e["description"], "url": e["url"], "color": e["color"], "timestamp": ts,
        "footer": e["footer"]["text"], "author": e["author"]["name"], "image": e["image"]["url"], "thumbnail": e["thumbnail"]["url"],
    })
}

#[tokio::test]
async fn post_payloads_match_the_js() {
    let t = TestApp::new();
    let g = golden("service.json");
    for c in g["posts"].as_array().into_iter().flatten() {
        let feed = feed_of(&c[0]);
        let item = item_of(&c[1]);
        let p = svc::build_post_payload(&t, &feed, &item, &svc::PostCtx { feed_image: Some("https://p.test/art.jpg"), ping: true, now: Utc::now(), ..Default::default() });
        let j = p.to_json();
        assert_eq!(j["content"].as_str(), c[2]["content"].as_str(), "content of {} / {}", c[0], c[1]["id"]);
        assert_eq!(j["allowed_mentions"], c[2]["allowedMentions"]);
        let ours: Vec<Value> = j["embeds"].as_array().into_iter().flatten().map(embed_core).collect();
        let theirs: Vec<Value> = c[2]["embeds"].as_array().into_iter().flatten().map(embed_core).collect();
        assert_eq!(ours, theirs, "embeds of {} / {}", c[0], c[1]["id"]);
    }
}

#[test]
fn display_names_status_links_and_retry_after_match_the_js() {
    let g = golden("service.json");
    for c in g["displayNameUpdate"].as_array().into_iter().flatten() {
        let items: Vec<FeedItem> = c[2]
            .as_array()
            .into_iter()
            .flatten()
            .map(|h| {
                let mut i = FeedItem::new("x", "x");
                i.actor_handle = h.as_str().unwrap_or("").into();
                i
            })
            .collect();
        assert_eq!(json!(svc::display_name_update(&feed_of(&c[0]), c[1].as_str(), &items)), c[3], "{c}");
    }
    for c in g["feedStatus"].as_array().into_iter().flatten() {
        let (icon, text) = svc::feed_status(&feed_of(&c[0]));
        assert_eq!(json!({ "icon": icon, "text": text }), c[1], "{c}");
    }
    for c in g["sourceLink"].as_array().into_iter().flatten() {
        assert_eq!(svc::source_link(ftype(&c[0]), "SRC", "https://f.test/feed"), c[1]);
    }
    for c in g["retryAfter"].as_array().into_iter().flatten() {
        let now = c[1].as_i64().unwrap_or(1_791_590_400_000);
        assert_eq!(svc::parse_retry_after_ms(c[0].as_str(), now), c[2].as_f64(), "{c}");
    }
    assert_eq!(svc::parse_retry_after_ms(None, 0), None);
}

#[tokio::test]
async fn third_party_text_cannot_form_masked_links_headings_or_mentions() {
    let feed = feed_of(&json!({ "type": "RSS", "displayName": "[Evil](https://evil.test)", "messageTemplate": "{title}\n{author}\n{feed}" }));
    let mut item = FeedItem::new("i1", "[Click here](https://evil.test) @everyone <@&42>");
    item.author = "# Big".into();
    let t = TestApp::new();
    let p = svc::build_post_payload(&t, &feed, &item, &svc::PostCtx { ping: true, now: Utc::now(), ..Default::default() });
    let content = p.content.unwrap_or_default();
    assert!(content.starts_with("\\[Click here](https://evil.test) @everyone <@&42>"), "{content}");
    assert!(content.contains("\n\\# Big\n"));
    assert!(content.contains("\\[Evil]"));
    assert_eq!(p.roles, Vec::<String>::new());
    let e = svc::disable_notice_embed(&[svc::DisabledFeed {
        feed_type: FeedType::Rss,
        display_name: "A_b".into(),
        channel_id: "1".into(),
        disabled_reason: "Its channel was deleted".into(),
    }]);
    let v = serde_json::to_value(e).unwrap_or(Value::Null);
    assert_eq!(v["title"], "⚠️  Feed Disabled");
    assert_eq!(v["description"], "📰 **A\\_b** — Its channel was deleted\nPick a new channel with `/feed edit channel:`, then run `/feed enable`.");
}

#[tokio::test]
async fn the_ping_role_is_allowed_only_when_the_bot_may_ping_it() {
    let g = "955030000000000101";
    let ch = "810000000000000101";
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[fake_guild(g, ch, Permissions::empty())]);
    let guild = bot_core::interaction_helpers::cached_guild(&t, serenity::all::GuildId::new(g.parse().unwrap_or(1))).expect("cached");
    let channel = bot_core::interaction_helpers::cached_channel(&t, guild.id, serenity::all::ChannelId::new(ch.parse().unwrap_or(1))).expect("channel");
    let mut feed = feed_of(&json!({ "type": "RSS", "displayName": "Example", "messageTemplate": "{title}" }));
    let item = FeedItem::new("i1", "Hello");
    let ctx = |ping| svc::PostCtx { guild: Some(&guild), channel: Some(&channel), ping, now: Utc::now(), ..Default::default() };
    feed.ping_role_id = Some(ROLE_OK.into());
    let p = svc::build_post_payload(&t, &feed, &item, &ctx(true));
    assert!(p.content.as_deref().unwrap_or("").starts_with(&format!("<@&{ROLE_OK}> ")));
    assert_eq!(p.roles, vec![ROLE_OK.to_string()]);
    assert!(svc::build_post_payload(&t, &feed, &item, &ctx(false)).roles.is_empty(), "a preview shows the role and pings nobody");
    feed.ping_role_id = Some(ROLE_NO.into());
    let p = svc::build_post_payload(&t, &feed, &item, &ctx(true));
    assert!(p.content.as_deref().unwrap_or("").starts_with(&format!("<@&{ROLE_NO}> ")));
    assert!(p.roles.is_empty(), "the bot lacks Mention Everyone, and the role is not mentionable");
    feed.ping_role_id = Some("99".into());
    let p = svc::build_post_payload(&t, &feed, &item, &ctx(true));
    assert!(!p.content.unwrap_or_default().contains("<@&"), "a deleted role gets no prefix");
}

#[test]
fn hostile_text_does_not_hang() {
    let t0 = std::time::Instant::now();
    p::strip_html(&"<".repeat(150_000));
    p::strip_html(&"<script>".repeat(30_000));
    p::strip_html(&" \n".repeat(80_000));
    p::discover_feed_links(&"<link".repeat(40_000), "https://a.test/");
    p::strip_doctype(&"<!DOCTYPE".repeat(30_000));
    p::looks_like_html(&format!("{}x", "<!---->".repeat(50_000)), "text/html");
    let token = format!("<link {}>", "a".repeat(150_000));
    p::discover_feed_links(&token, "https://a.test/");
    let feed = format!(
        "<rss><channel><title>t</title><item><title>{}</title><guid>1</guid><description>{}</description></item></channel></rss>",
        "&lt;".repeat(20),
        "&lt;script ".repeat(10_000)
    );
    p::parse_xml_feed(&feed).expect("parses");
    assert!(t0.elapsed().as_secs() < 20, "took {:?}", t0.elapsed());
}

/// `escapeThirdParty` is discord.js `escapeMarkdown(s, { maskedLink, heading, bulletedList, numberedList })`, code
/// blocks and inline code included (rust/fixtures/gen/feat-feeds-escape.mjs).
#[test]
fn escape_third_party_matches_discord_js() {
    let cases = golden("escape.json");
    assert!(cases.as_array().is_some_and(|a| a.len() > 300), "the fixture has its cases");
    let mut bad = Vec::new();
    for c in cases.as_array().expect("cases") {
        let input = c["input"].as_str().unwrap_or_default();
        let ours = svc::escape_third_party(input);
        if ours != c["output"].as_str().unwrap_or_default() {
            bad.push(format!("{input:?}: js {} rust {ours:?}", c["output"]));
        }
    }
    assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
}

/// Generated inputs through the user-input parsers, the item URL normaliser, stripHtml and oneLine, against the JS
/// (rust/fixtures/gen/feat-feeds-fuzz.mjs). Every mismatch is listed before the test fails.
#[test]
fn parsers_match_the_js_on_generated_inputs() {
    let g = golden("fuzz.json");
    assert!(g["url"].as_array().is_some_and(|a| a.len() > 600), "the fixture has its inputs");
    let mut bad: Vec<String> = Vec::new();
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    for c in g["url"].as_array().into_iter().flatten() {
        let ours = p::http_or_null(&s(&c[0])).map(Value::String).unwrap_or(Value::Null);
        if ours != c[1] {
            bad.push(format!("httpOrNull {:?}: js {} rust {}", c[0], c[1], ours));
        }
    }
    for c in g["normalizeFeedUrl"].as_array().into_iter().flatten() {
        let ours = match p::normalize_feed_url(&s(&c[0])) {
            Ok(url) => json!({ "ok": true, "url": url }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        if ours != c[1] {
            bad.push(format!("normalizeFeedUrl {:?}: js {} rust {}", c[0], c[1], ours));
        }
    }
    for c in g["youtube"].as_array().into_iter().flatten() {
        let ours = match p::parse_youtube_channel_input(&s(&c[0])) {
            Ok(id) => json!({ "ok": true, "channelId": id }),
            Err(r) => json!({ "ok": false, "handle": r.handle, "reason": r.reason }),
        };
        if ours != c[1] {
            bad.push(format!("youtube {:?}: js {} rust {}", c[0], c[1], ours));
        }
    }
    for c in g["subreddit"].as_array().into_iter().flatten() {
        let ours = match p::parse_subreddit_input(&s(&c[0])) {
            Ok(name) => json!({ "ok": true, "name": name }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        if ours != c[1] {
            bad.push(format!("subreddit {:?}: js {} rust {}", c[0], c[1], ours));
        }
    }
    for c in g["bluesky"].as_array().into_iter().flatten() {
        let ours = match p::parse_bluesky_actor_input(&s(&c[0])) {
            Ok(actor) => json!({ "ok": true, "actor": actor }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        if ours != c[1] {
            bad.push(format!("bluesky {:?}: js {} rust {}", c[0], c[1], ours));
        }
    }
    for c in g["stripHtml"].as_array().into_iter().flatten() {
        let ours = p::strip_html(&s(&c[0]));
        if ours != s(&c[1]) {
            bad.push(format!("stripHtml {:?}: js {:?} rust {ours:?}", c[0], c[1]));
        }
    }
    for c in g["oneLine"].as_array().into_iter().flatten() {
        let ours = p::one_line(&s(&c[0]));
        if ours != s(&c[1]) {
            bad.push(format!("oneLine {:?}: js {:?} rust {ours:?}", c[0], c[1]));
        }
    }
    for c in g["itemKey"].as_array().into_iter().flatten() {
        let ours = p::item_key(&s(&c[0]));
        if ours != s(&c[1]) {
            bad.push(format!("itemKey {:?}: js {:?} rust {ours:?}", c[0], c[1]));
        }
    }
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

/// `/feed test`'s `createThrottle(15_000)` runs on the app clock.
#[tokio::test]
async fn feed_test_throttle_runs_on_the_app_clock() {
    let t = TestApp::new();
    assert!(svc::feed_test_allowed(&t, "f1"));
    assert!(!svc::feed_test_allowed(&t, "f1"));
    assert!(svc::feed_test_allowed(&t, "f2"));
    t.clock.advance(15_001);
    assert!(svc::feed_test_allowed(&t, "f1"), "15 s of app time later");
}
