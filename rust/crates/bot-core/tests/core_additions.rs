//! The core additions requested by the first wave of feature crates (`rust/docs/wave-a-core-requests.txt`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bot_core::guild_members::{FetchMembersError, MemberChunks, deliver_members_chunk, fetch_members_by_ids, set_chunk_sender};
use bot_core::hooks::{AutomodAction, HookState};
use bot_core::interaction::{AutocompleteCtx, CommandCtx, Responder};
use bot_core::interaction_helpers::{self as ih, FetchChannelError};
use bot_core::markdown::{EscapeOptions, escape_markdown, escape_markdown_with};
use bot_core::testing::{FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, load_golden, persistent_handler_problems, persistent_handler_problems_for};
use bot_core::{Registry, discord};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Interaction, Permissions, RoleId, UserId};

const G: &str = "880000000000000001";
const OWNER: &str = "880000000000000002";

fn gid() -> GuildId {
    GuildId::new(G.parse().unwrap())
}

fn command_ctx(t: &TestApp, fi: FakeInteraction) -> CommandCtx {
    let Interaction::Command(ci) = fi.build() else { panic!("a command") };
    let r = Responder::new(t.interaction_backend(), ci.id, ci.token.clone());
    CommandCtx::new(t.app.clone(), ci, r, None)
}

// ── FakeInteraction fills data.resolved ──────────────────────────────────────

#[tokio::test]
async fn options_fill_resolved_with_defaults() {
    let t = TestApp::new();
    let ctx = command_ctx(
        &t,
        FakeInteraction::slash("x")
            .guild(G)
            .option("user", 6, json!("880000000000000010"))
            .option("role", 8, json!("880000000000000020"))
            .option("channel", 7, json!("880000000000000030"))
            .option("who", 9, json!("880000000000000040")),
    );
    assert_eq!(ctx.get_user("user").unwrap().name, "user880000000000000010");
    assert!(ctx.get_member("user").is_some(), "a member in a guild");
    // Roles, channels and mentionables need a guild to resolve from (a made-up name would shadow the cache).
    assert!(ctx.get_role("role").is_none());
    assert!(ctx.get_channel("channel").is_none());
    assert_eq!(ctx.get_mentionable_id("who").map(|g| g.get()), Some(880000000000000040));
    // With a guild, unknown ids get defaults.
    let ctx = command_ctx(&t, FakeInteraction::slash("x").guild(G).resolve_from(&FakeGuild::new(G, OWNER)).option("role", 8, json!("880000000000000020")).option("channel", 7, json!("880000000000000030")));
    assert_eq!(ctx.get_role("role").unwrap().name, "role880000000000000020");
    assert_eq!(ctx.get_channel("channel").unwrap().name.as_deref(), Some("channel880000000000000030"));
}

#[tokio::test]
async fn options_resolve_from_a_fake_guild_and_keep_hand_written_entries() {
    let t = TestApp::new();
    let guild = FakeGuild::new(G, OWNER)
        .role_with("880000000000000020", 3, Permissions::empty(), |r| r["name"] = json!("Mods"))
        .member_with("880000000000000010", &["880000000000000020"], |m| m["nick"] = json!("Nick"))
        .channel("880000000000000030", 0, "general", &[]);
    let mut fi = FakeInteraction::slash("x")
        .guild(G)
        .resolve_from(&guild)
        .option("user", 6, json!("880000000000000010"))
        .option("role", 8, json!("880000000000000020"))
        .option("channel", 7, json!("880000000000000030"))
        .option("who", 9, json!("880000000000000020"))
        .option("other", 6, json!("880000000000000011"))
        .option("manual", 6, json!("880000000000000012"));
    fi.json_mut()["data"]["resolved"] = json!({ "users": { "880000000000000012": { "id": "880000000000000012", "username": "handmade", "discriminator": "0", "avatar": null } } });
    let ctx = command_ctx(&t, fi);
    assert_eq!(ctx.get_role("role").unwrap().name, "Mods");
    assert_eq!(ctx.get_member("user").unwrap().nick.as_deref(), Some("Nick"));
    assert_eq!(ctx.get_member("user").unwrap().roles, vec![RoleId::new(880000000000000020)]);
    assert_eq!(ctx.get_channel("channel").unwrap().name.as_deref(), Some("general"));
    // A mentionable that is a guild role resolves as a role.
    assert!(ctx.interaction.data.resolved.roles.contains_key(&RoleId::new(880000000000000020)));
    // A user who is not a member of the fake guild has no member entry.
    assert!(ctx.get_user("other").is_some());
    assert!(ctx.get_member("other").is_none());
    // A hand-written entry is left alone (and gets no member added).
    assert_eq!(ctx.get_user("manual").unwrap().name, "handmade");
    assert!(ctx.get_member("manual").is_none());
}

// ── AutocompleteCtx ──────────────────────────────────────────────────────────

#[tokio::test]
async fn autocomplete_reads_other_options_and_levels() {
    let t = TestApp::new();
    let fi = FakeInteraction::autocomplete("ticket-panel")
        .guild(G)
        .subcommand("edit")
        .option("panel", 3, json!("support"))
        .option("count", 4, json!(7))
        .option("flag", 5, json!(true))
        .option("chan", 7, json!("880000000000000030"))
        .focused("field", "ti");
    let Interaction::Autocomplete(ci) = fi.build() else { panic!("an autocomplete") };
    let r = Responder::new(t.interaction_backend(), ci.id, ci.token.clone());
    let ctx = AutocompleteCtx { app: t.app.clone(), interaction: ci, responder: r };
    assert_eq!(ctx.get_string("panel").as_deref(), Some("support"));
    assert_eq!(ctx.get_integer("count"), Some(7));
    assert_eq!(ctx.get_boolean("flag"), Some(true));
    assert_eq!(ctx.get_channel_id("chan"), Some(ChannelId::new(880000000000000030)));
    assert_eq!(ctx.get_string("field").as_deref(), Some("ti"), "the focused option's text");
    assert_eq!(ctx.get_string("missing"), None);
    assert!(ctx.ack_budget_ms() > 0);
    assert_eq!(ctx.permission_level().await, bot_config::PermissionLevel::Member);
}

// ── App ready time, channel nsfw, member roles, borrowed guild ───────────────

#[tokio::test]
async fn ready_timestamp_is_set_once() {
    let t = TestApp::new();
    assert_eq!(t.ready_at_ms(), None);
    assert!(t.set_ready_at_ms(1_000));
    assert!(!t.set_ready_at_ms(2_000));
    assert_eq!(t.ready_at_ms(), Some(1_000));
}

#[tokio::test]
async fn channel_nsfw_member_roles_and_borrowed_guild() {
    let guild = FakeGuild::new(G, OWNER)
        .role("880000000000000020", 1, Permissions::empty())
        .member(TEST_BOT_ID, &[])
        .member("880000000000000010", &["880000000000000099", "880000000000000020"])
        .channel("880000000000000030", 0, "adult", &[])
        .channel_nsfw("880000000000000030", true)
        .thread("880000000000000031", 11, "880000000000000030");
    let t = TestApp::new().with_fake_guilds(&[guild]);
    let in_channel = |c: &str| command_ctx(&t, FakeInteraction::slash("x").guild(G).channel(c));
    assert_eq!(in_channel("880000000000000030").channel_nsfw(), Some(true));
    assert_eq!(in_channel("880000000000000031").channel_nsfw(), Some(true), "a thread follows its parent");
    assert_eq!(in_channel("880000000000000039").channel_nsfw(), None, "uncached");

    let ids = ih::with_cached_guild(&t, gid(), |g| {
        let m = g.members.get(&UserId::new(880000000000000010)).unwrap();
        ih::member_role_ids(g, m)
    })
    .unwrap();
    assert_eq!(ids, vec![RoleId::new(G.parse().unwrap()), RoleId::new(880000000000000020)], "@everyone first, deleted roles dropped");
    assert!(ih::with_cached_guild(&t, GuildId::new(5), |_| ()).is_none());
}

// ── FakeGuild setters ────────────────────────────────────────────────────────

#[tokio::test]
async fn fake_guild_setters_reach_the_cache() {
    let guild = FakeGuild::new(G, OWNER)
        .name("Renamed")
        .premium_tier(2)
        .premium_count(9)
        .features(&["ROLE_ICONS"])
        .vanity_code("cool")
        .member(TEST_BOT_ID, &[])
        .bot_member("880000000000000050", &[])
        .channel("880000000000000060", 4, "cat", &[])
        .channel_with("880000000000000061", 2, "voice", &[], |c| c["bitrate"] = json!(64000))
        .channel_parent("880000000000000061", "880000000000000060")
        .channel_user_limit("880000000000000061", 5)
        .channel_bitrate("880000000000000061", 96000)
        .afk("880000000000000061", 900)
        .thread("880000000000000062", 11, "880000000000000060")
        .thread_owner("880000000000000062", "880000000000000050")
        .voice_state("880000000000000050", "880000000000000061")
        .presence("880000000000000050", "online", vec![json!({ "name": "Game", "type": 0 })]);
    let t = TestApp::new().with_fake_guilds(&[guild]);
    ih::with_cached_guild(&t, gid(), |g| {
        assert_eq!(g.name, "Renamed");
        assert_eq!(u8::from(g.premium_tier), 2);
        assert_eq!(g.premium_subscription_count, Some(9));
        assert_eq!(g.features, vec!["ROLE_ICONS".to_string()]);
        assert_eq!(g.vanity_url_code.as_deref(), Some("cool"));
        let afk = g.afk_metadata.as_ref().unwrap();
        assert_eq!(afk.afk_channel_id, ChannelId::new(880000000000000061));
        assert_eq!(serde_json::to_value(afk.afk_timeout).unwrap(), json!(900));
        assert!(g.members[&UserId::new(880000000000000050)].user.bot);
        let v = &g.channels[&ChannelId::new(880000000000000061)];
        assert_eq!(v.parent_id, Some(ChannelId::new(880000000000000060)));
        assert_eq!(v.user_limit, Some(5));
        assert_eq!(v.bitrate, Some(96000));
        assert_eq!(g.threads[0].owner_id, Some(UserId::new(880000000000000050)));
        assert_eq!(g.voice_states[&UserId::new(880000000000000050)].channel_id, Some(ChannelId::new(880000000000000061)));
        assert_eq!(g.presences[&UserId::new(880000000000000050)].activities[0].name, "Game");
    })
    .unwrap();
}

#[tokio::test]
async fn with_discord_guilds_knows_the_bot() {
    let t = TestApp::new().with_discord_guilds(&[bot_core::testing::guild_create_json(G, OWNER, &[], &[(TEST_BOT_ID, &[])])]);
    assert_eq!(t.discord().unwrap().bot_user_id().to_string(), TEST_BOT_ID);
    assert!(ih::bot_permissions(&t, ih::PermTarget::Guild(gid())).is_some());
}

#[tokio::test]
async fn button_message_timestamp_defaults_to_now() {
    let Interaction::Component(ci) = FakeInteraction::button("x:1").build() else { panic!("a component") };
    let age = chrono::Utc::now().timestamp_millis() - ci.message.timestamp.unix_timestamp() * 1000;
    assert!((0..60_000).contains(&age), "age {age}");
    let Interaction::Component(old) = FakeInteraction::button("x:1").message_timestamp("2026-01-01T00:00:00.000000+00:00").build() else { panic!() };
    assert_eq!(old.message.timestamp.unix_timestamp(), 1_767_225_600);
}

// ── The REST double and the REST helpers ─────────────────────────────────────

fn rest_guild() -> FakeGuild {
    FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS)
        .member(TEST_BOT_ID, &[])
        .channel("880000000000000030", 0, "general", &[])
}

fn channel_json(id: &str, guild: &str, name: &str) -> Value {
    json!({ "id": id, "type": 0, "guild_id": guild, "name": name, "position": 3, "permission_overwrites": [], "nsfw": false, "parent_id": null })
}

#[tokio::test]
async fn fake_rest_records_and_scripts_calls() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[rest_guild()], &rest);
    let http = t.http().unwrap();
    let msg = ChannelId::new(880000000000000030)
        .send_message(http.as_ref(), serenity::builder::CreateMessage::new().content("hi").embed(serenity::builder::CreateEmbed::new().title("T")))
        .await
        .expect("the default answer echoes a message");
    assert_eq!(msg.content, "hi");
    assert_eq!(msg.embeds[0].title.as_deref(), Some("T"));
    let sends = rest.calls_to("POST", "/channels/*/messages");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].path, "/channels/880000000000000030/messages");
    assert_eq!(sends[0].body["content"], "hi");

    // DMs: the default opens a channel.
    let dm = UserId::new(880000000000000010).create_dm_channel(http.as_ref()).await.expect("a DM channel");
    assert_eq!(dm.recipient.id, UserId::new(880000000000000010));

    // Scripted failure and the defaults' 404s, read through bot_core::discord.
    rest.fail("PUT", "/guilds/*/members/*/roles/*", 403, 50013, "Missing Permissions");
    let err = http.add_member_role(gid(), UserId::new(1), RoleId::new(2), Some("why")).await.unwrap_err();
    assert_eq!(discord::error_code(&err), Some(50013));
    assert!(discord::is_gone(&err));
    let err = http.get_channel(ChannelId::new(42)).await.unwrap_err();
    assert_eq!(discord::error_code(&err), Some(10003));
    let put = rest.calls_to("PUT", "/guilds/*/members/*/roles/*");
    assert_eq!(put[0].reason.as_deref(), Some("why"));

    // A one-shot script, then the default again.
    rest.on_times("GET", "/channels/43", 1, 200, channel_json("43", G, "once"));
    assert!(http.get_channel(ChannelId::new(43)).await.is_ok());
    assert!(http.get_channel(ChannelId::new(43)).await.is_err());
    rest.clear_calls();
    assert!(rest.calls().is_empty());
}

#[tokio::test]
async fn ban_bans_page_and_role_positions_have_the_discord_js_shape() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_rest(&rest);
    let http = t.http().unwrap();
    discord::ban_with_delete_seconds(&http, gid(), UserId::new(7), Some(3600), Some("spam & more")).await.unwrap();
    discord::ban_with_delete_seconds(&http, gid(), UserId::new(8), None, None).await.unwrap();
    let bans = rest.calls_to("PUT", "/guilds/*/bans/*");
    assert_eq!(bans[0].body, json!({ "delete_message_seconds": 3600 }));
    assert_eq!(bans[0].reason.as_deref(), Some("spam & more"));
    assert_eq!(bans[0].headers.get("x-audit-log-reason").map(String::as_str), Some("spam%20%26%20more"));
    assert_eq!(bans[1].body, json!({}));
    assert_eq!(bans[1].reason, None);

    rest.on("GET", "/guilds/*/bans", 200, json!([{ "reason": "r", "user": { "id": "9", "username": "u", "discriminator": "0", "avatar": null } }]));
    let page = discord::fetch_bans_page(&http, gid(), 1000, Some(discord::BanCursor::After(UserId::new(5)))).await.unwrap();
    assert_eq!(page.len(), 1);
    let call = &rest.calls_to("GET", "/guilds/*/bans")[0];
    assert_eq!(call.query_param("limit"), Some("1000"));
    assert_eq!(call.query_param("after"), Some("5"));

    let role = |id: &str, pos: u16| json!({ "id": id, "name": "r", "color": 0, "hoist": false, "position": pos, "permissions": "0", "managed": false, "mentionable": false, "flags": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null } });
    rest.on("PATCH", "/guilds/*/roles", 200, json!([role("11", 2), role("12", 1)]));
    let roles = discord::set_role_positions(&http, gid(), &[(RoleId::new(11), 2), (RoleId::new(12), 1)], Some("Moved")).await.unwrap();
    assert_eq!(roles.len(), 2);
    assert_eq!(roles[0].guild_id, gid());
    let patch = &rest.calls_to("PATCH", "/guilds/*/roles")[0];
    assert_eq!(patch.body, json!([{ "id": "11", "position": 2 }, { "id": "12", "position": 1 }]));
    assert_eq!(patch.reason.as_deref(), Some("Moved"));
    assert_eq!(discord::encode_audit_reason("a b/é!*'()"), "a%20b%2F%C3%A9!*'()");
}

#[tokio::test]
async fn channel_resolver_falls_back_to_rest_and_caches() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[rest_guild()], &rest);
    // Cached: no REST call.
    assert_eq!(ih::fetch_channel(&t, gid(), ChannelId::new(880000000000000030)).await.unwrap().name, "general");
    assert!(rest.calls().is_empty());
    // Uncached: fetched, then cached and usable.
    rest.on("GET", "/channels/880000000000000035", 200, channel_json("880000000000000035", G, "late"));
    assert!(ih::cached_channel(&t, gid(), ChannelId::new(880000000000000035)).is_none());
    let usable = ih::check_channel_usable_fetch(&t, gid(), Some(ChannelId::new(880000000000000035)), &ih::CHANNEL_POST_NEEDS).await;
    assert!(usable.ok, "{usable:?}");
    assert!(ih::check_channel_usable(&t, gid(), Some(ChannelId::new(880000000000000035)), &ih::CHANNEL_POST_NEEDS).ok, "now in the cache");
    // Another guild's channel is refused and not cached.
    rest.on("GET", "/channels/880000000000000036", 200, channel_json("880000000000000036", "880000000000000777", "theirs"));
    assert!(matches!(ih::fetch_channel(&t, gid(), ChannelId::new(880000000000000036)).await, Err(FetchChannelError::Unowned)));
    assert!(ih::cached_channel(&t, gid(), ChannelId::new(880000000000000036)).is_none());
    // Unknown: the REST error; the verdict says not found.
    assert!(matches!(ih::fetch_channel(&t, gid(), ChannelId::new(880000000000000037)).await, Err(FetchChannelError::Rest(_))));
    let v = ih::check_channel_usable_fetch(&t, gid(), Some(ChannelId::new(880000000000000037)), &ih::CHANNEL_POST_NEEDS).await;
    assert_eq!(v.reason.as_deref(), Some("Channel not found"));
    // check_channel_usable_for judges a channel in hand.
    let ch: serenity::all::GuildChannel = serde_json::from_value(channel_json("880000000000000038", G, "x")).unwrap();
    assert!(ih::check_channel_usable_for(&t, gid(), &ch, &["ViewChannel", "ManageChannels"]).missing == vec!["ManageChannels".to_string()]);
}

#[tokio::test]
async fn created_roles_and_channels_go_into_the_cache() {
    let t = TestApp::new().with_fake_guilds(&[rest_guild()]);
    let role: serenity::all::Role = serde_json::from_value(json!({ "id": "880000000000000070", "name": "New", "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false, "mentionable": false, "flags": 0, "guild_id": G, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null } })).unwrap();
    assert!(discord::cache_created_role(&t, gid(), &role));
    assert!(ih::with_cached_guild(&t, gid(), |g| g.roles.contains_key(&role.id)).unwrap());
    let ch: serenity::all::GuildChannel = serde_json::from_value(channel_json("880000000000000071", G, "fresh")).unwrap();
    assert!(discord::cache_created_channel(&t, &ch));
    assert_eq!(ih::cached_channel(&t, gid(), ch.id).unwrap().name, "fresh");
    let mut thread = json!({ "id": "880000000000000072", "type": 11, "guild_id": G, "name": "th", "parent_id": "880000000000000071", "owner_id": OWNER,
        "thread_metadata": { "archived": false, "auto_archive_duration": 60, "archive_timestamp": "2026-01-01T00:00:00.000000+00:00", "locked": false } });
    thread["permission_overwrites"] = json!([]);
    let th: serenity::all::GuildChannel = serde_json::from_value(thread).unwrap();
    assert!(discord::cache_created_channel(&t, &th));
    assert!(ih::with_cached_guild(&t, gid(), |g| g.threads.iter().any(|x| x.id == th.id)).unwrap());
    // A guild that is not cached: nothing to do.
    let other: serenity::all::GuildChannel = serde_json::from_value(channel_json("880000000000000073", "880000000000000777", "x")).unwrap();
    assert!(!discord::cache_created_channel(&t, &other));
}

// ── Members by ids ───────────────────────────────────────────────────────────

fn chunk(nonce: &str, ids: &[UserId]) -> serenity::all::GuildMembersChunkEvent {
    let members: Vec<Value> = ids
        .iter()
        .filter(|id| id.get() != 880000000000000099)
        .map(|id| json!({ "user": { "id": id.to_string(), "username": format!("u{id}"), "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 }))
        .collect();
    serde_json::from_value(json!({ "guild_id": G, "members": members, "chunk_index": 0, "chunk_count": 1, "nonce": nonce, "not_found": [] })).unwrap()
}

#[tokio::test]
async fn members_by_ids_over_the_gateway() {
    let t = TestApp::new().with_fake_guilds(&[rest_guild()]);
    let sent = Arc::new(AtomicUsize::new(0));
    let (app, count) = (t.app.clone(), sent.clone());
    set_chunk_sender(
        &t,
        Some(Arc::new(move |g, ids, nonce| {
            assert_eq!(g, gid());
            assert!(ids.len() <= 100);
            count.fetch_add(1, Ordering::SeqCst);
            assert!(deliver_members_chunk(&app.ext::<MemberChunks>(), &chunk(&nonce, &ids)));
            Box::pin(async { true })
        })),
    );
    let ids: Vec<UserId> = (0..150).map(|i| UserId::new(880000000000001000 + i)).chain([UserId::new(880000000000000099)]).collect();
    let got = fetch_members_by_ids(&t, gid(), &ids, 1_000).await.unwrap();
    assert_eq!(got.len(), 150, "the unknown id is absent");
    assert_eq!(sent.load(Ordering::SeqCst), 2, "100 ids per request");
    assert!(ih::with_cached_guild(&t, gid(), |g| g.members.contains_key(&UserId::new(880000000000001149))).unwrap(), "cached");
    // No chunk: a timeout. A chunk with another nonce is not delivered.
    set_chunk_sender(&t, Some(Arc::new(|_, _, _| Box::pin(async { true }))));
    assert_eq!(fetch_members_by_ids(&t, gid(), &ids[..1], 30).await.unwrap_err(), FetchMembersError::Timeout);
    assert!(!deliver_members_chunk(&t.ext::<MemberChunks>(), &chunk("stale", &ids[..1])));
    set_chunk_sender(&t, Some(Arc::new(|_, _, _| Box::pin(async { false }))));
    assert_eq!(fetch_members_by_ids(&t, gid(), &ids[..1], 30).await.unwrap_err(), FetchMembersError::NotSent);
    assert_eq!(fetch_members_by_ids(&TestApp::new(), gid(), &ids[..1], 30).await.unwrap_err(), FetchMembersError::Offline);
    assert!(bot_core::guild_members::has_chunk_sender(&t));
    set_chunk_sender(&t, None);
    assert!(!bot_core::guild_members::has_chunk_sender(&t));
}

#[tokio::test]
async fn members_by_ids_fall_back_to_rest_without_a_gateway() {
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[rest_guild()], &rest);
    rest.on("GET", "/guilds/*/members/880000000000000010", 200, json!({ "user": { "id": "880000000000000010", "username": "a", "discriminator": "0", "avatar": null }, "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 }));
    let got = fetch_members_by_ids(&t, gid(), &[UserId::new(880000000000000010), UserId::new(880000000000000011)], 1_000).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(rest.calls_to("GET", "/guilds/*/members/*").len(), 2);
}

// ── Hook state, registry boot and shutdown ───────────────────────────────────

#[tokio::test]
async fn automod_action_state() {
    let s = HookState::default();
    assert_eq!(s.automod_action(), None);
    s.set_automod_action(&AutomodAction { rule: "spam".into(), deleted: true });
    assert_eq!(s.automod_action(), Some(AutomodAction { rule: "spam".into(), deleted: true }));
    // Readers that take JSON (as feat-translation does) see the JS object.
    assert_eq!(s.get::<Value>("automodAction"), Some(json!({ "rule": "spam", "deleted": true })));
    s.insert("automodAction", AutomodAction { rule: "x".into(), deleted: false });
    assert_eq!(s.automod_action().map(|a| a.rule), Some("x".to_string()));
}

#[tokio::test]
async fn registry_boot_steps_and_shutdown_flushes() {
    let t = TestApp::new();
    let ran = Arc::new(AtomicUsize::new(0));
    let booted = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    let r = ran.clone();
    reg.on_shutdown(20, "log queue flush", 1_000, "Log queue was not flushed before shutdown", move |app| {
        let r = r.clone();
        async move {
            assert!(app.ready_at_ms().is_none());
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let b = booted.clone();
    reg.on_boot("count", move |_| {
        b.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("logged, not fatal")
    });
    let report = t.install(reg);
    assert_eq!(report.plugins.plugins, 0, "no plugin");
    assert_eq!(booted.load(Ordering::SeqCst), 1);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    bot_core::client::run_shutdown_flushes(&t).await;
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistent_problems_filtered_by_plugin() {
    let plugins = load_golden("plugins.json");
    let p = plugins.as_array().unwrap().iter().find(|p| p["components"].as_array().is_some_and(|c| !c.is_empty())).unwrap();
    let id = p["id"].as_str().unwrap();
    let reg = Registry::new();
    let all = persistent_handler_problems(&reg);
    let mine = persistent_handler_problems_for(&reg, &[id]);
    assert!(!mine.is_empty() && mine.len() < all.len());
    for line in &mine {
        let owned = p["components"].as_array().unwrap().iter().any(|c| line.contains(&format!("prefix \"{}\" has", c["prefix"].as_str().unwrap())))
            || p["jobs"].as_array().unwrap().iter().any(|j| line.starts_with(&format!("job type {} has", j.as_str().unwrap())));
        assert!(owned, "{line}");
    }
    assert!(persistent_handler_problems_for(&reg, &[]).is_empty());
}

// ── Scheduler and job-store failures ─────────────────────────────────────────

#[tokio::test]
async fn failing_job_store_and_try_get_job() {
    use bot_config::JobType;
    use bot_core::scheduler::ScheduleJob;
    let (t, failing) = TestApp::with_failing_jobs();
    let job = || ScheduleJob::new("880000000000000001", JobType::PollEnd, chrono::Utc::now()).dedupe_key("poll:1");
    failing.fail_next_schedules(1);
    assert!(t.scheduler().schedule_job(job()).await.is_err(), "the first schedule fails");
    assert!(t.scheduler().schedule_job(job()).await.is_ok(), "the retry succeeds");
    assert!(t.scheduler().try_get_job("poll:1").await.unwrap().is_some());
    failing.set_failing(true);
    assert!(t.scheduler().try_get_job("poll:1").await.is_err(), "a database error is an error");
    assert!(t.scheduler().get_job("poll:1").await.is_none(), "the JS-compatible variant swallows it");
    assert!(failing.failures() >= 3);
}

// ── Throttle on the app clock ────────────────────────────────────────────────

#[tokio::test]
async fn clock_throttle_follows_the_manual_clock() {
    let t = TestApp::new();
    let th = bot_core::clock::clock_throttle(t.clock(), 1_000);
    assert!(th.allow("k"));
    assert!(!th.allow("k"));
    t.clock.advance(999);
    assert!(!th.allow("k"));
    t.clock.advance(1);
    assert!(th.allow("k"));
    assert!(th.allow("other"));
}

// ── Adapters ─────────────────────────────────────────────────────────────────

const HASH: &str = "0123456789abcdef0123456789abcdef";

fn user(id: &str, avatar: Option<String>) -> serenity::all::User {
    serde_json::from_value(json!({ "id": id, "username": format!("name{id}"), "discriminator": "0", "avatar": avatar, "global_name": "Global" })).unwrap()
}

#[tokio::test]
async fn avatar_and_icon_urls_with_options() {
    use bot_core::adapters::*;
    let animated = user("880000000000000010", Some(format!("a_{HASH}")));
    let plain = user("880000000000000011", Some(HASH.into()));
    let none = user("880000000000000012", None);
    assert_eq!(display_avatar_url_default(&animated), format!("https://cdn.discordapp.com/avatars/880000000000000010/a_{HASH}.gif"));
    assert_eq!(display_avatar_url_default(&plain), format!("https://cdn.discordapp.com/avatars/880000000000000011/{HASH}.webp"));
    assert_eq!(user_display_avatar_url_with(&plain, ImageUrlOptions { size: Some(128), ..Default::default() }), format!("https://cdn.discordapp.com/avatars/880000000000000011/{HASH}.webp?size=128"));
    assert!(display_avatar_url_default(&none).starts_with("https://cdn.discordapp.com/embed/avatars/") && display_avatar_url_default(&none).ends_with(".png"));
    let member: serenity::all::Member = serde_json::from_value(json!({ "user": animated, "roles": [], "deaf": false, "mute": false, "flags": 0, "guild_id": G, "avatar": format!("a_{HASH}") })).unwrap();
    assert_eq!(member_static_png_avatar_url(gid(), &member), format!("https://cdn.discordapp.com/guilds/{G}/users/880000000000000010/avatars/a_{HASH}.png?size=256"));
    let hash: serenity::all::ImageHash = format!("a_{HASH}").parse().unwrap();
    assert_eq!(guild_icon_url_default(gid(), Some(&hash)), Some(format!("https://cdn.discordapp.com/icons/{G}/a_{HASH}.gif")));
    assert_eq!(guild_icon_url_with(gid(), Some(&hash), ImageUrlOptions::new("png", Some(64)).force_static()), Some(format!("https://cdn.discordapp.com/icons/{G}/a_{HASH}.png?size=64")));
}

#[tokio::test]
async fn clean_content_like_discord_js() {
    let guild = FakeGuild::new(G, OWNER)
        .role_with("880000000000000020", 1, Permissions::empty(), |r| r["name"] = json!("Mods"))
        .member_with("880000000000000010", &[], |m| m["nick"] = json!("Nicky"))
        .channel("880000000000000030", 0, "general", &[]);
    let t = TestApp::new().with_fake_guilds(&[guild]);
    let text = "hi <@880000000000000010> <@!880000000000000010> <@&880000000000000020> <#880000000000000030> </ping now:880000000000000040> <a:wave:880000000000000041> <@880000000000000098> <#880000000000000097> <@1>";
    assert_eq!(
        bot_core::adapters::clean_content_in(&t, Some(gid()), text),
        "hi @Nicky @Nicky @Mods #general /ping now :wave: <@880000000000000098> <#880000000000000097> <@1>"
    );
    assert_eq!(bot_core::adapters::clean_content_in(&t, None, "<@&880000000000000020>"), "<@&880000000000000020>", "no roles in DMs");
    let m = bot_core::testing::fake_message("880000000000000080", G, "880000000000000030", "880000000000000010", "to <#880000000000000030>");
    assert_eq!(bot_core::adapters::clean_content(&t, &m), "to #general");
}

// ── Markdown ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn escape_markdown_with_matches_discord_js() {
    let cases = load_golden("bot-core/markdown.json");
    for c in cases.as_array().unwrap() {
        let o = &c["options"];
        let get = |k: &str, d: bool| o.get(k).and_then(Value::as_bool).unwrap_or(d);
        let d = EscapeOptions::default();
        let opts = EscapeOptions {
            code_block: get("codeBlock", d.code_block),
            inline_code: get("inlineCode", d.inline_code),
            bold: get("bold", d.bold),
            italic: get("italic", d.italic),
            underline: get("underline", d.underline),
            strikethrough: get("strikethrough", d.strikethrough),
            spoiler: get("spoiler", d.spoiler),
            code_block_content: get("codeBlockContent", d.code_block_content),
            inline_code_content: get("inlineCodeContent", d.inline_code_content),
            escape: get("escape", d.escape),
            heading: get("heading", d.heading),
            bulleted_list: get("bulletedList", d.bulleted_list),
            numbered_list: get("numberedList", d.numbered_list),
            masked_link: get("maskedLink", d.masked_link),
        };
        let text = c["text"].as_str().unwrap();
        assert_eq!(escape_markdown_with(text, opts), c["out"].as_str().unwrap(), "escapeMarkdown({text:?}, {o})");
        if o.as_object().is_some_and(|m| m.is_empty()) {
            assert_eq!(escape_markdown(text), c["out"].as_str().unwrap());
        }
    }
}

#[tokio::test]
async fn escape_markdown_is_not_quadratic() {
    let mut text = "word ".repeat(3_000);
    text.push_str("snake_case and *star* https://x.y/a_b");
    let started = std::time::Instant::now();
    let out = escape_markdown(&text);
    assert!(out.ends_with("snake\\_case and \\*star\\* https://x.y/a_b"), "{}", &out[out.len() - 60..]);
    assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
}

// ── Components back to builders ──────────────────────────────────────────────

#[tokio::test]
async fn components_round_trip_to_builders() {
    let rows: Vec<serenity::all::ActionRow> = serde_json::from_value(json!([
        { "type": 1, "components": [
            { "type": 2, "style": 1, "custom_id": "a:1", "label": "Go", "emoji": { "name": "🎉" } },
            { "type": 2, "style": 5, "url": "https://example.com", "label": "Link", "disabled": true }
        ]},
        { "type": 1, "components": [
            { "type": 3, "custom_id": "s:1", "placeholder": "Pick", "min_values": 1, "max_values": 2, "options": [
                { "label": "A", "value": "a", "description": "first", "default": true }, { "label": "B", "value": "b" }
            ]}
        ]},
        { "type": 1, "components": [{ "type": 8, "custom_id": "c:1", "channel_types": [0, 5] }] }
    ]))
    .unwrap();
    let built = ih::components_to_builders(&rows);
    let json = serde_json::to_value(&built).unwrap();
    assert_eq!(json[0]["components"][0], json!({ "type": 2, "style": 1, "custom_id": "a:1", "label": "Go", "emoji": { "name": "🎉" }, "disabled": false }));
    assert_eq!(json[0]["components"][1]["url"], "https://example.com");
    assert_eq!(json[0]["components"][1]["disabled"], true);
    let select = &json[1]["components"][0];
    assert_eq!(select["custom_id"], "s:1");
    assert_eq!(select["placeholder"], "Pick");
    assert_eq!(select["max_values"], 2);
    assert_eq!(select["options"][0]["default"], true);
    assert_eq!(select["options"][0]["description"], "first");
    assert_eq!(json[2]["components"][0]["type"], 8);
    assert_eq!(json[2]["components"][0]["channel_types"], json!([0, 5]));
}

// ── Settings peek-or-load ────────────────────────────────────────────────────

#[tokio::test]
async fn peek_or_load_reads_the_memo_or_loads() {
    let t = TestApp::new();
    let s = t.settings().peek_or_load("880000000000000001", 1_000).await;
    assert!(s.is_some(), "loaded from the in-memory store");
    assert!(t.settings().peek_settings("880000000000000001").is_some(), "and memoised");
}

// ── new_targets puts what it creates into the cache ──────────────────────────

#[tokio::test]
async fn new_channel_is_cached_after_creation() {
    use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand, channel_type};
    use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
    let rest = FakeRest::new();
    let guild = FakeGuild::new(G, OWNER)
        .role("880000000000000021", 5, Permissions::MANAGE_CHANNELS | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS)
        .member(TEST_BOT_ID, &["880000000000000021"]);
    let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
    let data = SlashCommand::new("counting", "Counting.").option(
        Opt::sub_command("set", "Set.")
            .option(Opt::channel("channel", "The channel.").channel_types(&[channel_type::GUILD_TEXT]))
            .option(new_channel_option("channel", "channel")),
    );
    t.commands().insert(CommandDef::new("test", data, None, |_| async { Ok(()) }));
    rest.on("POST", "/guilds/*/channels", 200, channel_json("880000000000000090", G, "counting-here"));
    let mut ctx = command_ctx(
        &t,
        FakeInteraction::slash("counting").guild(G).user(OWNER).permissions(Permissions::ADMINISTRATOR).subcommand("set").option("new_channel", 3, json!("counting here")),
    );
    assert!(resolve_new_options(&mut ctx, NewOptions { required: &["channel"], private: &[] }).await);
    assert_eq!(ctx.get_channel_id("channel"), Some(ChannelId::new(880000000000000090)));
    assert_eq!(rest.calls_to("POST", "/guilds/*/channels")[0].body["name"], "counting here");
    let usable = ih::check_channel_usable(&t, gid(), Some(ChannelId::new(880000000000000090)), &ih::CHANNEL_POST_NEEDS);
    assert!(usable.ok, "the created channel is in the cache at once: {usable:?}");
}

// ── Emoji ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rgi_emoji_matcher_is_exact() {
    use bot_core::custom_embed::{is_extended_pictographic, is_rgi_emoji, is_unicode_emoji, is_unicode_emoji_exact};
    let data: Value = serde_json::from_str(include_str!("../data/emoji-zwj.json")).unwrap();
    let zwj = data["zwj"].as_array().unwrap();
    assert!(zwj.len() > 1000, "{} sequences", zwj.len());
    for s in zwj {
        assert!(is_rgi_emoji(s.as_str().unwrap()), "{s}");
    }
    // Verdicts of Node's /^\p{RGI_Emoji}$/v.
    for (s, rgi) in [
        ("😀", true),
        ("❤️", true),
        ("👍🏽", true),
        ("🇫🇷", true),
        ("1️⃣", true),
        ("👨‍👩‍👧‍👦", true),
        ("🏳️‍🌈", true),
        ("👩🏻‍🤝‍👨🏼", true),
        ("🧑🏻‍❤️‍💋‍🧑🏼", true),
        ("🐶‍🐱", false),
        ("😀‍😀", false),
        ("❤", false),
        ("a", false),
    ] {
        assert_eq!(is_rgi_emoji(s), rgi, "{s}");
    }
    assert!(is_unicode_emoji_exact("❤"), "a pictographic without FE0F");
    assert!(!is_unicode_emoji_exact("🐶‍🐱"));
    assert!(is_unicode_emoji("🐶‍🐱"), "the structural check stays as it was");
    assert!(is_extended_pictographic('❤') && !is_extended_pictographic('a'));
}
