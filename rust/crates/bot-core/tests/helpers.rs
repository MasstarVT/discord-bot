//! interactionHelpers, the paginator flow, the confirm flow and the deletion tracker, offline (recorded responses,
//! the collector hub, the manual clock).

use std::time::Duration;

use bot_core::interaction::{CommandCtx, Responder};
use bot_core::interaction_helpers::{self as ih, Payload};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_core::testing::{FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp};
use serenity::all::{Interaction, Permissions};

const G: &str = "900000000000000300";

fn command_ctx(t: &TestApp, user: &str) -> CommandCtx {
    let Interaction::Command(ci) = FakeInteraction::slash("counting").guild(G).user(user).build() else { panic!("a command") };
    let responder = Responder::new(t.interaction_backend(), ci.id, ci.token.clone());
    CommandCtx::new(t.app.clone(), ci, responder, None)
}

#[tokio::test]
async fn safe_reply_follows_the_interaction_state() {
    let t = TestApp::new().with_guilds([G]);
    let ctx = command_ctx(&t, "600000000000000001");
    // Not acknowledged: reply (ephemeral as asked).
    ih::safe_reply(&ctx.responder, Payload::text("one").ephemeral(true)).await;
    // Replied: follow up.
    ih::safe_reply(&ctx.responder, "two").await;
    assert_eq!(t.responses.names(), vec!["reply", "followUp"]);
    assert!(t.responses.calls()[0].ephemeral());

    // Deferred with deferReply: edit the reply; a deleted original (10008) falls back to a follow-up.
    let ctx = command_ctx(&t, "600000000000000001");
    t.responses.clear();
    assert!(ih::safe_defer(&ctx.responder, false).await);
    assert!(ih::safe_defer(&ctx.responder, true).await, "already acknowledged is fine");
    t.responses.fail_next("editReply", 10008);
    ih::safe_reply(&ctx.responder, "three").await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply", "followUp"]);

    // deferUpdate leaves `ephemeral` null: the clicked panel is never overwritten.
    let ctx = command_ctx(&t, "600000000000000001");
    t.responses.clear();
    ctx.responder.defer_update().await.expect("ack");
    ih::safe_reply(&ctx.responder, "four").await;
    assert_eq!(t.responses.names(), vec!["deferUpdate", "followUp"]);

    // An expired interaction is swallowed.
    let ctx = command_ctx(&t, "600000000000000001");
    t.responses.set_failing(true);
    assert!(ih::safe_reply(&ctx.responder, "five").await.is_none());
    assert!(!ih::safe_defer(&command_ctx(&t, "600000000000000001").responder, false).await);
    t.responses.set_failing(false);
    assert!(ih::is_interaction_gone(&bot_core::interaction::InteractionError::Api { code: 10062, message: String::new() }));
    assert!(ih::is_interaction_gone(&bot_core::interaction::InteractionError::AlreadyReplied));
    assert!(!ih::is_interaction_gone(&bot_core::interaction::InteractionError::Api { code: 50013, message: String::new() }));
}

#[tokio::test]
async fn require_bot_permissions_names_what_is_missing() {
    let guild = FakeGuild::new(G, "600000000000000099")
        .everyone(Permissions::VIEW_CHANNEL)
        .member(TEST_BOT_ID, &[])
        .channel("700000000000000301", 0, "general", &[]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild]);
    let ctx = command_ctx(&t, "600000000000000001");
    let ch = serenity::all::ChannelId::new(700000000000000301);
    assert!(!ih::require_bot_permissions(&ctx, &["ViewChannel", "SendMessages", "EmbedLinks"], Some(ch), true).await);
    let call = &t.responses.calls()[0];
    assert_eq!(call.embed_title(), Some("❌  Missing Permissions"));
    assert_eq!(call.embed_description(), Some("I need **Send Messages, Embed Links** in <#700000000000000301> to do that."));
    assert!(call.ephemeral());
    let ctx = command_ctx(&t, "600000000000000001");
    assert!(!ih::require_bot_permissions(&ctx, &["ManageRoles"], None, true).await);
    assert_eq!(t.responses.calls()[1].embed_description(), Some("I need **Manage Roles** in this server to do that."));
    assert!(ih::require_bot_permissions(&command_ctx(&t, "600000000000000001"), &["ViewChannel"], Some(ch), true).await);
    // Unknown channel: every permission counts as missing (`bits == null`).
    assert_eq!(ih::missing_bot_permissions(&t, ih::PermTarget::Channel(serenity::all::GuildId::new(900000000000000300), serenity::all::ChannelId::new(5)), &["ViewChannel"]), vec!["ViewChannel"]);
}

#[tokio::test]
async fn await_confirm_answers_strangers_and_resolves_for_the_invoker() {
    let t = TestApp::new().with_guilds([G]);
    let ctx = command_ctx(&t, "600000000000000001");
    let app = t.app.clone();
    let task = tokio::spawn(async move { ih::await_confirm(&ctx, ih::ConfirmOptions { title: Some("Delete?".into()), ..Default::default() }).await });
    while app.collectors().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let prompt = t.responses.calls()[0].clone();
    assert_eq!(prompt.method, "reply");
    assert!(prompt.ephemeral());
    assert_eq!(prompt.embed_title(), Some("⚠️  Delete?"));
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or_default().to_string();
    assert!(yes.starts_with("confirm:") && yes.ends_with(":yes"));
    let message_id = "1000000000000000100"; // the reply as fetchReply returns it
    bot_core::router::route_interaction(&t, FakeInteraction::button(&yes).guild(G).user("600000000000000002").message_id(message_id).build()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stranger = t.responses.calls().last().cloned().expect("answered");
    assert_eq!(stranger.content(), Some("These buttons aren't for you."));
    bot_core::router::route_interaction(&t, FakeInteraction::button(&yes).guild(G).user("600000000000000001").message_id(message_id).build()).await;
    assert!(task.await.expect("joined"), "confirmed");
    let done = t.responses.calls().last().cloned().expect("update");
    assert_eq!(done.method, "update");
    assert_eq!(done.body["embeds"][0]["footer"]["text"], "Confirmed");
    assert_eq!(done.body["components"], serde_json::json!([]));
}

#[tokio::test(start_paused = true)]
async fn await_confirm_times_out_without_a_change() {
    let t = TestApp::new().with_guilds([G]);
    let ctx = command_ctx(&t, "600000000000000001");
    let result = ih::await_confirm(&ctx, ih::ConfirmOptions { timeout_ms: 1.0, ..Default::default() }).await;
    assert!(!result);
    let last = t.responses.calls().last().cloned().expect("edited");
    assert_eq!(last.method, "editReply");
    assert_eq!(last.body["embeds"][0]["footer"]["text"], "Timed out — nothing was changed.");
}

#[tokio::test]
async fn the_paginator_pages_for_the_invoker_only() {
    let t = TestApp::new().with_guilds([G]);
    let ctx = command_ctx(&t, "600000000000000001");
    let pages = vec![neutral_embed(Some("P1"), None, None), neutral_embed(Some("P2"), None, None), neutral_embed(Some("P3"), None, None)];
    paginate(&ctx, &pages, PaginateOptions::default()).await.expect("sent").expect("a message");
    assert_eq!(t.responses.names(), vec!["reply", "fetchReply"]);
    assert_eq!(t.responses.calls()[0].body["embeds"][0]["footer"]["text"], "Page 1 of 3");
    let click = |id: &str, user: &str| FakeInteraction::button(id).guild(G).user(user).message_id("1000000000000000100").build();
    while t.collectors().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    bot_core::router::route_interaction(&t, click("paginator:next", "600000000000000002")).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(t.responses.calls().last().and_then(|c| c.content().map(String::from)).as_deref(), Some("These buttons aren't for you."));
    bot_core::router::route_interaction(&t, click("paginator:next", "600000000000000001")).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let upd = t.responses.calls().last().cloned().expect("update");
    assert_eq!(upd.method, "update");
    assert_eq!(upd.body["embeds"][0]["footer"]["text"], "Page 2 of 3");
    bot_core::router::route_interaction(&t, click("paginator:stop", "600000000000000001")).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stop = t.responses.calls().last().cloned().expect("update");
    assert!(stop.body["components"][0]["components"].as_array().into_iter().flatten().all(|b| b["disabled"] == true), "every button disabled");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(t.collectors().is_empty(), "the collector ended");
}

#[tokio::test]
async fn respond_autocomplete_answers_once() {
    let t = TestApp::new().with_guilds([G]);
    let Interaction::Autocomplete(ci) = FakeInteraction::autocomplete("counting").guild(G).build() else { panic!("autocomplete") };
    let r = Responder::new(t.interaction_backend(), ci.id, ci.token.clone());
    ih::respond_autocomplete(&r, &[ih::Choice::new("A", "a"), ih::Choice::value(""), ih::Choice::value(5)]).await;
    ih::respond_autocomplete(&r, &[ih::Choice::new("B", "b")]).await;
    let calls = t.responses.calls();
    assert_eq!(calls.len(), 1, "a second answer is skipped");
    assert_eq!(calls[0].choices(), vec![serde_json::json!({ "name": "A", "value": "a" }), serde_json::json!({ "name": "5", "value": 5 })]);
}

#[tokio::test]
async fn the_deletion_tracker_marks_and_expires() {
    use bot_core::deletion_tracker::{mark_bot_deletion, was_deleted_by_bot};
    let t = TestApp::new();
    mark_bot_deletion(&t, &["1", "2"], Some("purge"), None);
    mark_bot_deletion(&t, &["3"], None, Some(5.0));
    assert_eq!(was_deleted_by_bot(&t, "1").as_deref(), Some("purge"));
    assert_eq!(was_deleted_by_bot(&t, "1").as_deref(), Some("purge"), "non-destructive");
    assert_eq!(was_deleted_by_bot(&t, "3").as_deref(), Some("bot"));
    t.clock.advance(10);
    assert_eq!(was_deleted_by_bot(&t, "3"), None, "a 5 ms mark expired");
    t.clock.advance(60_000);
    assert_eq!(was_deleted_by_bot(&t, "1"), None, "the default lifetime is 60 s");
    mark_bot_deletion(&t, &["4"], None, Some(f64::NAN));
    t.clock.advance(59_000);
    assert_eq!(was_deleted_by_bot(&t, "4").as_deref(), Some("bot"), "NaN falls back to 60 s");
    mark_bot_deletion(&t, &["5"], None, Some(10.0 * 3_600_000.0));
    t.clock.advance(3_600_001);
    assert_eq!(was_deleted_by_bot(&t, "5"), None, "capped at 1 h");
}

#[tokio::test]
async fn fetch_all_members_degrades_offline() {
    use bot_core::guild_members::{FetchFailure, fetch_all_members};
    let t = TestApp::new();
    assert_eq!(fetch_all_members(&t, serenity::all::GuildId::new(1), 1000, 10).await.err(), Some(FetchFailure::Error));
    let guild = FakeGuild::new(G, "1").member("600000000000000001", &[]).member("600000000000000002", &[]);
    let t = TestApp::new().with_fake_guilds(&[guild]);
    assert_eq!(fetch_all_members(&t, serenity::all::GuildId::new(900000000000000300), 1000, 1).await.err(), Some(FetchFailure::TooLarge));
    assert_eq!(bot_core::guild_members::retry_ms(Some(0.2)), 1000);
    assert_eq!(bot_core::guild_members::retry_ms(None), 30_000);
    assert_eq!(bot_core::guild_members::retry_ms(Some(10_000.0)), 600_000);
}
