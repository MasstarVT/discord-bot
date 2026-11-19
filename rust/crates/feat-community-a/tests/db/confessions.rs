//! Confessions against the database (part of the `db` test binary): port of the behaviour tests of
//! `test/confessions.test.js` (create, submit, form, images, review buttons, delete, block, reveal, setup, panel,
//! hooks, anonymity), and the JS round trip.

use super::*;
use bot_db::ConfessionStatus;
use feat_community_a::confession_service as conf;

const AUTHOR: &str = "600000000000000011";

/// A 1x1 PNG.
fn png() -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
    b.extend([0x1f, 0x15, 0xc4, 0x89]);
    b
}

/// Confessions on, posting to channel 1; `review` adds channel 4 (private) as the review channel.
async fn conf_on(e: &Env, review: bool) {
    e.set(
        SettingsPatch::new()
            .set("confessionsEnabled", true)
            .set("confessionChannelId", e.ch(1))
            .set("confessionReviewChannelId", if review { json!(e.ch(4)) } else { Value::Null })
            .set("confessionAllowImages", false)
            .set("confessionBlockLinks", true)
            .set("confessionCooldownSeconds", 0)
            .set("modLogChannelId", e.ch(2)),
    )
    .await;
    let mod_role = MOD_ROLE.to_string();
    e.t.settings()
        .update_settings_with(e.g, move |_| Ok(SettingsPatch::new().set("modRoleIds", json!([mod_role])).set("confessionBlockedUserIds", json!([]))))
        .await
        .expect("roles");
    let png = png();
    conf::set_image_fetch(
        &e.t,
        Some(Arc::new(move |url: String| {
            let png = png.clone();
            Box::pin(async move { if url.contains("broken") { Err("FETCH_FAILED".to_string()) } else if url.contains("text") { Ok(b"hello".to_vec()) } else { Ok(png) } })
        })),
    );
}

async fn settings(e: &Env) -> bot_core::Settings {
    e.t.settings().get_settings(e.g).await.expect("settings")
}

async fn row(e: &Env, number: i64) -> Option<bot_db::Confession> {
    conf::find_by_number(e.t.db(), e.g, number).await.unwrap()
}

fn submit(e: &Env, user: &str, text: Option<&str>) -> FakeInteraction {
    let f = e.slash("confession", "submit", user);
    match text {
        Some(t) => f.option("text", 3, json!(t)),
        None => f,
    }
}

fn with_image(mut f: FakeInteraction, name: &str, url: &str, kind: &str, size: u64) -> serenity::all::Interaction {
    f = f.option("image", 11, json!("990000000000000001"));
    f.json_mut()["data"]["resolved"] = json!({ "attachments": { "990000000000000001": {
        "id": "990000000000000001", "filename": name, "size": size, "url": url, "proxy_url": url, "content_type": kind
    } } });
    f.build()
}

/// Stores the review message the bot sent (from the recorded call) where `fetch_message` finds it, with its image
/// as an attachment named like the upload, and returns its JSON.
fn store_message(e: &Env, call: &common::IoCall) -> Value {
    let attachments: Vec<Value> = call.body["files"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|n| json!({ "id": "990000000000000002", "filename": n, "size": 70, "url": "https://cdn.test/review.png", "proxy_url": "https://cdn.test/review.png" }))
        .collect();
    let mut embeds = call.body["embeds"].clone();
    if let Some(img) = embeds[0].get_mut("image")
        && !img.is_null()
    {
        *img = json!({ "url": "https://cdn.discordapp.com/attachments/1/2/confession.png?ex=signed" });
    }
    let m = json!({
        "id": call.message, "channel_id": call.channel,
        "author": { "id": TEST_BOT_ID, "username": "bot", "discriminator": "0", "avatar": null, "bot": true },
        "content": "", "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": attachments, "embeds": embeds, "pinned": false,
        "type": 0, "components": call.body["components"].clone()
    });
    let msg: serenity::all::Message = serde_json::from_value(m.clone()).expect("a message");
    e.io.messages.lock().unwrap().insert(call.message.clone().unwrap(), msg);
    m
}

/// A click on the review message's button, by `user` (MOD by default).
fn review_click(e: &Env, message: &Value, action: &str, user: &str) -> serenity::all::Interaction {
    let id = message["components"][0]["components"].as_array().unwrap().iter().find(|b| b["custom_id"].as_str().unwrap().ends_with(action)).unwrap()["custom_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut f = FakeInteraction::button(&id).guild(e.g).user(user).channel(message["channel_id"].as_str().unwrap());
    if user == MOD {
        f = f.roles(&[MOD_ROLE]);
    }
    f.json_mut()["message"] = message.clone();
    f.build()
}

/// The review message of the last pending submission.
fn last_review(e: &Env) -> Value {
    let call = e.io.of("send").into_iter().rev().find(|c| c.channel == e.ch(4)).expect("a review message");
    store_message(e, &call)
}

#[tokio::test]
async fn create_posts_at_once_or_holds_for_review() {
    let Some(e) = Env::new("933000000000000001").await else { return };
    conf_on(&e, false).await;
    let s = settings(&e).await;
    let a = conf::create_confession(&e.t, e.gid(), AUTHOR, "first", None, &s, None).await.unwrap();
    let b = conf::create_confession(&e.t, e.gid(), "600000000000000012", "second", None, &s, None).await.unwrap();
    assert_eq!((a.confession.number, b.confession.number, a.pending), (1, 2, false));
    assert_eq!(a.confession.status, ConfessionStatus::Approved);
    let sent = e.io.of("send");
    assert_eq!(sent[0].channel, e.ch(1));
    assert_eq!(sent[0].embed()["title"], "🤫 Confession #1");
    assert_eq!(sent[0].body["allowed_mentions"], json!({ "parse": [], "users": [], "roles": [] }));
    assert!(sent[0].body["components"].as_array().is_none_or(Vec::is_empty));
    assert_eq!(row(&e, 1).await.unwrap().message_id, sent[0].message);

    conf_on(&e, true).await;
    let c = conf::create_confession(&e.t, e.gid(), AUTHOR, "third", None, &*settings(&e).await, None).await.unwrap();
    assert!(c.pending);
    let r = row(&e, 3).await.unwrap();
    assert_eq!((r.status, r.message_id.clone(), r.review_message_id.is_some()), (ConfessionStatus::Pending, None, true));
    let review = e.io.of("send").last().cloned().unwrap();
    assert_eq!(review.channel, e.ch(4));
    assert_eq!(review.embed()["fields"][0]["value"], "⏳ Pending review");
    assert_eq!(review.body["components"][0]["components"][0]["custom_id"], format!("confession:{}:approve", r.id));
    for call in e.io.calls() {
        assert!(!call.body.to_string().contains(AUTHOR), "a message names the author: {}", call.body);
    }
    e.done().await;
}

#[tokio::test]
async fn create_cooldown_failed_send_and_unusable_channels() {
    let Some(e) = Env::new("933000000000000002").await else { return };
    conf_on(&e, false).await;
    e.set(SettingsPatch::new().set("confessionCooldownSeconds", 600)).await;
    let s = settings(&e).await;
    conf::create_confession(&e.t, e.gid(), AUTHOR, "one", None, &s, None).await.unwrap();
    assert!(matches!(conf::create_confession(&e.t, e.gid(), AUTHOR, "two", None, &s, None).await.unwrap_err(), conf::CreateError::Cooldown(_)));
    conf::create_confession(&e.t, e.gid(), "600000000000000012", "other member", None, &s, None).await.unwrap();
    // A failed send leaves no row and costs nothing.
    e.io.fail_with("send", Some(50013));
    assert_eq!(conf::create_confession(&e.t, e.gid(), "600000000000000013", "x", None, &s, None).await.unwrap_err(), conf::CreateError::Send("I'm missing permissions there.".into()));
    e.io.heal();
    assert_eq!(conf::create_confession(&e.t, e.gid(), "600000000000000013", "x", None, &s, None).await.unwrap().confession.number, 3);
    // Unusable channels.
    e.set(SettingsPatch::new().set("confessionChannelId", e.ch(3))).await;
    let err = conf::create_confession(&e.t, e.gid(), "600000000000000014", "x", None, &*settings(&e).await, None).await.unwrap_err();
    assert_eq!(err, conf::CreateError::Channel(format!("I need **Send Messages** in <#{}>.", e.ch(3))));
    e.set(SettingsPatch::new().set("confessionChannelId", e.ch(1)).set("confessionReviewChannelId", "899999999999999999")).await;
    let err = conf::create_confession(&e.t, e.gid(), "600000000000000014", "x", None, &*settings(&e).await, None).await.unwrap_err();
    assert_eq!(err, conf::CreateError::Channel("The review channel no longer exists.".into()));
    e.done().await;
}

/// Port of 'createConfession: ... a delete that races the save takes the post back': the row is claimed as DELETED
/// between the send and the save, so the guarded save changes nothing, the post is deleted again, the row is kept and
/// nothing gets a message id.
#[tokio::test]
async fn a_delete_that_races_the_save_takes_the_post_back() {
    let Some(e) = Env::new("933000000000000031").await else { return };
    conf_on(&e, false).await;
    let s = settings(&e).await;
    let (pool, g) = (e.t.db().clone(), e.g.to_string());
    *e.io.on_send.lock().unwrap() = Some(Arc::new(move || {
        let (pool, g) = (pool.clone(), g.clone());
        Box::pin(async move {
            sqlx::query(r#"UPDATE "Confession" SET "status" = 'DELETED' WHERE "guildId" = $1"#).bind(&g).execute(&pool).await.unwrap();
        })
    }));
    let err = conf::create_confession(&e.t, e.gid(), AUTHOR, "x", None, &s, None).await.unwrap_err();
    *e.io.on_send.lock().unwrap() = None;
    assert_eq!(err, conf::CreateError::DbError);
    let sent = e.io.of("send");
    let deleted = e.io.of("delete_message");
    assert_eq!(deleted.len(), 1, "the post is taken back");
    assert_eq!((deleted[0].channel.clone(), deleted[0].message.clone()), (sent[0].channel.clone(), sent[0].message.clone()));
    let r = row(&e, 1).await.expect("the claimed row stays");
    assert_eq!((r.status, r.message_id, r.review_message_id), (ConfessionStatus::Deleted, None, None));
    e.done().await;
}

#[tokio::test]
async fn uncached_confession_review_and_log_channels_are_fetched() {
    let Some(e) = Env::new("933000000000000021").await else { return };
    conf_on(&e, false).await;
    // The post and the review channel come from guild.channels.fetch when the cache lacks them.
    let (post, review, log) = ("833000000000000097", "833000000000000098", "833000000000000099");
    for id in [post, review, log] {
        e.rest.on("GET", &format!("/channels/{id}"), 200, channel_json(e.g, id));
    }
    e.set(SettingsPatch::new().set("confessionChannelId", post).set("confessionReviewChannelId", review).set("modLogChannelId", log)).await;
    let c = conf::create_confession(&e.t, e.gid(), AUTHOR, "fetched", None, &*settings(&e).await, None).await.expect("held for review");
    assert!(c.pending);
    assert_eq!(c.confession.channel_id.as_deref(), Some(post));
    assert_eq!(e.io.of("send")[0].channel, review);
    // The reveal writes to a log channel the cache lacks.
    e.io.clear();
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.io.of("send").iter().map(|c| c.channel.clone()).collect::<Vec<_>>(), vec![log.to_string()]);
    assert_eq!(e.last().embed_title(), Some("✅  Author Revealed"));
    e.done().await;
}

#[tokio::test]
async fn submit_answers_ephemerally_and_refuses_without_posting() {
    let Some(e) = Env::new("933000000000000003").await else { return };
    conf_on(&e, false).await;
    e.route(submit(&e, AUTHOR, Some("  a thing I did  ")).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral(), "the invocation never shows");
    assert_eq!(e.last().embed_title(), Some("✅  Confession Posted"));
    let r = row(&e, 1).await.unwrap();
    assert_eq!(r.content, "a thing I did");
    assert_eq!(
        e.last().embed_description(),
        Some(format!("Your confession is live as [#1](https://discord.com/channels/{}/{}/{}).\nYour name is not shown with it, and only you can see this message.", e.g, e.ch(1), r.message_id.unwrap()).as_str())
    );

    let refusals: Vec<(Option<String>, &str)> = vec![
        (Some("   ".into()), "❌  Empty Confession"),
        (Some("x".repeat(2001)), "❌  Too Long"),
        (Some("join discord.gg/abc".into()), "❌  Links Not Allowed"),
        (Some("see ｈｔｔｐｓ://example.com".into()), "❌  Links Not Allowed"),
    ];
    e.io.clear();
    for (text, title) in refusals {
        e.route(submit(&e, AUTHOR, text.as_deref()).build()).await;
        assert_eq!(e.last().embed_title(), Some(title));
    }
    assert!(e.io.calls().is_empty(), "nothing was posted");
    // Links are allowed when the setting is off.
    e.set(SettingsPatch::new().set("confessionBlockLinks", false)).await;
    e.route(submit(&e, AUTHOR, Some("see https://example.com")).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Confession Posted"));

    // The cooldown.
    e.set(SettingsPatch::new().set("confessionCooldownSeconds", 600)).await;
    e.route(submit(&e, AUTHOR, Some("again")).build()).await;
    assert_eq!(e.last().embed_title(), Some("⚠️  Slow Down"));
    // Blocked: the same neutral words, never a reason.
    let author = AUTHOR.to_string();
    e.t.settings().update_settings_with(e.g, move |_| Ok(SettingsPatch::new().set("confessionBlockedUserIds", json!([author])))).await.unwrap();
    e.route(submit(&e, AUTHOR, Some("x")).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Can't Confess"));
    assert_eq!(e.last().embed_description(), Some("You can't post confessions on this server."));
    // No channel; module off.
    e.set(SettingsPatch::new().set("confessionChannelId", Value::Null)).await;
    e.route(submit(&e, "600000000000000019", Some("x")).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  No Confession Channel"));
    e.set(SettingsPatch::new().set("confessionsEnabled", false)).await;
    e.route(submit(&e, "600000000000000019", Some("x")).build()).await;
    assert_eq!(e.last().embed_title(), Some("⚠️  Module Disabled"));
    e.done().await;
}

#[tokio::test]
async fn submit_with_a_review_channel_and_the_form() {
    let Some(e) = Env::new("933000000000000004").await else { return };
    conf_on(&e, true).await;
    e.route(submit(&e, AUTHOR, Some("held")).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Confession Sent For Review"));
    assert_eq!(
        e.last().embed_description(),
        Some(format!("Your confession was sent to the moderators for review as **#1**. It appears in <#{}> once it is approved.\nYour name is not shown with it, and only you can see this message.", e.ch(1)).as_str())
    );
    assert!(e.io.of("send").iter().all(|c| c.channel == e.ch(4)), "nothing public yet");

    // Without text: the form opens as the first response; the form posts it and checks everything again.
    e.route(submit(&e, AUTHOR, None).build()).await;
    assert_eq!(e.t.responses.names(), vec!["showModal"]);
    assert_eq!(e.last().body["custom_id"], "confession-modal");
    e.route(FakeInteraction::modal("confession-modal", &[("confession-text", "from the form")]).guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(e.last().embed_title(), Some("✅  Confession Sent For Review"));
    assert_eq!(row(&e, 2).await.unwrap().content, "from the form");
    e.route(FakeInteraction::modal("confession-modal", &[("confession-text", "discord.gg/x")]).guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Links Not Allowed"));
    // An image with no text is refused: the form cannot take images.
    e.route(with_image(submit(&e, AUTHOR, None), "a.png", "https://cdn.test/a.png", "image/png", 70)).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert_eq!(e.last().embed_title(), Some("❌  Add The Text"));

    // The panel button opens the form, or refuses ephemerally.
    e.route(FakeInteraction::button("confession:new").guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.t.responses.names(), vec!["showModal"]);
    let author = AUTHOR.to_string();
    e.t.settings().update_settings_with(e.g, move |_| Ok(SettingsPatch::new().set("confessionBlockedUserIds", json!([author])))).await.unwrap();
    e.route(FakeInteraction::button("confession:new").guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert!(e.last().ephemeral());
    assert_eq!(e.last().embed_title(), Some("❌  Can't Confess"));
    e.route(FakeInteraction::button("confession:what:ever").guild(e.g).user(AUTHOR).build()).await;
    assert_eq!(e.last().content(), Some("This button is no longer active."));
    e.done().await;
}

#[tokio::test]
async fn images_are_re_uploaded_under_a_neutral_name_and_copied_on_approve() {
    let Some(e) = Env::new("933000000000000005").await else { return };
    conf_on(&e, true).await;
    // Images not allowed.
    e.route(with_image(submit(&e, AUTHOR, Some("pic")), "IMG_alice_selfie.png", "https://cdn.test/a.png", "image/png", 70)).await;
    assert_eq!(e.last().embed_title(), Some("❌  Images Not Allowed"));
    e.set(SettingsPatch::new().set("confessionAllowImages", true)).await;
    for (name, url, kind, size, title) in [
        ("x.svg", "https://cdn.test/a.svg", "image/svg+xml", 70, "❌  Not An Image"),
        ("x.png", "https://cdn.test/a.png", "image/png", 8_000_001, "❌  Image Too Large"),
        ("x.png", "https://cdn.test/broken.png", "image/png", 70, "❌  Image Problem"),
        ("x.png", "https://cdn.test/text.png", "image/png", 70, "❌  Image Problem"),
    ] {
        e.route(with_image(submit(&e, AUTHOR, Some("pic")), name, url, kind, size)).await;
        assert_eq!(e.last().embed_title(), Some(title), "{url}");
    }
    assert!(e.io.calls().is_empty());
    e.route(with_image(submit(&e, AUTHOR, Some("with picture")), "IMG_alice_selfie.png", "https://cdn.test/a.png", "image/png", 70)).await;
    assert_eq!(e.last().embed_title(), Some("✅  Confession Sent For Review"));
    let review = e.io.of("send")[0].clone();
    assert_eq!(review.body["files"], json!(["confession.png"]));
    assert_eq!(review.embed()["image"]["url"], "attachment://confession.png");
    assert!(!review.body.to_string().contains("alice"), "the member's file name is never used");
    assert!(row(&e, 1).await.unwrap().has_image);

    // Approving copies the image from a fresh fetch of the review message.
    let msg = store_message(&e, &review);
    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert_eq!(e.last().embed_title(), Some("✅  Approved"));
    let post = e.io.of("send").into_iter().find(|c| c.channel == e.ch(1)).expect("the post");
    assert_eq!(post.body["files"], json!(["confession.png"]));
    assert_eq!(post.embed()["image"]["url"], "attachment://confession.png");
    // The review message keeps its image (by attachment name), loses its buttons and shows the reviewer.
    let edit = e.io.of("edit").last().cloned().unwrap();
    assert_eq!(edit.embed()["image"]["url"], "attachment://confession.png");
    assert_eq!(edit.body["components"], json!([]));
    assert!(edit.embed()["fields"].as_array().unwrap().last().unwrap()["value"].as_str().unwrap().starts_with(&format!("✅ Approved by <@{MOD}> <t:")));

    // The review message lost its image: posted without one, with a warning.
    e.route(with_image(submit(&e, AUTHOR, Some("two")), "x.png", "https://cdn.test/a.png", "image/png", 70)).await;
    let mut call = last_review_call(&e);
    call.body["files"] = json!([]);
    let msg = store_message(&e, &call);
    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert!(e.last().embed_description().unwrap().contains("⚠️ The review message no longer has its image, so it was posted without one."));
    e.done().await;
}

fn last_review_call(e: &Env) -> common::IoCall {
    e.io.of("send").into_iter().rev().find(|c| c.channel == e.ch(4)).expect("a review message")
}

#[tokio::test]
async fn review_buttons_approve_reject_and_refuse() {
    let Some(e) = Env::new("933000000000000006").await else { return };
    conf_on(&e, true).await;
    e.route(submit(&e, AUTHOR, Some("a thing I did")).build()).await;
    let msg = last_review(&e);
    // A member is refused and nothing changes.
    e.route(review_click(&e, &msg, "approve", "600000000000000055")).await;
    assert_eq!(e.t.responses.names(), vec!["reply"]);
    assert!(e.last().ephemeral());
    assert_eq!(e.last().embed_title(), Some("❌  Insufficient Permissions"));
    assert_eq!(row(&e, 1).await.unwrap().status, ConfessionStatus::Pending);

    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral());
    let r = row(&e, 1).await.unwrap();
    let post = e.io.of("send").into_iter().find(|c| c.channel == e.ch(1)).unwrap();
    assert_eq!(post.embed()["title"], "🤫 Confession #1");
    assert_eq!((r.status, r.reviewer_id.as_deref(), r.message_id.clone(), r.channel_id.clone()), (ConfessionStatus::Approved, Some(MOD), post.message.clone(), Some(e.ch(1))));
    assert!(r.reviewed_at.is_some());
    assert_eq!(e.last().embed_description(), Some(format!("Confession #1 is live ([jump](https://discord.com/channels/{}/{}/{})).", e.g, e.ch(1), post.message.unwrap()).as_str()));
    let edit = e.io.of("edit").last().cloned().unwrap();
    assert_eq!(edit.message.as_deref(), msg["id"].as_str());
    assert_eq!(edit.embed()["description"], "a thing I did");

    // A second moderator cannot post it twice; the stale message is settled.
    e.io.clear();
    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert_eq!(e.last().embed_title(), Some("ℹ️  Already Reviewed"));
    assert_eq!(e.last().embed_description(), Some("Confession #1 was already approved."));
    assert!(e.io.of("send").is_empty());
    assert_eq!(e.io.of("edit").len(), 1, "the live buttons were taken off");

    // Reject.
    e.route(submit(&e, AUTHOR, Some("not ok")).build()).await;
    let msg = last_review(&e);
    e.io.clear();
    e.route(review_click(&e, &msg, "reject", MOD)).await;
    assert_eq!(e.last().embed_title(), Some("✅  Rejected"));
    assert_eq!(e.last().embed_description(), Some("Confession #2 was rejected and will not be posted."));
    assert!(e.io.of("send").is_empty());
    let r = row(&e, 2).await.unwrap();
    assert_eq!((r.status, r.reviewer_id.as_deref(), r.message_id), (ConfessionStatus::Rejected, Some(MOD), None));
    let status = e.io.of("edit")[0].embed()["fields"].as_array().unwrap().last().unwrap()["value"].as_str().unwrap().to_string();
    assert!(status.starts_with(&format!("❌ Rejected by <@{MOD}>")), "{status}");

    // Two clicks at once post it once.
    e.route(submit(&e, AUTHOR, Some("race")).build()).await;
    let msg = last_review(&e);
    e.io.clear();
    let (a, b) = (review_click(&e, &msg, "approve", MOD), review_click(&e, &msg, "approve", MOD));
    tokio::join!(bot_core::router::route_interaction(&e.t, a), bot_core::router::route_interaction(&e.t, b));
    assert_eq!(e.io.of("send").len(), 1);
    e.done().await;
}

#[tokio::test]
async fn approve_failures_keep_it_pending() {
    let Some(e) = Env::new("933000000000000007").await else { return };
    conf_on(&e, true).await;
    e.route(submit(&e, AUTHOR, Some("one")).build()).await;
    let msg = last_review(&e);
    // No confession channel: stays pending.
    e.set(SettingsPatch::new().set("confessionChannelId", Value::Null)).await;
    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert_eq!(e.last().embed_title(), Some("❌  No Confession Channel"));
    assert_eq!(e.last().embed_description(), Some("Set one with `/confession setup` first. The confession stays pending."));
    assert_eq!(row(&e, 1).await.unwrap().status, ConfessionStatus::Pending);
    // A failing post hands it back to the queue.
    e.set(SettingsPatch::new().set("confessionChannelId", e.ch(1))).await;
    e.io.fail_with("send", Some(500));
    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert_eq!(e.last().embed_title(), Some("❌  Could Not Post"));
    assert_eq!(e.last().embed_description(), Some("I couldn't post it: Discord error 500. The confession stays pending."));
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.status, r.reviewer_id), (ConfessionStatus::Pending, None));
    e.io.heal();
    // Deleted meanwhile: the review buttons come off.
    let st = settings(&e).await;
    assert!(matches!(conf::delete_confession(&e.t, e.gid(), 1, Some(&*st)).await.unwrap(), conf::Deleted::Ok(_)));
    let closed = e.io.of("edit").last().cloned().expect("the review message was closed");
    assert!(closed.embed()["fields"].as_array().unwrap().last().unwrap()["value"].as_str().unwrap().starts_with("🗑️ Deleted <t:"));
    e.io.clear();
    e.route(review_click(&e, &msg, "approve", MOD)).await;
    assert_eq!(e.last().embed_description(), Some("Confession #1 was already deleted."));
    assert_eq!(e.io.of("edit").len(), 1, "the stale buttons of the clicked message are taken off");
    assert!(e.io.of("send").is_empty());
    e.done().await;
}

#[tokio::test]
async fn delete_block_and_reveal() {
    let Some(e) = Env::new("933000000000000008").await else { return };
    conf_on(&e, false).await;
    e.route(submit(&e, AUTHOR, Some("one")).build()).await;
    e.route(submit(&e, "600000000000000012", Some("two")).build()).await;
    let post = row(&e, 1).await.unwrap().message_id.unwrap();

    // delete: the post goes, the row keeps its data, the bot is named as the deleter.
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(e.last().embed_title(), Some("✅  Confession Deleted"));
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.status, r.message_id, r.content.as_str()), (ConfessionStatus::Deleted, None, "one"));
    assert_eq!(e.io.of("delete_message")[0].message.as_deref(), Some(post.as_str()));
    assert!(bot_core::deletion_tracker::was_deleted_by_bot(&e.t, &post).is_some());
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Already Deleted"));
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(9)).build()).await;
    assert_eq!(e.last().embed_description(), Some("This server has no confession **#9**."));
    // A post that cannot be deleted changes nothing; one already gone is fine.
    e.io.fail_with("delete_message", Some(50013));
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(2)).build()).await;
    assert_eq!(e.last().embed_description(), Some("I couldn't delete the post: I'm missing permissions there. Nothing was changed."));
    assert_eq!(row(&e, 2).await.unwrap().status, ConfessionStatus::Approved);
    e.io.fail_with("delete_message", Some(10008));
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(2)).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Confession Deleted"));
    e.io.heal();

    // block / unblock: the same words whatever the state was, never a name.
    for _ in 0..2 {
        e.route(e.slash("confession", "block", MOD).option("number", 4, json!(1)).build()).await;
        assert_eq!(e.last().embed_description(), Some("The author of confession **#1** can no longer post confessions. You do not see who they are."));
    }
    assert_eq!(settings(&e).await.confession_blocked_user_ids, vec![AUTHOR.to_string()]);
    e.route(submit(&e, AUTHOR, Some("blocked?")).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Can't Confess"));
    for _ in 0..2 {
        e.route(e.slash("confession", "unblock", MOD).option("number", 4, json!(1)).build()).await;
        assert_eq!(e.last().embed_description(), Some("The author of confession **#1** can post confessions again."));
    }
    assert!(settings(&e).await.confession_blocked_user_ids.is_empty());
    // The cap: a block is refused whoever the author is; an unblock still works.
    let full: Vec<String> = (0..bot_config::limits::CONFESSION_BLOCKED_USERS).map(|i| format!("7{i:017}")).collect();
    e.t.settings().update_settings_with(e.g, move |_| Ok(SettingsPatch::new().set("confessionBlockedUserIds", json!(full)))).await.unwrap();
    e.route(e.slash("confession", "block", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Blocklist Full"));
    assert_eq!(e.last().embed_description(), Some("Up to **500** members can be blocked. Unblock someone first."));
    e.route(e.slash("confession", "unblock", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Author Unblocked"));

    // reveal: admins only; the log first (naming the admin, never the author), then the author to the admin.
    e.route(e.slash("confession", "reveal", MOD).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.t.responses.names(), vec!["reply"], "moderators cannot reveal");
    e.io.clear();
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(1)).build()).await;
    let log = e.io.of("send");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].channel, e.ch(2));
    assert_eq!(log[0].embed()["title"], "🕵️ Confession author revealed");
    assert_eq!(log[0].embed()["description"], format!("<@{ADMIN}> looked up the author of **Confession #1**."));
    assert_eq!(log[0].embed()["footer"]["text"], format!("Admin ID: {ADMIN}"));
    assert!(!log[0].body.to_string().contains(AUTHOR));
    assert_eq!(e.last().embed_title(), Some("✅  Author Revealed"));
    assert_eq!(
        e.last().embed_description(),
        Some(format!("Confession **#1** was written by <@{AUTHOR}>.\nUser ID: `{AUTHOR}`\nThey are no longer in this server.\nThis lookup was written to the moderation log.").as_str())
    );
    // A member of the server is named.
    e.route(e.slash("confession", "submit", MOD).option("text", 3, json!("mod wrote this")).build()).await;
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(3)).build()).await;
    assert!(e.last().embed_description().unwrap().starts_with(&format!("Confession **#3** was written by <@{MOD}> (**user{MOD}**).")));
    // Without a usable log, nothing is revealed.
    e.set(SettingsPatch::new().set("modLogChannelId", e.ch(3))).await;
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(1)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Not Revealed"));
    assert!(e.last().embed_description().unwrap().contains(&format!("I need **Send Messages** in <#{}>. Set one with `/modconfig logchannel`", e.ch(3))));
    assert!(!e.last().body.to_string().contains(AUTHOR));
    e.set(SettingsPatch::new().set("modLogChannelId", Value::Null)).await;
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(1)).build()).await;
    assert!(e.last().embed_description().unwrap().contains("No moderation log channel is set."));
    e.set(SettingsPatch::new().set("modLogChannelId", e.ch(2))).await;
    e.io.fail_with("send", Some(50013));
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(1)).build()).await;
    assert!(e.last().embed_description().unwrap().ends_with("I couldn't write the log entry: I'm missing permissions there."));
    e.done().await;
}

#[tokio::test]
async fn confession_setup_and_panel() {
    let Some(e) = Env::new("933000000000000009").await else { return };
    e.route(e.slash("confession", "setup", ADMIN).build()).await;
    assert!(e.t.responses.calls()[0].ephemeral());
    assert_eq!(e.last().embed_title(), Some("🤫 Confessions"));
    let d = e.last().embed_description().unwrap().to_string();
    assert!(d.starts_with("Module: ❌ Off, turn it on with `/module enable confessions`\nChannel: Not set\nReview channel: None, confessions are posted at once\n"), "{d}");
    assert!(d.ends_with("Blocked members: not listed (so nobody can tell who they are or whether two confessions share an author)"));

    e.route(e.slash("confession", "setup", ADMIN).option("review_channel", 7, json!(e.ch(2))).option("no_review", 5, json!(true)).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Pick One"));
    e.route(e.slash("confession", "setup", ADMIN).option("channel", 7, json!(e.ch(6))).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Cannot Use That Channel"));
    e.route(e.slash("confession", "setup", ADMIN).option("channel", 7, json!(e.ch(1))).option("review_channel", 7, json!(e.ch(1))).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Same Channel"));
    // A public review channel is saved with a warning; a private one without.
    e.route(e.slash("confession", "setup", ADMIN).option("review_channel", 7, json!(e.ch(2))).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅ Confessions Updated"));
    let check = fields_of(&e.last().body);
    assert_eq!(check[0].0, "Check");
    assert_eq!(
        check[0].1,
        format!("⚠️ <#{}> can be seen by @everyone, so members would read confessions before they are approved. Make it private.\n⚠️ Pick the confession channel too, or approved confessions have nowhere to go.\n⚠️ Pick a channel with the `channel` option, or nobody can confess.", e.ch(2))
    );
    e.route(e.slash("confession", "setup", ADMIN).option("channel", 7, json!(e.ch(1))).option("review_channel", 7, json!(e.ch(4))).build()).await;
    assert!(fields_of(&e.last().body).is_empty(), "{:?}", fields_of(&e.last().body));
    let s = settings(&e).await;
    assert_eq!((s.confession_channel_id.clone(), s.confession_review_channel_id.clone()), (Some(e.ch(1)), Some(e.ch(4))));
    e.route(e.slash("confession", "setup", ADMIN).option("no_review", 5, json!(true)).build()).await;
    assert_eq!(settings(&e).await.confession_review_channel_id, None);

    // panel
    e.io.clear();
    e.route(e.slash("confession", "panel", ADMIN).option("channel", 7, json!(e.ch(2))).build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Panel Posted"));
    assert_eq!(e.last().embed_description(), Some(format!("The Confess button is in <#{}>.\n⚠️ The Confessions module is off. Turn it on with `/module enable confessions`.", e.ch(2)).as_str()));
    let panel = &e.io.of("send")[0];
    assert_eq!(panel.channel, e.ch(2));
    assert_eq!(panel.body["components"][0]["components"][0]["custom_id"], "confession:new");
    e.route(e.slash("confession", "panel", ADMIN).option("channel", 7, json!(e.ch(3))).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Cannot Use That Channel"));
    e.route(e.slash("confession", "panel", MOD).build()).await;
    assert_eq!(e.t.responses.names(), vec!["reply"], "moderators cannot post panels");
    e.done().await;
}

#[tokio::test]
async fn confession_hooks_clear_message_ids_and_channels() {
    let Some(e) = Env::new("933000000000000010").await else { return };
    conf_on(&e, true).await;
    e.route(submit(&e, AUTHOR, Some("pending")).build()).await;
    let pending = row(&e, 1).await.unwrap();
    let rmid = serenity::all::MessageId::new(pending.review_message_id.as_deref().unwrap().parse().unwrap());
    let del = |id: serenity::all::MessageId| MessageDelete { guild_id: e.gid(), message_id: id, channel_id: ChannelId::new(e.ch(4).parse().unwrap()), message: None, partial: true, channel: None };
    e.t.run_hooks(del(rmid)).await;
    let r = row(&e, 1).await.unwrap();
    assert_eq!((r.review_message_id, r.content.as_str(), r.status), (None, "pending", ConfessionStatus::Pending));

    // The confession channel deleted: unset, its posts lose their ids.
    conf_on(&e, false).await;
    e.route(submit(&e, AUTHOR, Some("posted")).build()).await;
    assert!(row(&e, 2).await.unwrap().message_id.is_some());
    let channel = |id: &str| bot_core::interaction_helpers::cached_channel(&e.t, e.gid(), ChannelId::new(id.parse().unwrap())).unwrap();
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: channel(&e.ch(1)) }).await;
    assert_eq!(settings(&e).await.confession_channel_id, None);
    assert!(row(&e, 2).await.unwrap().message_id.is_none());

    // The review channel deleted: BOTH channels unset, the queue loses its review messages.
    conf_on(&e, true).await;
    e.route(submit(&e, AUTHOR, Some("queued")).build()).await;
    assert!(row(&e, 3).await.unwrap().review_message_id.is_some());
    e.t.run_hooks(ChannelDelete { guild_id: e.gid(), channel: channel(&e.ch(4)) }).await;
    let s = settings(&e).await;
    assert_eq!((s.confession_channel_id.clone(), s.confession_review_channel_id.clone()), (None, None));
    assert!(row(&e, 3).await.unwrap().review_message_id.is_none());
    e.done().await;
}

/// The ANONYMITY sweep of the JS tests: every message the bot sent and every staff answer, over every flow, never
/// names the author (the reveal reply to the admin is the one exception).
#[tokio::test]
async fn anonymity_nothing_staff_can_read_names_the_author() {
    let Some(e) = Env::new("933000000000000011").await else { return };
    conf_on(&e, true).await;
    e.set(SettingsPatch::new().set("confessionAllowImages", true)).await;
    // Every log line of the process from here on (the sink is global; AUTHOR is used by confession tests only, and
    // none of them may print it either).
    let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let sink = lines.clone();
    bot_util::logger::set_sink(Some(Box::new(move |_, line: &str| sink.lock().unwrap().push(line.to_string()))));
    let mut staff: Vec<Value> = Vec::new();
    let mut keep = |e: &Env| staff.extend(e.t.responses.calls().into_iter().map(|c| c.body));
    e.route(with_image(submit(&e, AUTHOR, Some("one with a picture")), "IMG_alice_selfie.png", "https://cdn.test/a.png", "image/png", 70)).await;
    let m = last_review(&e);
    e.route(review_click(&e, &m, "approve", MOD)).await;
    keep(&e);
    e.route(submit(&e, AUTHOR, Some("two")).build()).await;
    let m = last_review(&e);
    e.route(review_click(&e, &m, "reject", MOD)).await;
    keep(&e);
    e.route(submit(&e, AUTHOR, Some("three")).build()).await;
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(3)).build()).await;
    keep(&e);
    e.route(FakeInteraction::modal("confession-modal", &[("confession-text", "four from the form")]).guild(e.g).user(AUTHOR).build()).await;
    let m = last_review(&e);
    e.route(review_click(&e, &m, "approve", MOD)).await;
    keep(&e);
    e.route(e.slash("confession", "delete", MOD).option("number", 4, json!(4)).build()).await;
    keep(&e);
    for (sub, n) in [("block", 1), ("block", 2), ("unblock", 2)] {
        e.route(e.slash("confession", sub, MOD).option("number", 4, json!(n)).build()).await;
        keep(&e);
    }
    e.route(e.slash("confession", "setup", ADMIN).build()).await;
    keep(&e);
    e.route(e.slash("confession", "panel", ADMIN).build()).await;
    keep(&e);
    e.route(e.slash("confession", "reveal", ADMIN).option("number", 4, json!(1)).build()).await;
    assert!(e.last().body.to_string().contains(AUTHOR), "sanity: the reveal does show the author to the admin");

    let visible = json!({ "io": e.io.calls().iter().map(|c| c.body.clone()).collect::<Vec<_>>(), "staff": staff }).to_string();
    for needle in [AUTHOR, "alice", "IMG_alice_selfie"] {
        assert!(!visible.contains(needle), "staff-visible output contains {needle}");
    }
    assert!(e.io.of("send").len() >= 7, "the sweep covered the flows");
    bot_util::logger::set_sink(None);
    let printed = lines.lock().unwrap().join("
");
    for needle in [AUTHOR, "one with a picture", "four from the form"] {
        assert!(!printed.contains(needle), "the process log contains {needle}");
    }
    assert!(printed.contains(&format!("revealed by {ADMIN}")), "the reveal is logged with the admin, not the author");
    e.done().await;
}

#[tokio::test]
async fn confession_raw_sql_runs() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    for (name, sql, binds) in conf::raw_sql() {
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
async fn js_and_rust_share_confessions_and_the_blocklist() {
    let Some(e) = Env::new("933000000000000020").await else { return };
    conf_on(&e, true).await;
    // JS creates a pending confession; Rust reads it and approves it through the button.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["conf-create", e.g, &e.ch(1), &e.ch(4), AUTHOR, "from JS"]);
    assert_eq!(js["out"]["ok"], true, "{js}");
    assert_eq!((js["posted"].clone(), js["reviewed"].clone()), (json!(0), json!(1)));
    let r = row(&e, 1).await.expect("the JS row");
    assert_eq!((r.status, r.content.as_str(), r.has_image), (ConfessionStatus::Pending, "from JS", false));
    assert_eq!(json!(r.review_message_id), js["out"]["confession"]["reviewMessageId"]);
    let id = format!("confession:{}:approve", r.id);
    let mut f = FakeInteraction::button(&id).guild(e.g).user(MOD).roles(&[MOD_ROLE]);
    f.json_mut()["message"]["id"] = json!(r.review_message_id.clone().unwrap());
    e.route(f.build()).await;
    assert_eq!(e.last().embed_title(), Some("✅  Approved"));
    // The JS reads the Rust approval.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["conf-read", e.g, "1"]);
    let ours = serde_json::to_value(row(&e, 1).await.unwrap()).unwrap();
    assert_eq!(ours, js["row"]);
    assert_eq!(js["row"]["status"], "APPROVED");

    // Rust creates; the JS blocks its author; Rust refuses the author; the JS deletes the Rust row.
    conf_on(&e, false).await;
    e.route(submit(&e, "600000000000000012", Some("from Rust")).build()).await;
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["conf-block", e.g, "2", "1"]);
    assert_eq!(js, json!({ "ok": true }));
    // The JS wrote the row (and its own cache); this process forgets its 30 s memo, as a restart would.
    e.t.settings().invalidate_settings(e.g).await;
    assert_eq!(settings(&e).await.confession_blocked_user_ids, vec!["600000000000000012".to_string()]);
    e.route(submit(&e, "600000000000000012", Some("again")).build()).await;
    assert_eq!(e.last().embed_title(), Some("❌  Can't Confess"));
    // Rust unblocks; the JS sees the list.
    assert_eq!(conf::set_blocked(&e.t, e.g, 2, false).await.unwrap(), conf::BlockResult::Ok);
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["conf-read", e.g, "2"]);
    assert_eq!(js["blocked"], json!([]));
    assert_eq!(js["row"]["content"], "from Rust");
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["conf-delete", e.g, "2"]);
    assert_eq!(js["ok"], true, "{js}");
    assert_eq!(row(&e, 2).await.unwrap().status, ConfessionStatus::Deleted);
    // The JS numbering continues after the Rust rows.
    let js = run_node("rust/fixtures/gen/feat-community-a-js.mjs", &["conf-create", e.g, &e.ch(1), "-", AUTHOR, "third"]);
    assert_eq!(js["out"]["confession"]["number"], 3);
    e.done().await;
}
