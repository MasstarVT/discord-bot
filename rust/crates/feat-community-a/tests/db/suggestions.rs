//! Suggestions against the database (part of the `db` test binary): port of the behaviour tests of
//! `test/suggestions.test.js`, the command through the router, and the JS round trip.

use super::*;
use bot_db::SuggestionStatus;
use feat_community_a::suggestion_service as sug;

const AUTHOR: &str = "600000000000000001";

async fn sugg_on(e: &Env) {
    e.set(
        SettingsPatch::new()
            .set("suggestionsEnabled", true)
            .set("suggestionChannelId", e.ch(1))
            .set("suggestionThreads", false)
            .set("suggestionAllowAnonymous", false)
            .set("suggestionDmOnUpdate", true)
            .set("suggestionCooldownSeconds", 0),
    )
    .await;
    let mod_role = MOD_ROLE.to_string();
    e.t.settings().update_settings_with(e.g, move |_| Ok(SettingsPatch::new().set("modRoleIds", json!([mod_role])))).await.expect("mod role");
}

async fn settings(e: &Env) -> bot_core::Settings {
    e.t.settings().get_settings(e.g).await.expect("settings")
}

async fn post(e: &Env, author: &str, text: &str) -> bot_db::Suggestion {
    let s = settings(e).await;
    sug::create_suggestion(&e.t, e.gid(), author, text, false, &s).await.expect("created").suggestion
}

async fn row(e: &Env, number: i64) -> Option<bot_db::Suggestion> {
    sug::find_by_number(e.t.db(), e.g, number).await.unwrap()
}

#[tokio::test]
async fn create_numbers_per_guild_posts_without_pings_and_opens_a_thread() {
    let Some(e) = Env::new("932000000000000001").await else { return };
    sugg_on(&e).await;
    let a = post(&e, AUTHOR, "  Add a music channel  ").await;
    let b = post(&e, "600000000000000002", "Second").await;
    assert_eq!((a.number, b.number), (1, 2));
    assert_eq!(a.content, "  Add a music channel  ", "the service stores what it is given; parseContent trims before");
    let sent = e.io.of("send");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].channel, e.ch(1));
    assert_eq!(sent[0].body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    assert_eq!(sent[0].embed()["title"], "💡 Suggestion #1");
    assert_eq!(sent[0].body["components"][0]["components"][0]["custom_id"], format!("suggestion:{}:up", a.id));
    assert_eq!(a.message_id.as_deref(), sent[0].message.as_deref());

    // Threads on: a thread named after the number; without Create Public Threads, a note instead.
    e.set(SettingsPatch::new().set("suggestionThreads", true)).await;
    let c = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "Third", false, &*settings(&e).await).await.unwrap();
    assert!(c.thread_note.is_none());
    let th = e.io.of("start_thread");
    assert_eq!(th[0].body["name"], "Suggestion #3");
    assert_eq!(c.suggestion.thread_id.as_deref(), th[0].body["id"].as_str());
    e.set(SettingsPatch::new().set("suggestionChannelId", e.ch(5))).await;
    let d = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "Fourth", false, &*settings(&e).await).await.unwrap();
    assert_eq!(d.thread_note.as_deref(), Some(format!("I couldn't open a discussion thread: I need **Create Public Threads** in <#{}>.", e.ch(5)).as_str()));
    e.set(SettingsPatch::new().set("suggestionChannelId", e.ch(1))).await;
    e.io.fail_with("start_thread", Some(50035));
    let f = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "Fifth", false, &*settings(&e).await).await.unwrap();
    assert_eq!(f.thread_note.as_deref(), Some("I couldn't open a discussion thread for it."));
    e.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_submissions_get_distinct_numbers() {
    let Some(e) = Env::new("932000000000000002").await else { return };
    sugg_on(&e).await;
    let s = settings(&e).await;
    let mut handles = Vec::new();
    for i in 0..8 {
        let (app, gid, s) = (e.t.app.clone(), e.gid(), s.clone());
        handles.push(tokio::spawn(async move { sug::create_suggestion(&app, gid, &format!("6100000000000000{i:02}"), "x", false, &s).await.map(|c| c.suggestion.number) }));
    }
    let mut numbers = Vec::new();
    for h in handles {
        numbers.push(h.await.unwrap().unwrap());
    }
    numbers.sort();
    assert_eq!(numbers, (1..=8).collect::<Vec<_>>());
    e.done().await;
}

#[tokio::test]
async fn create_refusals_cooldown_and_a_failed_send() {
    let Some(e) = Env::new("932000000000000003").await else { return };
    sugg_on(&e).await;
    e.set(SettingsPatch::new().set("suggestionCooldownSeconds", 3600)).await;
    post(&e, AUTHOR, "one").await;
    let again = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "two", false, &*settings(&e).await).await.unwrap_err();
    assert!(matches!(again, sug::CreateError::Cooldown(ms) if ms > 3_590_000 && ms <= 3_600_000), "{again:?}");
    post(&e, "600000000000000002", "another member").await;
    // The cooldown passes.
    sqlx::query(r#"UPDATE "Suggestion" SET "createdAt" = "createdAt" - interval '2 hours' WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    post(&e, AUTHOR, "after").await;
    assert!(sug::cooldown_left_ms(&e.t, e.g, AUTHOR, &*settings(&e).await).await.unwrap() > 0);

    e.set(SettingsPatch::new().set("suggestionCooldownSeconds", 0)).await;
    let s = settings(&e).await;
    assert_eq!(sug::create_suggestion(&e.t, e.gid(), AUTHOR, "x", true, &s).await.unwrap_err(), sug::CreateError::Anonymous);
    e.set(SettingsPatch::new().set("suggestionAllowAnonymous", true)).await;
    let anon = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "secret", true, &*settings(&e).await).await.unwrap().suggestion;
    assert!(anon.anonymous);
    assert!(!e.io.of("send").last().unwrap().body.to_string().contains(AUTHOR), "the post never names the author");

    e.set(SettingsPatch::new().set("suggestionChannelId", e.ch(3))).await;
    let err = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "x", false, &*settings(&e).await).await.unwrap_err();
    assert_eq!(err, sug::CreateError::Channel(format!("I need **Send Messages** in <#{}>.", e.ch(3))));
    e.set(SettingsPatch::new().set("suggestionChannelId", "899999999999999999")).await;
    let err = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "x", false, &*settings(&e).await).await.unwrap_err();
    assert_eq!(err, sug::CreateError::Channel("<#899999999999999999> no longer exists.".into()));
    e.set(SettingsPatch::new().set("suggestionChannelId", Value::Null)).await;
    assert_eq!(sug::create_suggestion(&e.t, e.gid(), AUTHOR, "x", false, &*settings(&e).await).await.unwrap_err(), sug::CreateError::NoChannel);

    // A failed send removes the row: it costs neither the cooldown nor a number.
    e.set(SettingsPatch::new().set("suggestionChannelId", e.ch(1)).set("suggestionCooldownSeconds", 3600)).await;
    e.io.fail_with("send", Some(50013));
    let err = sug::create_suggestion(&e.t, e.gid(), "600000000000000007", "x", false, &*settings(&e).await).await.unwrap_err();
    assert_eq!(err, sug::CreateError::Send("I'm missing permissions there.".into()));
    e.io.heal();
    let ok = post(&e, "600000000000000007", "now").await;
    assert_eq!(ok.number, 5);
    e.done().await;
}

#[tokio::test]
async fn an_uncached_suggestion_channel_is_fetched_before_it_counts_as_gone() {
    // guild.channels.cache.get(id) ?? guild.channels.fetch(id): a channel the cache lacks is not "no longer exists".
    let Some(e) = Env::new("932000000000000041").await else { return };
    sugg_on(&e).await;
    let fetched = "832000000000000099";
    e.rest.on("GET", &format!("/channels/{fetched}"), 200, channel_json(e.g, fetched));
    e.set(SettingsPatch::new().set("suggestionChannelId", fetched)).await;
    let c = sug::create_suggestion(&e.t, e.gid(), AUTHOR, "fetched", false, &*settings(&e).await).await.expect("posted in the fetched channel");
    assert_eq!(c.suggestion.channel_id, fetched);
    assert_eq!(e.io.of("send")[0].channel, fetched);
    assert_eq!(e.rest.calls_to("GET", &format!("/channels/{fetched}")).len(), 1, "fetched once, then cached");
    sug::create_suggestion(&e.t, e.gid(), AUTHOR, "again", false, &*settings(&e).await).await.unwrap();
    assert_eq!(e.rest.calls_to("GET", &format!("/channels/{fetched}")).len(), 1);
    e.done().await;
}

#[tokio::test]
async fn votes_toggle_switch_and_close() {
    let Some(e) = Env::new("932000000000000004").await else { return };
    sugg_on(&e).await;
    let s = post(&e, AUTHOR, "vote on me").await;
    let v = |u: &'static str, value: i32| sug::apply_vote(e.t.db(), &s.id, u, value);
    let voted = |r: sug::VoteResult, up: i64, down: i64| move |o: sug::VoteOutcome| matches!(o, sug::VoteOutcome::Voted { result, upvotes, downvotes, .. } if result == r && upvotes == up && downvotes == down);
    assert!(voted(sug::VoteResult::Added, 1, 0)(v("A", 1).await.unwrap()));
    assert!(voted(sug::VoteResult::Switched, 0, 1)(v("A", -1).await.unwrap()));
    assert!(voted(sug::VoteResult::Added, 1, 1)(v("B", 1).await.unwrap()));
    assert!(voted(sug::VoteResult::Removed, 1, 0)(v("A", -1).await.unwrap()));
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.upvotes, r.downvotes), (1, 0));
    for (status, open) in [("CONSIDERED", true), ("APPROVED", false), ("DENIED", false), ("IMPLEMENTED", false)] {
        sqlx::query(r#"UPDATE "Suggestion" SET "status" = $2::"SuggestionStatus" WHERE "id" = $1"#).bind(&s.id).bind(status).execute(e.t.db()).await.unwrap();
        let out = v("C", 1).await.unwrap();
        assert_eq!(matches!(out, sug::VoteOutcome::Closed(_)), !open, "{status}");
        if open {
            v("C", 1).await.unwrap();
        }
    }
    assert_eq!(sug::apply_vote(e.t.db(), "nope", "A", 1).await.unwrap(), sug::VoteOutcome::Gone);
    e.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_concurrent_votes_keeps_the_totals_consistent() {
    let Some(e) = Env::new("932000000000000005").await else { return };
    sugg_on(&e).await;
    let s = post(&e, AUTHOR, "burst").await;
    let mut handles = Vec::new();
    for i in 0..20 {
        let (pool, id) = (e.t.db().clone(), s.id.clone());
        handles.push(tokio::spawn(async move { sug::apply_vote(&pool, &id, &format!("U{}", i % 10), if i % 3 == 0 { -1 } else { 1 }).await }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let (up, down): (i64, i64) = sqlx::query_as(r#"SELECT COUNT(*) FILTER (WHERE "value" = 1), COUNT(*) FILTER (WHERE "value" = -1) FROM "SuggestionVote" WHERE "suggestionId" = $1"#)
        .bind(&s.id)
        .fetch_one(e.t.db())
        .await
        .unwrap();
    let r = row(&e, 1).await.unwrap();
    assert_eq!((i64::from(r.upvotes), i64::from(r.downvotes)), (up, down));
    e.done().await;
}

fn click(e: &Env, id: &str, user: &str) -> serenity::all::Interaction {
    FakeInteraction::button(id).guild(e.g).user(user).build()
}

#[tokio::test]
async fn the_vote_button_replies_in_words_and_renders_the_post() {
    let Some(e) = Env::new("932000000000000006").await else { return };
    sugg_on(&e).await;
    sug::set_render_interval(&e.t, Some(0));
    let s = post(&e, AUTHOR, "click").await;
    let up = format!("suggestion:{}:up", s.id);
    let down = format!("suggestion:{}:down", s.id);
    e.route(click(&e, &up, "600000000000000002")).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(e.last().content(), Some("👍 You upvoted suggestion #1."));
    e.route(click(&e, &down, "600000000000000002")).await;
    assert_eq!(e.last().content(), Some("🔁 Your vote on suggestion #1 changed to 👎."));
    e.route(click(&e, &down, "600000000000000002")).await;
    assert_eq!(e.last().content(), Some("🗑️ Your vote on suggestion #1 was removed."));
    // The renders run on their own tasks: wait for them (bounded) instead of a fixed sleep, which a loaded machine
    // (parallel tests on one database) can outlast.
    let mut edits = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        edits = e.io.of("edit");
        if !edits.is_empty() {
            break;
        }
    }
    assert!(!edits.is_empty());
    assert!(edits.iter().all(|c| c.message == s.message_id));

    sqlx::query(r#"UPDATE "Suggestion" SET "status" = 'DENIED' WHERE "id" = $1"#).bind(&s.id).execute(e.t.db()).await.unwrap();
    e.route(click(&e, &up, "600000000000000003")).await;
    assert_eq!(e.last().content(), Some("Voting on suggestion #1 is closed (Denied)."));
    // The post still shows live buttons (an edit was lost): the click asks for a render that disables them.
    // Let every render queued so far finish (the edit count stops moving) before counting the next one.
    let mut seen = usize::MAX;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let n = e.io.of("edit").len();
        if n == seen {
            break;
        }
        seen = n;
    }
    e.io.clear();
    let mut live = FakeInteraction::button(&up).guild(e.g).user("600000000000000003");
    live.json_mut()["message"]["components"] = json!([{ "type": 1, "components": [{ "type": 2, "style": 2, "custom_id": up, "label": "1", "disabled": false }] }]);
    e.route(live.build()).await;
    assert_eq!(e.last().content(), Some("Voting on suggestion #1 is closed (Denied)."));
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if !e.io.of("edit").is_empty() {
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let fixed = e.io.of("edit");
    assert_eq!(fixed.len(), 1, "one render");
    assert!(fixed[0].body["components"][0]["components"].as_array().unwrap().iter().all(|c| c["disabled"] == true));
    e.route(click(&e, "suggestion:whatever:sideways", "600000000000000003")).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert_eq!(e.last().content(), Some("This button is no longer active."));
    e.route(click(&e, "suggestion:missing:up", "600000000000000003")).await;
    assert_eq!(e.last().content(), Some("This suggestion no longer exists."));

    // The module off: the gate answers for the buttons.
    e.set(SettingsPatch::new().set("suggestionsEnabled", false)).await;
    e.route(click(&e, &up, "600000000000000003")).await;
    assert_eq!(e.last().content(), Some("The **Suggestions** module is turned off on this server."));
    e.done().await;
}

#[tokio::test]
async fn a_vote_burst_edits_the_post_a_few_times_and_the_last_edit_shows_the_final_counts() {
    let Some(e) = Env::new("932000000000000007").await else { return };
    sugg_on(&e).await;
    // A window longer than the 12 routed votes take even on a loaded machine (the debounce sleeps in real time).
    sug::set_render_interval(&e.t, Some(1000));
    let s = post(&e, AUTHOR, "burst").await;
    for i in 0..12 {
        e.route(click(&e, &format!("suggestion:{}:up", s.id), &format!("6200000000000000{i:02}"))).await;
    }
    // Wait (bounded) for the trailing render that shows the final count; a fixed sleep fails on a loaded machine.
    let mut edits = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        edits = e.io.of("edit");
        if edits.last().is_some_and(|c| c.embed()["fields"][1]["value"] == "👍 12  ·  👎 0") {
            break;
        }
    }
    assert!((1..=6).contains(&edits.len()), "{} edits for 12 votes", edits.len());
    let last = edits.last().unwrap();
    assert_eq!(last.embed()["fields"][1]["value"], "👍 12  ·  👎 0");
    assert_eq!(last.body["components"][0]["components"][0]["label"], "12");
    assert_eq!(last.message, s.message_id);
    assert_eq!(last.body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    e.done().await;
}

#[tokio::test]
async fn render_forgets_a_message_discord_no_longer_has_and_keeps_one_it_cannot_reach() {
    let Some(e) = Env::new("932000000000000008").await else { return };
    sugg_on(&e).await;
    let s = post(&e, AUTHOR, "render").await;
    e.io.fail_with("edit", Some(50013));
    assert_eq!(sug::render_now(&e.t, &s.id).await, sug::RenderResult::Skip);
    assert!(row(&e, 1).await.unwrap().message_id.is_some(), "kept");
    e.io.fail_with("edit", Some(10008));
    assert_eq!(sug::render_now(&e.t, &s.id).await, sug::RenderResult::Gone);
    assert!(row(&e, 1).await.unwrap().message_id.is_none(), "a purged post is forgotten");
    assert_eq!(sug::render_now(&e.t, &s.id).await, sug::RenderResult::Skip, "nothing to edit without a message");
    e.io.heal();
    let t = post(&e, AUTHOR, "render 2").await;
    e.io.fail_with("edit", Some(500));
    assert_eq!(sug::render_now(&e.t, &t.id).await, sug::RenderResult::Error, "unexpected errors are reported, never thrown");
    e.done().await;
}

#[tokio::test]
async fn review_saves_edits_the_post_and_dms_the_author() {
    let Some(e) = Env::new("932000000000000009").await else { return };
    sugg_on(&e).await;
    let s = post(&e, AUTHOR, "review me").await;
    let st = settings(&e).await;
    let out = sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Approved, Some("  Great idea  "), MOD, Some(&*st)).await.unwrap().unwrap();
    assert_eq!((out.previous, out.edited, out.dm), (SuggestionStatus::Open, true, sug::DmResult::Sent));
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.status, r.review_note.as_deref(), r.reviewer_id.as_deref()), (SuggestionStatus::Approved, Some("Great idea"), Some(MOD)));
    assert!(r.reviewed_at.is_some());
    let edit = e.io.of("edit").last().cloned().unwrap();
    assert_eq!(edit.embed()["color"], json!(sug::status_info(SuggestionStatus::Approved).color));
    assert_eq!(edit.embed()["fields"][2]["value"], format!("Great idea\n— <@{MOD}>"));
    assert!(edit.body["components"][0]["components"].as_array().unwrap().iter().all(|c| c["disabled"] == true));
    let dm = e.io.of("dm");
    assert_eq!(dm[0].channel, AUTHOR);
    assert_eq!(dm[0].embed()["title"], "✅ Your suggestion #1 was approved");
    assert_eq!(dm[0].body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    let _ = s;

    // Consider keeps voting open; a later review with no note clears it.
    sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Considered, Some("Thinking"), MOD, Some(&*st)).await.unwrap();
    assert!(e.io.of("edit").last().unwrap().body["components"][0]["components"].as_array().unwrap().iter().all(|c| c["disabled"] == false));
    sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Denied, Some(""), MOD, Some(&*st)).await.unwrap();
    assert_eq!(row(&e, 1).await.unwrap().review_note, None);
    assert_eq!(e.io.of("edit").last().unwrap().embed()["fields"][2]["value"], format!("*No note.*\n— <@{MOD}>"));

    // The DM is skipped when off or for the author's own review; a closed DM is "failed", never an error.
    e.set(SettingsPatch::new().set("suggestionDmOnUpdate", false)).await;
    let off = sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Considered, None, MOD, Some(&*settings(&e).await)).await.unwrap().unwrap();
    assert_eq!(off.dm, sug::DmResult::Off);
    let own = sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Considered, None, AUTHOR, Some(&*st)).await.unwrap().unwrap();
    assert_eq!(own.dm, sug::DmResult::Off);
    e.io.fail_with("dm", Some(50007));
    let failed = sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Implemented, None, MOD, Some(&*st)).await.unwrap().unwrap();
    assert_eq!(failed.dm, sug::DmResult::Failed);
    e.io.heal();

    // Unknown numbers; a deleted post still saves the status.
    assert!(sug::review_suggestion(&e.t, e.gid(), 99, SuggestionStatus::Approved, None, MOD, Some(&*st)).await.unwrap().is_none());
    sqlx::query(r#"UPDATE "Suggestion" SET "messageId" = NULL WHERE "guildId" = $1"#).bind(e.g).execute(e.t.db()).await.unwrap();
    e.io.clear();
    let out = sug::review_suggestion(&e.t, e.gid(), 1, SuggestionStatus::Approved, None, MOD, Some(&*st)).await.unwrap().unwrap();
    assert!(!out.edited);
    assert!(e.io.of("edit").is_empty());
    e.done().await;
}

#[tokio::test]
async fn delete_removes_the_post_thread_row_and_votes() {
    let Some(e) = Env::new("932000000000000010").await else { return };
    sugg_on(&e).await;
    e.set(SettingsPatch::new().set("suggestionThreads", true)).await;
    let s = post(&e, AUTHOR, "delete me").await;
    sug::apply_vote(e.t.db(), &s.id, "A", 1).await.unwrap();
    assert!(matches!(sug::delete_suggestion(&e.t, e.gid(), 42).await.unwrap(), sug::Deleted::NotFound));
    // A post Discord cannot delete: refused, nothing changed.
    e.io.fail_with("delete_message", Some(50001));
    assert!(matches!(sug::delete_suggestion(&e.t, e.gid(), 1).await.unwrap(), sug::Deleted::Message(r) if r == "I'm missing permissions there."));
    assert!(row(&e, 1).await.is_some());
    // Already gone is fine.
    e.io.fail_with("delete_message", Some(10008));
    assert!(matches!(sug::delete_suggestion(&e.t, e.gid(), 1).await.unwrap(), sug::Deleted::Ok(_)));
    assert!(row(&e, 1).await.is_none());
    let (votes,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM "SuggestionVote" WHERE "suggestionId" = $1"#).bind(&s.id).fetch_one(e.t.db()).await.unwrap();
    assert_eq!(votes, 0, "votes cascade");
    assert_eq!(e.io.of("delete_channel")[0].channel, s.thread_id.clone().unwrap());
    e.io.heal();
    let t = post(&e, AUTHOR, "again").await;
    assert!(matches!(sug::delete_suggestion(&e.t, e.gid(), i64::from(t.number)).await.unwrap(), sug::Deleted::Ok(_)));
    assert_eq!(e.io.of("delete_message")[0].message, t.message_id);
    assert!(bot_core::deletion_tracker::was_deleted_by_bot(&e.t, t.message_id.as_deref().unwrap()).is_some(), "the bot is named as the deleter");
    e.done().await;
}

#[tokio::test]
async fn suggestion_hooks_clear_message_ids_and_the_channel() {
    let Some(e) = Env::new("932000000000000011").await else { return };
    sugg_on(&e).await;
    let s = post(&e, AUTHOR, "hooks").await;
    let mid = serenity::all::MessageId::new(s.message_id.as_deref().unwrap().parse().unwrap());
    let del = |id: serenity::all::MessageId, partial: bool| MessageDelete { guild_id: e.gid(), message_id: id, channel_id: ChannelId::new(e.ch(1).parse().unwrap()), message: None, partial, channel: None };
    e.t.run_hooks(del(serenity::all::MessageId::new(1), true)).await;
    assert!(row(&e, 1).await.unwrap().message_id.is_some());
    // Not partial and not the bot's own message: the filter skips it.
    e.t.run_hooks(del(mid, false)).await;
    assert!(row(&e, 1).await.unwrap().message_id.is_some());
    e.t.run_hooks(del(mid, true)).await;
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.message_id, r.content.as_str(), r.number), (None, "hooks", 1));

    let t = post(&e, AUTHOR, "channel").await;
    assert!(t.message_id.is_some());
    let channel = |id: &str| bot_core::interaction_helpers::cached_channel(&e.t, e.gid(), ChannelId::new(id.parse().unwrap())).unwrap();
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: channel(&e.ch(2)) }).await;
    assert_eq!(settings(&e).await.suggestion_channel_id, Some(e.ch(1)));
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: channel(&e.ch(1)) }).await;
    assert_eq!(settings(&e).await.suggestion_channel_id, None);
    assert!(row(&e, 2).await.unwrap().message_id.is_none());
    e.done().await;
}

// ── The command ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn suggestion_submit_through_the_router() {
    let Some(e) = Env::new("932000000000000012").await else { return };
    sugg_on(&e).await;
    let submit = |text: Option<&str>| {
        let f = e.slash("suggestion", "submit", AUTHOR);
        match text {
            Some(t) => f.option("text", bot_core::command::option_type::STRING, json!(t)),
            None => f,
        }
        .build()
    };
    e.route(submit(Some("  Add a music channel  "))).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(e.last().embed_title(), Some("✅  Suggestion Posted"));
    let s = row(&e, 1).await.unwrap();
    assert_eq!(s.content, "Add a music channel");
    assert_eq!(e.last().embed_description(), Some(format!("Your suggestion is live as [#1](https://discord.com/channels/{}/{}/{}).", e.g, e.ch(1), s.message_id.unwrap()).as_str()));
    e.route(submit(Some(&"x".repeat(2001)))).await;
    assert_eq!(e.last().embed_title(), Some("❌  Too Long"));
    assert_eq!(e.last().embed_description(), Some("Suggestions can be at most **2000** characters."));
    e.route(submit(Some("   "))).await;
    assert_eq!(e.last().embed_title(), Some("❌  Empty Suggestion"));
    // Anonymous while not allowed.
    e.route(e.slash("suggestion", "submit", AUTHOR).option("text", 3, json!("x")).option("anonymous", 5, json!(true)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Anonymous Not Allowed"));

    // Without text: the form is the first response; the form posts it.
    e.route(submit(None)).await;
    assert_eq!(e.t.responses.names(), vec!["showModal"]);
    assert_eq!(e.last().body["custom_id"], "suggestion-modal:0");
    e.route(FakeInteraction::modal("suggestion-modal:0", &[("suggestion-text", "From the form")]).guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(e.last().embed_title(), Some("✅  Suggestion Posted"));
    assert_eq!(row(&e, 2).await.unwrap().content, "From the form");
    // The form re-checks: anonymous no longer allowed.
    e.route(FakeInteraction::modal("suggestion-modal:1", &[("suggestion-text", "x")]).guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Anonymous Not Allowed"));

    // The cooldown refuses, before the form opens as well.
    e.set(SettingsPatch::new().set("suggestionCooldownSeconds", 600)).await;
    e.route(submit(None)).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert!(e.last().ephemeral());
    assert_eq!(e.last().embed_title(), Some("⚠️  Slow Down"));
    e.route(submit(Some("again"))).await;
    assert_eq!(e.last().embed_title(), Some("⚠️  Slow Down"));

    // The module off: the router's wording, from the command itself (it is not gated).
    e.set(SettingsPatch::new().set("suggestionsEnabled", false)).await;
    e.route(submit(Some("x"))).await;
    assert_eq!(e.last().embed_title(), Some("⚠️  Module Disabled"));
    e.route(submit(None)).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert_eq!(e.last().embed_title(), Some("⚠️  Module Disabled"));
    // No channel.
    e.set(SettingsPatch::new().set("suggestionsEnabled", true).set("suggestionChannelId", Value::Null).set("suggestionCooldownSeconds", 0)).await;
    e.route(submit(Some("x"))).await;
    assert_eq!(e.last().embed_title(), Some("❌  No Suggestion Channel"));
    e.done().await;
}

#[tokio::test]
async fn suggestion_review_and_delete_commands() {
    let Some(e) = Env::new("932000000000000013").await else { return };
    sugg_on(&e).await;
    post(&e, AUTHOR, "staff").await;
    // A member cannot review.
    e.route(e.slash("suggestion", "approve", AUTHOR).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert_eq!(row(&e, 1).await.unwrap().status, SuggestionStatus::Open);

    e.route(e.slash("suggestion", "approve", MOD).option("number", 4, json!(1)).option("note", 3, json!("Soon")).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(e.last().embed_title(), Some("✅  Suggestion #1: Approved"));
    let s = row(&e, 1).await.unwrap();
    assert_eq!(
        e.last().embed_description(),
        Some(format!("Author: <@{AUTHOR}>\nNote: Soon\nThe post was updated ([jump](https://discord.com/channels/{}/{}/{})).\nThe author was sent a DM.", e.g, e.ch(1), s.message_id.clone().unwrap()).as_str())
    );
    e.route(e.slash("suggestion", "consider", MOD).option("number", 4, json!(7)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Suggestion Not Found"));
    assert_eq!(e.last().embed_description(), Some("This server has no suggestion **#7**."));

    // Staff commands work while the module is off, and say so.
    e.set(SettingsPatch::new().set("suggestionsEnabled", false)).await;
    e.route(e.slash("suggestion", "deny", MOD).option("number", 4, json!(1)).build()).await;
    assert!(e.last().embed_description().unwrap_or("").ends_with("⚠️ The Suggestions module is off. Turn it on with `/module enable suggestions`."));
    assert_eq!(row(&e, 1).await.unwrap().status, SuggestionStatus::Denied);

    e.route(e.slash("suggestion", "delete", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Suggestion Deleted"));
    assert_eq!(e.last().embed_description(), Some("Suggestion **#1** and its votes were deleted."));
    assert!(row(&e, 1).await.is_none());
    e.route(e.slash("suggestion", "delete", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Suggestion Not Found"));
    e.done().await;
}

#[tokio::test]
async fn suggestion_setup_saves_checks_and_shows() {
    let Some(e) = Env::new("932000000000000014").await else { return };
    e.route(e.slash("suggestion", "setup", ADMIN).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!e.t.responses.calls()[0].ephemeral(), "setup answers publicly");
    assert_eq!(e.last().embed_title(), Some("💡 Suggestions"));
    assert_eq!(
        e.last().embed_description(),
        Some("Module: ❌ Off, turn it on with `/module enable suggestions`\nChannel: Not set\nDiscussion threads: Yes\nAnonymous suggestions: No\nDM the author on review: Yes\nCooldown: 300 seconds")
    );
    e.route(e.slash("suggestion", "setup", ADMIN).option("threads", 5, json!(true)).option("cooldown", 4, json!(0)).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅ Suggestions Updated"));
    assert!(e.last().embed_description().unwrap().ends_with("Cooldown: none"));
    assert_eq!(fields_of(&e.last().body), vec![("Channel".into(), "⚠️ Pick a channel with the `channel` option, or nobody can post.".into())]);

    e.route(e.slash("suggestion", "setup", ADMIN).option("channel", 7, json!(e.ch(5))).option("anonymous", 5, json!(true)).option("cooldown", 4, json!(30)).build()).await;
    let f = fields_of(&e.last().body);
    assert_eq!(f, vec![("Threads".into(), format!("⚠️ I need **Create Public Threads** in <#{}> to open threads.", e.ch(5)))]);
    let s = settings(&e).await;
    assert_eq!((s.suggestion_channel_id.clone(), s.suggestion_allow_anonymous, s.suggestion_cooldown_seconds, s.suggestion_threads), (Some(e.ch(5)), true, 30, true));
    assert!(e.last().embed_description().unwrap().contains("Cooldown: 30 seconds"));

    e.route(e.slash("suggestion", "setup", ADMIN).option("channel", 7, json!(e.ch(3))).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Cannot Use That Channel"));
    assert_eq!(settings(&e).await.suggestion_channel_id, Some(e.ch(5)), "nothing saved");
    // A moderator cannot run setup.
    e.route(e.slash("suggestion", "setup", MOD).build()).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    e.done().await;
}

#[tokio::test]
async fn suggestion_raw_sql_runs() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    for (name, sql, binds) in sug::raw_sql() {
        let mut tx = pool.begin().await.unwrap();
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(b.as_str().unwrap().to_string());
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.unwrap();
    }
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_share_suggestions_and_votes() {
    let Some(e) = Env::new("932000000000000020").await else { return };
    sugg_on(&e).await;
    sug::set_render_impl(&e.t, Some(Arc::new(|_, _| Box::pin(async { Ok(sug::RenderResult::Ok) }))));
    // JS creates; Rust reads, votes and reviews.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["sugg-create", e.g, &e.ch(1), AUTHOR, "From JS", "1"]);
    assert_eq!(js["out"]["ok"], true, "{js}");
    let s = row(&e, 1).await.expect("the JS row");
    assert_eq!((s.content.as_str(), s.anonymous, s.status), ("From JS", true, SuggestionStatus::Open));
    assert_eq!(json!(s.message_id), js["out"]["suggestion"]["messageId"]);
    // The JS post and the Rust post are the same message.
    let author = sug::Author { name: "x".into(), icon_url: None };
    let ours = serde_json::to_value(sug::build_suggestion_payload(&s, Some(&author)).to_message()).unwrap();
    assert_eq!(common::norm(&ours["components"]), common::norm(&js["sent"][0]["components"]));
    assert_eq!(common::no_ts(ours["embeds"][0].clone())["title"], js["sent"][0]["embeds"][0]["title"]);
    sug::apply_vote(e.t.db(), &s.id, "600000000000000002", 1).await.unwrap();
    // The JS votes on the same row and sees the Rust vote.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["sugg-vote", &s.id, "600000000000000003", "-1"]);
    assert_eq!(js, json!({ "result": "added", "value": -1, "upvotes": 1, "downvotes": 1 }));
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.upvotes, r.downvotes), (1, 1));

    // Rust creates and reviews; the JS reads the same row.
    let mine = post(&e, "600000000000000004", "From Rust").await;
    sug::review_suggestion(&e.t, e.gid(), i64::from(mine.number), SuggestionStatus::Considered, Some("Looking"), MOD, None).await.unwrap();
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["sugg-read", e.g, "2"]);
    let ours = serde_json::to_value(row(&e, 2).await.unwrap()).unwrap();
    assert_eq!(ours, js["row"]);
    // A JS review of a Rust row, and a JS vote on it.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["sugg-review", e.g, "2", "APPROVED", "Done", MOD]);
    assert_eq!(js["ok"], true, "{js}");
    assert_eq!(row(&e, 2).await.unwrap().status, SuggestionStatus::Approved);
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["sugg-vote", &mine.id, "600000000000000005", "1"]);
    assert_eq!(js, json!({ "result": "closed", "status": "APPROVED" }));
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["sugg-read", e.g, "1"]);
    assert_eq!(js["votes"].as_array().map(Vec::len), Some(2));
    e.done().await;
}
