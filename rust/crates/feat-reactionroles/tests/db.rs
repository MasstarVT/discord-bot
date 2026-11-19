//! Database tests of feat-reactionroles (the `D*` cases of `test/reaction-roles.test.js`): every service query, the
//! panel index, the prune/sync engine, the component handlers and every `/reactionrole` subcommand through the router,
//! and both cross-language directions (rows and the Redis index). They need `TEST_DATABASE_URL` (and `TEST_REDIS_URL`
//! for the cache cases); without them they return early.

mod common;

use std::sync::Arc;

use bot_core::command::option_type;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, database_url, offline_discord_as, redis_url, run_node};
use common::FakeOps;
use feat_reactionroles::reaction_role_service::{self as rr, NewItem, PanelArg, PanelDraft, PanelPatch};
use serde_json::{Value, json};
use serenity::all::Permissions as P;

const OWNER: &str = "600000000000000099";
const ADMIN: &str = "600000000000000009";
const ADMIN_ROLE: &str = "820000000000000002";
const BOT_ROLE: &str = "820000000000000001";
const USER: &str = "700000000000000001";

fn g(n: u32) -> String {
    format!("9550191{n:011}")
}

fn role(n: u32, i: u32) -> String {
    format!("4{:05}{:012}", n, i)
}

fn ch(n: u32) -> String {
    format!("3{:05}{:012}", n, 1)
}

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_reactionroles::register(&mut reg);
    reg
}

struct W {
    t: DbTestApp,
    ops: FakeOps,
    g: String,
    roles: Vec<String>,
    channel: String,
}

impl W {
    /// A guild with a text channel, four panel roles below the bot, the bot (Manage Roles and the channel
    /// permissions) and an admin; the same world in the serenity cache (for the command's checks) and in the fake
    /// Discord operations (for the writes).
    async fn new(n: u32) -> Option<Self> {
        let url = database_url()?;
        let g = g(n);
        let t = DbTestApp::new(&url).await.with_guilds([g.as_str()]);
        t.reset_guilds(&[&g]).await;
        let ops = FakeOps::new();
        rr::set_discord_ops(&t, Arc::new(ops.clone()));
        let channel = ch(n);
        let roles: Vec<String> = (1..=4).map(|i| role(n, i)).collect();
        let bot_perms = P::MANAGE_ROLES | P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::ADD_REACTIONS | P::READ_MESSAGE_HISTORY | P::MANAGE_MESSAGES;
        let mut guild = FakeGuild::new(&g, OWNER)
            .role(BOT_ROLE, 20, bot_perms)
            .member(TEST_BOT_ID, &[BOT_ROLE])
            .role(ADMIN_ROLE, 30, P::MANAGE_GUILD)
            .member(ADMIN, &[ADMIN_ROLE])
            .channel(&channel, 0, "roles", &[]);
        for (i, r) in roles.iter().enumerate() {
            guild = guild.role(r, (i + 1) as u16, P::empty());
            ops.role(r, &format!("Role {}", i + 1), (i + 1) as i64);
        }
        t.set_discord(offline_discord_as(&[guild.json()]));
        ops.channel(&channel, "roles");
        t.settings().ensure_guild_settings(&g).await.expect("settings row");
        Some(Self { t, ops, g, roles, channel })
    }

    async fn panel(&self, kind: &str, layout: &str, max_roles: Option<i32>, emojis: &[Option<&str>]) -> PanelDraft {
        let id = bot_db::new_uuid();
        let message = self.ops.w().next_id + 1;
        let message_id = format!("5{:017}", 100_000 + message);
        self.ops.w().next_id += 1;
        self.ops.message(&self.g, &self.channel, &message_id);
        let items = self
            .roles
            .iter()
            .zip(emojis)
            .enumerate()
            .map(|(i, (r, e))| {
                rr::draft_item(&id, &NewItem { role_id: r.clone(), label: format!("Role {}", i + 1), emoji: e.map(str::to_string), style: Some(1), description: None }, i as i32)
                    .unwrap()
            })
            .collect();
        let draft = PanelDraft {
            id,
            guild_id: self.g.clone(),
            channel_id: self.channel.clone(),
            message_id,
            title: "Panel".into(),
            description: None,
            kind: kind.into(),
            layout: layout.into(),
            max_roles,
            plain_text: false,
            message_content: None,
            attached: false,
            placeholder: None,
            items,
        };
        rr::create_panel(self.t.db(), &draft).await.expect("panel created");
        draft
    }

    async fn done(&self) {
        rr::idle(&self.t).await;
        self.t.reset_guilds(&[&self.g]).await;
    }

    fn calls(&self) -> Vec<bot_core::interaction::RecordedCall> {
        self.t.responses.calls()
    }

    fn last(&self) -> bot_core::interaction::RecordedCall {
        self.calls().last().cloned().expect("an answer")
    }
}

fn four(e: Option<&str>) -> Vec<Option<&str>> {
    vec![e; 4]
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn raw_sql_prepares_against_the_schema() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.unwrap();
    for (name, sql) in rr::raw_sql() {
        sqlx::Executor::prepare(&mut *tx, sqlx::SqlSafeStr::into_sql_str(sql)).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    tx.rollback().await.unwrap();
}

// ── D1-D3: the queries ────────────────────────────────────────────────────────

#[tokio::test]
async fn d1_create_get_add_remove_and_the_unique_custom_id() {
    let Some(w) = W::new(30).await else { return };
    let db = w.t.db();
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None, None, None]).await;
    let got = rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap();
    assert_eq!(got.items.iter().map(|i| i.position).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(got.items[0].custom_id, format!("rr:{}:{}", draft.id, w.roles[0]));
    assert!(got.group.message_missing_at.is_none());
    assert!(rr::get_panel(db, &g(31), &draft.id).await.unwrap().is_none(), "scoped to the guild");
    assert_eq!(rr::count_panels(db, &w.g).await.unwrap(), 1);
    assert_eq!(rr::find_panel_by_message(db, &w.g, &draft.message_id).await.unwrap().unwrap(), rr::PanelRef { id: draft.id.clone(), title: "Panel".into() });
    assert!(rr::find_panel_by_message(db, &w.g, "500000000000000001").await.unwrap().is_none());

    let new = |r: &str, label: &str| NewItem { role_id: r.into(), label: label.into(), emoji: Some("🔔".into()), style: Some(3), description: Some("About four".into()) };
    let added = rr::add_item(db, &got.group.id, &got.items, &new(&w.roles[3], "Four")).await.unwrap();
    assert_eq!((added.position, added.description.as_deref()), (3, Some("About four")));
    let dup = rr::add_item(db, &got.group.id, &got.items, &new(&w.roles[3], "Dup")).await.unwrap_err();
    assert!(dup.downcast_ref::<bot_db::DbError>().is_some_and(|e| e.is_unique_violation()), "P2002: {dup}");
    let fresh = rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap();
    rr::remove_item(db, &fresh.group.id, &fresh.items[1].id).await.unwrap();
    let after = rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap();
    assert_eq!(
        after.items.iter().map(|i| (i.role_id.clone(), i.position)).collect::<Vec<_>>(),
        vec![(w.roles[0].clone(), 0), (w.roles[2].clone(), 1), (w.roles[3].clone(), 2)]
    );
    // a failed create leaves no group behind (transaction): the item's customId is taken
    let mut bad = draft.clone();
    bad.id = bot_db::new_uuid();
    bad.message_id = "500000000000000777".into();
    bad.items = vec![rr::draft_item(&draft.id, &NewItem { role_id: w.roles[0].clone(), label: "x".into(), ..Default::default() }, 0).unwrap()];
    assert!(rr::create_panel(db, &bad).await.is_err());
    assert_eq!(rr::count_panels(db, &w.g).await.unwrap(), 1);
    w.done().await;
}

#[tokio::test]
async fn d2_update_move_delete_are_scoped_and_the_missing_marker() {
    let Some(w) = W::new(32).await else { return };
    let db = w.t.db();
    let other = g(33);
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    let hacked = PanelPatch { title: Some("Hacked".into()), ..Default::default() };
    assert!(rr::update_panel(db, &other, &draft.id, &hacked).await.unwrap().is_none());
    assert_eq!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.title, "Panel");
    let patch = PanelPatch {
        title: Some("New".into()),
        description: Some(None),
        placeholder: Some(Some("Pick".into())),
        max_roles: Some(Some(3)),
        plain_text: Some(true),
        message_content: Some(Some("Hi".into())),
        ..Default::default()
    };
    let up = rr::update_panel(db, &w.g, &draft.id, &patch).await.unwrap().unwrap();
    assert_eq!(
        (up.group.title.as_str(), up.group.placeholder.as_deref(), up.group.max_roles, up.group.plain_text, up.group.message_content.as_deref()),
        ("New", Some("Pick"), Some(3), true, Some("Hi"))
    );
    assert_eq!(up.items.len(), 1);

    rr::mark_panel_missing(&w.t, &w.g, &draft.id).await;
    let first = rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.expect("flagged");
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    rr::mark_panel_missing(&w.t, &w.g, &draft.id).await;
    assert_eq!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at, Some(first), "the first marker is kept");
    w.ops.w().available = false;
    rr::clear_panel_missing(db, &w.g, &draft.id).await.unwrap();
    rr::mark_panel_missing(&w.t, &w.g, &draft.id).await;
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.is_none(), "rule 20");
    w.ops.w().available = true;
    rr::mark_panel_missing(&w.t, &w.g, &draft.id).await;

    rr::move_panel(db, &other, &draft.id, "300000000000000007", "500000000000000077", true).await.unwrap();
    assert!(!rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.attached, "another guild cannot move it");
    rr::move_panel(db, &w.g, &draft.id, "300000000000000007", "500000000000000077", true).await.unwrap();
    let moved = rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group;
    assert_eq!((moved.channel_id.as_str(), moved.message_id.as_str(), moved.attached, moved.message_missing_at), ("300000000000000007", "500000000000000077", true, None));

    rr::delete_panel(db, &other, &draft.id).await.unwrap();
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().is_some());
    rr::delete_panel(db, &w.g, &draft.id).await.unwrap();
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().is_none());
    let left: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ReactionRoleItem" WHERE "groupId" = $1"#).bind(&draft.id).fetch_one(db).await.unwrap();
    assert_eq!(left, 0, "items cascade");
    w.done().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn d3_search_and_list_match_the_js_queries() {
    let Some(w) = W::new(34).await else { return };
    let db = w.t.db();
    let mut made = Vec::new();
    for i in 0..30 {
        let mut d = w.panel("NORMAL", "BUTTONS", None, if i == 3 { &[None, None] } else { &[] }).await;
        if i == 3 {
            rr::update_panel(db, &w.g, &d.id, &PanelPatch { title: Some("Special NEWS 100%_panel".into()), ..Default::default() }).await.unwrap();
        } else {
            rr::update_panel(db, &w.g, &d.id, &PanelPatch { title: Some(format!("Panel {i}")), ..Default::default() }).await.unwrap();
        }
        d.title.clear();
        made.push(d);
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let all = rr::search_panels(db, &w.g, "").await.unwrap();
    assert_eq!(all.len(), 25);
    assert_eq!(all[0].id, made[29].id, "newest first");
    let by_title = rr::search_panels(db, &w.g, "news").await.unwrap();
    assert_eq!(by_title.iter().map(|r| (r.id.clone(), r.item_count, r.layout.clone())).collect::<Vec<_>>(), vec![(made[3].id.clone(), 2, "BUTTONS".into())]);
    assert_eq!(rr::search_panels(db, &w.g, &made[7].id).await.unwrap().len(), 1, "by id");
    assert_eq!(rr::search_panels(db, &w.g, &made[8].message_id).await.unwrap().len(), 1, "by message id");
    assert!(rr::search_panels(db, &w.g, "zzz-nothing").await.unwrap().is_empty());
    assert!(rr::search_panels(db, &g(35), "").await.unwrap().is_empty(), "scoped to the guild");
    assert!(rr::search_panels(db, &w.g, "Panel 0").await.unwrap().iter().any(|r| r.id == made[0].id), "an old panel is found by a DB-side filter");
    // the same queries through the JS service (Prisma's contains escapes % and _)
    for q in ["", "news", "100%", "%", "_panel", "Panel 1", &made[8].message_id, &made[7].id] {
        let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["search", &w.g, q]);
        let rust = serde_json::to_value(rr::search_panels(db, &w.g, q).await.unwrap()).unwrap();
        assert_eq!(rust, js, "searchPanels {q:?}");
    }

    let full = rr::get_panel(db, &w.g, &made[3].id).await.unwrap().unwrap();
    assert_eq!(rr::search_items(db, &w.g, &full.group.id, "", &[]).await.unwrap().len(), 2);
    assert_eq!(rr::search_items(db, &w.g, &full.group.id, "ROLE 2", &[]).await.unwrap().iter().map(|i| i.label.clone()).collect::<Vec<_>>(), vec!["Role 2"]);
    assert_eq!(rr::search_items(db, &w.g, &full.group.id, "zzz", &[w.roles[0].clone()]).await.unwrap()[0].role_id, w.roles[0], "role-name matches by id");
    assert_eq!(rr::search_items(db, &w.g, &full.group.id, &w.roles[1], &[]).await.unwrap()[0].role_id, w.roles[1]);
    assert!(rr::search_items(db, &g(35), &full.group.id, "", &[]).await.unwrap().is_empty(), "scoped to the guild");
    for (q, ids) in [("", json!([])), ("role", json!([])), ("zzz", json!([w.roles[0]]))] {
        let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["items", &w.g, &full.group.id, q, &ids.to_string()]);
        let rust = serde_json::to_value(rr::search_items(db, &w.g, &full.group.id, q, &serde_json::from_value::<Vec<String>>(ids).unwrap()).await.unwrap()).unwrap();
        assert_eq!(rust, js, "searchItems {q:?}");
    }

    let listed = rr::list_panels(db, &w.g).await.unwrap();
    assert_eq!(listed.len(), 30);
    assert_eq!(listed[0].id, made[29].id);
    assert_eq!(listed.iter().find(|p| p.id == made[3].id).unwrap().item_count, 2);
    let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["list", &w.g]);
    assert_eq!(serde_json::to_value(&listed).unwrap(), js, "listPanels");
    w.done().await;
}

// ── D4: the panel index ───────────────────────────────────────────────────────

#[tokio::test]
async fn d4_index_loads_from_postgres_and_survives_an_invalidation_race() {
    let Some(w) = W::new(36).await else { return };
    rr::reset_panel_index(&w.t);
    let a = w.panel("NORMAL", "REACTIONS", None, &[Some("❤️"), Some("<:pog:123456789012345678>")]).await;
    assert!(rr::peek_panel_index(&w.t, &w.g).is_none());
    let index = rr::get_panel_index(&w.t, &w.g).await.unwrap();
    let panel = index.get(&a.message_id).unwrap();
    assert_eq!(panel.layout, "REACTIONS");
    assert_eq!(panel.items.iter().map(|i| i.key.clone().unwrap()).collect::<Vec<_>>(), vec!["❤", "123456789012345678"]);
    assert!(rr::peek_panel_index(&w.t, &w.g).unwrap().fresh);
    assert!(Arc::ptr_eq(&rr::get_panel_index(&w.t, &w.g).await.unwrap(), &index), "served from the memo");

    rr::invalidate_panel_index(&w.t, &w.g).await;
    assert!(rr::peek_panel_index(&w.t, &w.g).is_none());
    let (t1, t2) = (w.t.app.clone(), w.t.app.clone());
    let (g1, g2) = (w.g.clone(), w.g.clone());
    let loading = tokio::spawn(async move { rr::get_panel_index(&t1, &g1).await });
    let second = tokio::spawn(async move { rr::get_panel_index(&t2, &g2).await });
    tokio::task::yield_now().await;
    rr::invalidate_panel_index(&w.t, &w.g).await; // while the load is in flight
    let (x, y) = (loading.await.unwrap().unwrap(), second.await.unwrap().unwrap());
    assert!(x.contains_key(&a.message_id) && y.contains_key(&a.message_id));
    assert!(rr::get_panel_index(&w.t, &w.g).await.unwrap().contains_key(&a.message_id));
    assert!(rr::peek_panel_index(&w.t, &w.g).is_some());
    w.done().await;
}

// ── D5-D7: prune, sync, message delete ────────────────────────────────────────

#[tokio::test]
async fn d5_prune_removes_items_across_panels_renumbers_and_rerenders() {
    let Some(w) = W::new(37).await else { return };
    let db = w.t.db();
    let p1 = w.panel("NORMAL", "BUTTONS", None, &[None, None, None]).await;
    let p2 = {
        let id = bot_db::new_uuid();
        let mid = "500000000000370002".to_string();
        w.ops.message(&w.g, &w.channel, &mid);
        let items = vec![
            rr::draft_item(&id, &NewItem { role_id: w.roles[1].clone(), label: "two".into(), emoji: Some("🔔".into()), ..Default::default() }, 0).unwrap(),
            rr::draft_item(&id, &NewItem { role_id: w.roles[3].clone(), label: "four".into(), emoji: Some("🎉".into()), ..Default::default() }, 1).unwrap(),
        ];
        let d = PanelDraft { id, message_id: mid, layout: "REACTIONS".into(), items, ..p1.clone() };
        rr::create_panel(db, &d).await.unwrap();
        d
    };
    w.ops.add_reaction(&p2.message_id, "🔔", true);
    let p3 = {
        let id = bot_db::new_uuid();
        let mid = "500000000000370003".to_string();
        w.ops.message(&w.g, &w.channel, &mid);
        let d = PanelDraft { id: id.clone(), message_id: mid, items: vec![rr::draft_item(&id, &NewItem { role_id: "400000000000999999".into(), label: "x".into(), ..Default::default() }, 0).unwrap()], ..p1.clone() };
        rr::create_panel(db, &d).await.unwrap();
        d
    };

    assert_eq!(rr::prune_items_for_role(&w.t, &w.g, &w.roles[1]).await, 2);
    rr::idle(&w.t).await;
    let a = rr::get_panel(db, &w.g, &p1.id).await.unwrap().unwrap();
    assert_eq!(a.items.iter().map(|i| (i.role_id.clone(), i.position)).collect::<Vec<_>>(), vec![(w.roles[0].clone(), 0), (w.roles[2].clone(), 1)]);
    let b = rr::get_panel(db, &w.g, &p2.id).await.unwrap().unwrap();
    assert_eq!(b.items.iter().map(|i| (i.role_id.clone(), i.position)).collect::<Vec<_>>(), vec![(w.roles[3].clone(), 0)]);
    let edits = w.ops.w().edits.clone();
    let e1: Vec<&Value> = edits.iter().filter(|(m, _)| *m == p1.message_id).map(|(_, b)| b).collect();
    assert_eq!(e1.len(), 1, "the panel message was re-rendered");
    assert_eq!(e1[0]["components"][0]["components"].as_array().unwrap().len(), 2);
    assert!(edits.iter().all(|(m, _)| *m != p3.message_id), "an unrelated panel is not touched");
    assert!(w.ops.w().messages[&p2.message_id].reaction("🔔").is_none(), "the reaction of the removed item is cleared");
    assert_eq!(rr::prune_items_for_role(&w.t, &w.g, &w.roles[1]).await, 0);
    w.ops.w().available = false;
    assert_eq!(rr::prune_items_for_role(&w.t, &w.g, &w.roles[0]).await, 0, "rule 20: never while the guild is unavailable");
    w.ops.w().available = true;
    assert_eq!(rr::prune_items_for_role(&w.t, &g(38), &w.roles[0]).await, 0, "other guilds are not touched");
    assert_eq!(rr::get_panel(db, &w.g, &p1.id).await.unwrap().unwrap().items.len(), 2);
    // the hook form
    w.t.install(registry());
    w.t.run_hooks(bot_core::prelude::RoleDelete { guild_id: w.g.parse::<u64>().unwrap().into(), role_id: w.roles[0].parse::<u64>().unwrap().into(), role: None }).await;
    assert_eq!(rr::get_panel(db, &w.g, &p1.id).await.unwrap().unwrap().items.len(), 1);
    w.done().await;
}

#[tokio::test]
async fn d6_sync_fetch_and_the_missing_marker() {
    let Some(w) = W::new(39).await else { return };
    let db = w.t.db();
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    let ok = rr::sync_panel(&w.t, &w.g, PanelArg::Id(draft.id.clone()), Some(0xabcdef)).await;
    assert_eq!(ok.status, "ok");
    let body = w.ops.w().edits[0].1.clone();
    assert_eq!(body["embeds"][0]["color"], json!(0xabcdef));
    assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
    assert_eq!(rr::sync_panel(&w.t, &w.g, PanelArg::Id(bot_db::new_uuid()), None).await.status, "skipped");
    w.ops.w().edit_error = Some(50035);
    assert_eq!(
        rr::sync_panel(&w.t, &w.g, PanelArg::Id(draft.id.clone()), None).await,
        rr::SyncResult { status: "failed", code: Some(50035), reason: Some("Discord rejected the panel content".into()) }
    );
    w.ops.w().edit_error = Some(50005);
    assert_eq!(rr::sync_panel(&w.t, &w.g, PanelArg::Id(draft.id.clone()), None).await.status, "failed");
    w.ops.w().edit_error = Some(10008);
    assert_eq!(rr::sync_panel(&w.t, &w.g, PanelArg::Id(draft.id.clone()), None).await.status, "missing");
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.is_some(), "a 10008 flags the panel");
    w.ops.w().edit_error = None;
    assert_eq!(rr::sync_panel(&w.t, &w.g, PanelArg::Id(draft.id.clone()), None).await.status, "ok");
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.is_none(), "a successful sync clears the flag");

    w.ops.w().messages.clear();
    let p = rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap();
    assert_eq!(rr::fetch_panel(&w.t, &w.g, &p).await.status(), "missing");
    w.ops.w().channels.clear();
    let no_channel = rr::fetch_panel(&w.t, &w.g, &p).await;
    assert!(matches!(no_channel, rr::Fetched::Missing { code: Some(10003), .. }));
    w.ops.w().channel_fetch_error = Some(Some(50001));
    assert_eq!(rr::fetch_panel(&w.t, &w.g, &p).await.status(), "failed");
    rr::clear_panel_missing(db, &w.g, &draft.id).await.unwrap();
    w.ops.w().channel_fetch_error = Some(None);
    assert_eq!(rr::fetch_panel(&w.t, &w.g, &p).await.status(), "failed", "a cache miss with an unknown REST error is not a deletion");
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.is_none());

    let mut attached = w.panel("NORMAL", "REACTIONS", None, &[]).await;
    rr::move_panel(db, &w.g, &attached.id, &w.channel, &attached.message_id, true).await.unwrap();
    attached.attached = true;
    assert_eq!(rr::sync_panel(&w.t, &w.g, PanelArg::Id(attached.id.clone()), None).await.status, "skipped", "attached messages are never edited");
    w.done().await;
}

#[tokio::test]
async fn d7_message_delete_flags_the_panel_unless_the_bot_deleted_it() {
    let Some(w) = W::new(40).await else { return };
    let db = w.t.db();
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    let other = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    rr::reset_panel_index(&w.t);
    rr::handle_message_delete(&w.t, &w.g, "500000000000000001").await;
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.is_none());
    bot_core::deletion_tracker::mark_bot_deletion(&w.t, &[other.message_id.as_str()], Some("reaction-roles:delete"), None);
    rr::handle_message_delete(&w.t, &w.g, &other.message_id).await;
    assert!(rr::get_panel(db, &w.g, &other.id).await.unwrap().unwrap().group.message_missing_at.is_none(), "our own deletions are ignored");
    bot_core::deletion_tracker::mark_bot_deletion(&w.t, &[draft.message_id.as_str()], Some("purge"), None);
    rr::handle_message_delete(&w.t, &w.g, &draft.message_id).await;
    assert!(rr::get_panel(db, &w.g, &draft.id).await.unwrap().unwrap().group.message_missing_at.is_some(), "another feature's deletion still flags the panel");
    w.done().await;
}

// ── D8-D13: the component handlers through the router ─────────────────────────

fn click(w: &W, custom_id: &str) -> serenity::all::Interaction {
    FakeInteraction::button(custom_id).guild(&w.g).user(USER).build()
}

fn choose(w: &W, panel: &str, values: &[&str], personal: bool) -> serenity::all::Interaction {
    let mut f = FakeInteraction::select(&format!("rr-select:{panel}"), values).guild(&w.g).user(USER);
    if personal {
        f.json_mut()["message"]["flags"] = json!(64);
    }
    f.build()
}

#[tokio::test]
async fn d8_buttons_toggle_with_a_fresh_member_read_and_reply_ephemerally() {
    let Some(w) = W::new(41).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &four(None)).await;
    w.ops.member(USER, &[]);
    let id = format!("rr:{}:{}", draft.id, w.roles[0]);
    route_interaction(&w.t, click(&w, &id)).await;
    let calls = w.calls();
    assert_eq!(calls[0].method, "deferReply");
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].method, "editReply");
    assert_eq!(calls[1].content(), Some(format!("✅ Added: <@&{}>", w.roles[0]).as_str()));
    assert_eq!(calls[1].body["allowed_mentions"]["parse"], json!([]), "pings nobody");
    // the interaction snapshot still says "no role", the handler re-reads the member
    route_interaction(&w.t, click(&w, &id)).await;
    assert_eq!(w.last().content(), Some(format!("➖ Removed: <@&{}>", w.roles[0]).as_str()));
    assert_eq!(w.ops.role_calls().iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["add", "remove"]);
    // a role that is not on the panel, an unknown panel
    route_interaction(&w.t, click(&w, &format!("rr:{}:400000000000000001", draft.id))).await;
    assert_eq!(w.last().content(), Some("⚠️ That option is no longer on this panel."));
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", bot_db::new_uuid(), w.roles[0]))).await;
    assert_eq!(w.last().content(), Some("⚠️ This panel is no longer active."));
    w.done().await;
}

#[tokio::test]
async fn d8b_legacy_panels_keep_working() {
    let Some(w) = W::new(42).await else { return };
    w.t.install(registry());
    // a cuid group id and a stored emoji that is not valid
    let id = bot_db::new_cuid();
    let mid = "500000000000420001".to_string();
    w.ops.message(&w.g, &w.channel, &mid);
    let draft = PanelDraft {
        id: id.clone(),
        guild_id: w.g.clone(),
        channel_id: w.channel.clone(),
        message_id: mid,
        title: "Old".into(),
        description: None,
        kind: "NORMAL".into(),
        layout: "BUTTONS".into(),
        max_roles: None,
        plain_text: false,
        message_content: None,
        attached: false,
        placeholder: None,
        items: vec![rr::draft_item(&id, &NewItem { role_id: w.roles[0].clone(), label: "Old".into(), emoji: Some(":bad:".into()), ..Default::default() }, 0).unwrap()],
    };
    rr::create_panel(w.t.db(), &draft).await.unwrap();
    w.ops.member(USER, &[]);
    route_interaction(&w.t, click(&w, &format!("rr:{id}:{}", w.roles[0]))).await;
    assert_eq!(w.last().content(), Some(format!("✅ Added: <@&{}>", w.roles[0]).as_str()));
    assert_eq!(rr::sync_panel(&w.t, &w.g, PanelArg::Id(id), None).await.status, "ok");
    assert!(w.ops.w().edits[0].1["components"][0]["components"][0].get("emoji").is_none());
    w.done().await;
}

#[tokio::test]
async fn d9_unique_verify_reverse_the_cap_and_unmanageable_roles() {
    let Some(w) = W::new(43).await else { return };
    w.t.install(registry());
    let unique = w.panel("UNIQUE", "BUTTONS", None, &four(None)).await;
    w.ops.member(USER, &[&w.roles[1]]);
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", unique.id, w.roles[0]))).await;
    assert_eq!(w.last().content(), Some(format!("✅ Added: <@&{}>\n➖ Removed: <@&{}>", w.roles[0], w.roles[1]).as_str()));
    let verify = w.panel("VERIFY", "BUTTONS", None, &four(None)).await;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", verify.id, w.roles[0]))).await;
    assert_eq!(w.last().content(), Some(format!("You already have <@&{}>.", w.roles[0]).as_str()));
    let reverse = w.panel("REVERSE", "BUTTONS", None, &four(None)).await;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", reverse.id, w.roles[2]))).await;
    assert_eq!(w.last().content(), Some(format!("You don't have <@&{}>, so there's nothing to remove.", w.roles[2]).as_str()));
    let capped = w.panel("NORMAL", "BUTTONS", Some(1), &four(None)).await;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", capped.id, w.roles[2]))).await;
    assert_eq!(w.last().content(), Some("You can hold at most **1** role(s) from this panel. Remove one first."));
    w.ops.w().roles.iter_mut().find(|r| r.id == w.roles[3]).unwrap().editable = false;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", capped.id, w.roles[0]))).await;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", unique.id, w.roles[3]))).await;
    assert_eq!(w.last().content(), Some(format!("I can't manage <@&{}> because it's above my highest role. Ask an admin to move my role higher.", w.roles[3]).as_str()));
    w.ops.w().manage_roles = false;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", unique.id, w.roles[2]))).await;
    assert_eq!(w.last().content(), Some("I need the **Manage Roles** permission to give roles. Ask an admin to fix my permissions."));
    w.done().await;
}

#[tokio::test]
async fn d10_a_deleted_role_is_confirmed_over_rest_pruned_and_reported() {
    let Some(w) = W::new(44).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &four(None)).await;
    w.ops.member(USER, &[]);
    w.ops.w().roles.retain(|r| r.id != w.roles[0]);
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", draft.id, w.roles[0]))).await;
    assert_eq!(w.last().content(), Some("That role no longer exists, so I removed it from this panel."));
    rr::idle(&w.t).await;
    assert_eq!(rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().unwrap().items.len(), 3);
    // REST cannot tell: nothing is pruned
    w.ops.w().roles.retain(|r| r.id != w.roles[1]);
    w.ops.w().role_fetch_error = true;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", draft.id, w.roles[1]))).await;
    assert_eq!(w.last().content(), Some("I couldn't check that role right now. Please try again in a moment."));
    rr::idle(&w.t).await;
    assert_eq!(rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().unwrap().items.len(), 3);
    // a 10011 on the add is reported, then confirmed and pruned
    w.ops.w().role_fetch_error = false;
    w.ops.w().fail_role.insert(w.roles[2].clone(), 10011);
    let gone = w.roles[2].clone();
    route_interaction(&w.t, click(&w, &format!("rr:{}:{gone}", draft.id))).await;
    assert_eq!(w.last().content(), Some(format!("⚠️ I couldn't add <@&{gone}>: it no longer exists.").as_str()));
    w.done().await;
}

#[tokio::test]
async fn d11_selects_apply_the_selection_as_a_set() {
    let Some(w) = W::new(45).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "SELECT", Some(2), &four(None)).await;
    let (r0, r1, r2) = (w.roles[0].clone(), w.roles[1].clone(), w.roles[2].clone());
    w.ops.member(USER, &[&r0, &r1]);
    route_interaction(&w.t, choose(&w, &draft.id, &[&r0, &r2], false)).await;
    let last = w.last();
    assert_eq!(last.content(), Some(format!("✅ Added: <@&{r2}>\n➖ Removed: <@&{r1}>\n-# Your current roles from this panel are selected below. Change them there.").as_str()));
    let menu = &last.body["components"][0]["components"][0];
    assert_eq!(menu["custom_id"], json!(format!("rr-select:{}", draft.id)));
    assert_eq!(menu["placeholder"], json!("Adjust your roles from this panel"));
    let defaults: Vec<bool> = menu["options"].as_array().unwrap().iter().map(|o| o["default"].as_bool().unwrap_or(false)).collect();
    assert_eq!(defaults, vec![true, false, true, false]);
    // over the cap, part of the choice is refused
    route_interaction(&w.t, choose(&w, &draft.id, &[&r0, &r1, &r2], false)).await;
    assert_eq!(w.last().content(), Some("You can hold at most **2** role(s) from this panel. Remove one first.\n-# Your current roles from this panel are selected below. Change them there."));
    // an empty selection removes everything; a stranger value is refused
    route_interaction(&w.t, choose(&w, &draft.id, &[], false)).await;
    assert!(w.last().content().unwrap().starts_with("➖ Removed: "));
    route_interaction(&w.t, choose(&w, &draft.id, &["400000000000000001"], false)).await;
    assert_eq!(w.last().content(), Some("⚠️ That option is no longer on this panel."));
    w.done().await;
}

#[tokio::test]
async fn d12_the_personal_menu_edits_its_own_message() {
    let Some(w) = W::new(46).await else { return };
    w.t.install(registry());
    let draft = w.panel("UNIQUE", "SELECT", None, &four(None)).await;
    let (r0, r1) = (w.roles[0].clone(), w.roles[1].clone());
    w.ops.member(USER, &[&r0]);
    w.t.responses.clear();
    route_interaction(&w.t, choose(&w, &draft.id, &[&r1], true)).await;
    let calls = w.calls();
    assert_eq!(calls.iter().map(|c| c.method).collect::<Vec<_>>(), vec!["deferUpdate", "editReply"]);
    assert_eq!(calls[1].content(), Some(format!("✅ Added: <@&{r1}>\n➖ Removed: <@&{r0}>\n-# Your current roles from this panel are selected below. Change them there.").as_str()));
    assert_eq!(calls[1].body["components"][0]["components"][0]["max_values"], json!(1));
    w.done().await;
}

#[tokio::test]
async fn d13_handlers_never_throw() {
    let Some(w) = W::new(47).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &four(None)).await;
    // the member read fails: the interaction snapshot is used
    w.ops.w().member_fetch_error = true;
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", draft.id, w.roles[0]))).await;
    assert_eq!(w.last().content(), Some(format!("✅ Added: <@&{}>", w.roles[0]).as_str()));
    // a bad custom id
    route_interaction(&w.t, click(&w, "rr")).await;
    assert_eq!(w.last().content(), Some("⚠️ This panel is no longer active."));
    // an expired interaction answers nothing and does not fail
    w.t.responses.set_failing(true);
    route_interaction(&w.t, click(&w, &format!("rr:{}:{}", draft.id, w.roles[0]))).await;
    w.t.responses.set_failing(false);
    w.done().await;
}

// ── D14-D23: /reactionrole through the router ─────────────────────────────────

fn slash(w: &W, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("reactionrole").guild(&w.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD).subcommand(sub)
}

fn s(v: &str) -> Value {
    json!(v)
}

#[tokio::test]
async fn d14_create_buttons_templates_and_validation() {
    let Some(w) = W::new(48).await else { return };
    w.t.install(registry());
    let create = slash(&w, "create")
        .option("channel", option_type::CHANNEL, s(&w.channel))
        .option("title", option_type::STRING, s("Pings"))
        .option("role", option_type::ROLE, s(&w.roles[0]))
        .option("emoji", option_type::STRING, s("🔔"))
        .build();
    route_interaction(&w.t, create).await;
    let last = w.last();
    assert_eq!(last.embed_title(), Some("✅  Panel Created"), "{:?}", last.body);
    let sent = w.ops.w().sent.clone();
    assert_eq!(sent.len(), 1);
    let posted = &sent[0].1;
    assert_eq!(posted["embeds"][0]["title"], json!("Pings"));
    let panels = rr::list_panels(w.t.db(), &w.g).await.unwrap();
    assert_eq!(panels.len(), 1);
    assert_eq!(posted["components"][0]["components"][0]["custom_id"], json!(format!("rr:{}:{}", panels[0].id, w.roles[0])));
    assert!(last.embed_description().unwrap().starts_with("Panel **Pings** posted in <#"));
    assert!(last.embed_description().unwrap().contains(&format!("First role: <@&{}> 🔔.\nAdd more roles with `/reactionrole add`.", w.roles[0])));

    // the verify preset
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("template", option_type::STRING, s("verify")).option("role", option_type::ROLE, s(&w.roles[1])).build()).await;
    let body = w.ops.w().sent[1].1.clone();
    assert_eq!(body["embeds"][0]["title"], json!("✅ Verification"));
    assert_eq!(body["components"][0]["components"][0]["label"], json!("Verify"));
    assert_eq!(body["components"][0]["components"][0]["style"], json!(3));

    // validation
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("emoji", option_type::STRING, s("🔔")).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Missing Role"));
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("layout", option_type::STRING, s("REACTIONS")).option("role", option_type::ROLE, s(&w.roles[2])).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Emoji Needed"));
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("role", option_type::ROLE, s(&w.roles[2])).option("emoji", option_type::STRING, s("hello world")).build()).await;
    assert_eq!(w.last().embed_description(), Some("\"hello world\" is not an emoji."));
    route_interaction(&w.t, slash(&w, "create").build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Missing Channel"));
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("type", option_type::STRING, s("UNIQUE")).option("max_roles", option_type::INTEGER, json!(2)).option("placeholder", option_type::STRING, s("x")).build()).await;
    let d = w.last().embed_description().unwrap().to_string();
    assert!(d.ends_with("ℹ️ `max_roles` only applies to Normal panels, so it was ignored.\nℹ️ `placeholder` only applies to dropdown panels, so it was ignored."), "{d}");
    w.done().await;
}

#[tokio::test]
async fn d14b_create_from_a_role_template() {
    let Some(w) = W::new(49).await else { return };
    w.t.install(registry());
    w.ops.role(&w.g, "@everyone", 0);
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("template", option_type::STRING, s("dms")).option("role", option_type::ROLE, s(&w.roles[0])).build()).await;
    let last = w.last();
    assert_eq!(last.embed_title(), Some("✅  Template Panel Created"), "{:?}", last.body);
    let d = last.embed_description().unwrap();
    assert!(d.contains("Created 3 roles."), "{d}");
    assert!(d.contains("Remove the roles you do not need with `/reactionrole remove`."));
    assert!(d.ends_with("`role`, `emoji` and `max_roles` are ignored with a role template."));
    let body = w.ops.w().sent[0].1.clone();
    assert_eq!(body["embeds"][0]["title"], json!("✉️ DM Roles"));
    assert_eq!(body["components"][0]["components"].as_array().unwrap().len(), 3);
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("template", option_type::STRING, s("dms")).option("layout", option_type::STRING, s("REACTIONS")).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Pick Another Layout"));
    w.done().await;
}

#[tokio::test]
async fn d15_create_rollback_post_failure_and_the_module_warning() {
    let Some(w) = W::new(50).await else { return };
    w.t.install(registry());
    w.ops.w().send_error = Some(50013);
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).build()).await;
    assert_eq!(w.last().embed_description(), Some(format!("I couldn't post in <#{}>: I'm missing permissions there.", w.channel).as_str()));
    w.ops.w().send_error = None;
    w.t.settings().update_settings(&w.g, bot_core::settings::SettingsPatch::new().set("reactionRolesEnabled", false)).await.unwrap();
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).build()).await;
    assert!(w.last().embed_description().unwrap().ends_with("⚠️ The Reaction Roles module is off, so panels don't respond. Turn it on with `/module enable reactionroles`."));
    // a role above the bot
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).option("role", option_type::ROLE, s(ADMIN_ROLE)).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Role Not Allowed"));
    w.done().await;
}

#[tokio::test]
async fn d16_create_enforces_the_panel_cap() {
    let Some(w) = W::new(51).await else { return };
    w.t.install(registry());
    for _ in 0..bot_config::constants::limits::REACTION_ROLE_PANELS {
        w.panel("NORMAL", "BUTTONS", None, &[]).await;
    }
    route_interaction(&w.t, slash(&w, "create").option("channel", option_type::CHANNEL, s(&w.channel)).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Panel Limit Reached"));
    w.done().await;
}

#[tokio::test]
async fn d17_add_and_remove_on_buttons_and_reactions() {
    let Some(w) = W::new(52).await else { return };
    w.t.install(registry());
    let buttons = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    let add = |panel: &str, role: &str| slash(&w, "add").option("panel", option_type::STRING, s(panel)).option("role", option_type::ROLE, s(role));
    route_interaction(&w.t, add(&buttons.id, &w.roles[1]).option("style", option_type::STRING, s("Danger")).option("description", option_type::STRING, s("d")).build()).await;
    let last = w.last();
    assert_eq!(last.embed_title(), Some("✅  Role Added"));
    assert_eq!(last.embed_description(), Some(format!("<@&{}> added to **Panel**.\n\nℹ️ `description` only applies to dropdown panels, so it was ignored.", w.roles[1]).as_str()));
    assert_eq!(w.ops.w().edits.last().unwrap().1["components"][0]["components"][1]["style"], json!(4));
    route_interaction(&w.t, add(&buttons.id, &w.roles[1]).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Already Added"));
    route_interaction(&w.t, add(&bot_db::new_uuid(), &w.roles[1]).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Panel Not Found"));

    let reactions = w.panel("NORMAL", "REACTIONS", None, &[Some("🔔")]).await;
    route_interaction(&w.t, add(&reactions.id, &w.roles[1]).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Emoji Needed"));
    route_interaction(&w.t, add(&reactions.id, &w.roles[1]).option("emoji", option_type::STRING, s("🔔")).build()).await;
    assert_eq!(w.last().embed_description(), Some(format!("🔔 is already used for <@&{}> on this panel.", w.roles[0]).as_str()));
    route_interaction(&w.t, add(&reactions.id, &w.roles[1]).option("emoji", option_type::STRING, s("🎉")).build()).await;
    assert_eq!(w.last().embed_title(), Some("✅  Role Added"));
    assert!(w.ops.w().reacted.iter().any(|(m, e)| *m == reactions.message_id && e == "🎉"));
    // a reaction Discord refuses undoes the change
    w.ops.w().react_error = Some(30010);
    route_interaction(&w.t, add(&reactions.id, &w.roles[2]).option("emoji", option_type::STRING, s("🎮")).build()).await;
    assert_eq!(w.last().embed_description(), Some("that message already has 20 different reactions, so I undid the change."));
    assert_eq!(rr::get_panel(w.t.db(), &w.g, &reactions.id).await.unwrap().unwrap().items.len(), 2);
    w.ops.w().react_error = None;

    // remove
    let remove = |panel: &str, item: &str| slash(&w, "remove").option("panel", option_type::STRING, s(panel)).option("item", option_type::STRING, s(item)).build();
    route_interaction(&w.t, remove(&reactions.id, &w.roles[1])).await;
    assert_eq!(w.last().embed_description(), Some(format!("<@&{}> removed from **Panel**. Members who already have the role keep it.", w.roles[1]).as_str()));
    assert!(w.ops.w().removed_all.iter().any(|(m, k)| *m == reactions.message_id && k == "🎉"));
    route_interaction(&w.t, remove(&reactions.id, "nothing")).await;
    assert_eq!(w.last().embed_title(), Some("❌  Not On Panel"));
    w.ops.w().roles.retain(|r| r.id != w.roles[0]);
    route_interaction(&w.t, remove(&buttons.id, "Role 1")).await;
    assert!(w.last().embed_description().unwrap().starts_with("**Role 1** (deleted role) removed from **Panel**."));
    w.done().await;
}

#[tokio::test]
async fn d18_add_and_edit_report_a_deleted_panel_message() {
    let Some(w) = W::new(53).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    w.ops.w().messages.clear();
    route_interaction(&w.t, slash(&w, "add").option("panel", option_type::STRING, s(&draft.id)).option("role", option_type::ROLE, s(&w.roles[1])).build()).await;
    assert!(w.last().embed_description().unwrap().ends_with("⚠️ The panel message no longer exists. Run `/reactionrole repost panel:Panel` to post it again."));
    route_interaction(&w.t, slash(&w, "edit").option("panel", option_type::STRING, s(&draft.id)).option("title", option_type::STRING, s("T2")).build()).await;
    assert!(w.last().embed_description().unwrap().ends_with("⚠️ The panel message no longer exists. Run `/reactionrole repost panel:T2` to post it again."));
    w.done().await;
}

#[tokio::test]
async fn d19_edit() {
    let Some(w) = W::new(54).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", Some(2), &[None]).await;
    let edit = || slash(&w, "edit").option("panel", option_type::STRING, s(&draft.id));
    route_interaction(&w.t, edit().build()).await;
    assert_eq!(w.last().embed_description(), Some("Give at least one option to change."));
    route_interaction(&w.t, edit().option("placeholder", option_type::STRING, s("x")).build()).await;
    assert_eq!(w.last().embed_description(), Some("None of those options apply to this panel.\n\nℹ️ `placeholder` only applies to dropdown panels, so it was ignored."));
    route_interaction(&w.t, edit().option("title", option_type::STRING, s(" New ")).option("description", option_type::STRING, s("-")).option("type", option_type::STRING, s("UNIQUE")).option("max_roles", option_type::INTEGER, json!(3)).build()).await;
    assert_eq!(w.last().embed_description(), Some("**New** updated: title, description, type.\n\nℹ️ `max_roles` only applies to Normal panels, so it was ignored."));
    let p = rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().unwrap();
    assert_eq!((p.group.title.as_str(), p.group.description.clone(), p.kind(), p.group.max_roles), ("New", None, "UNIQUE", None));
    route_interaction(&w.t, edit().option("type", option_type::STRING, s("NORMAL")).option("max_roles", option_type::INTEGER, json!(0)).build()).await;
    assert_eq!(w.last().embed_description(), Some("**New** updated: type, max_roles."));
    w.done().await;
}

#[tokio::test]
async fn d20_attach_binds_an_existing_message_and_never_edits_it() {
    let Some(w) = W::new(55).await else { return };
    w.t.install(registry());
    let mid = "500000000000550001";
    w.ops.message(&w.g, &w.channel, mid);
    w.ops.w().messages.get_mut(mid).unwrap().content = "**Rules** here".into();
    let link = format!("https://discord.com/channels/{}/{}/{mid}", w.g, w.channel);
    route_interaction(&w.t, slash(&w, "attach").option("message_link", option_type::STRING, s(&link)).option("type", option_type::STRING, s("UNIQUE")).build()).await;
    let last = w.last();
    assert_eq!(last.embed_title(), Some("✅  Panel Attached"), "{:?}", last.body);
    assert!(last.embed_description().unwrap().starts_with(&format!("**Rules here** is now an emoji-reaction panel on [this message]({link}).")));
    let listed = rr::list_panels(w.t.db(), &w.g).await.unwrap();
    assert_eq!((listed[0].attached, listed[0].layout.as_str(), listed[0].title.as_str()), (true, "REACTIONS", "Rules here"));
    route_interaction(&w.t, slash(&w, "attach").option("message_link", option_type::STRING, s(&link)).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Already a Panel"));
    route_interaction(&w.t, slash(&w, "attach").option("message_link", option_type::STRING, s("hello")).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Invalid Message"));
    route_interaction(&w.t, slash(&w, "attach").option("message_link", option_type::STRING, s(&format!("{}-500000000000559999", w.channel))).build()).await;
    assert_eq!(w.last().embed_title(), Some("❌  Message Not Found"));
    // adding a role reacts but never edits the message
    route_interaction(&w.t, slash(&w, "add").option("panel", option_type::STRING, s(&listed[0].id)).option("role", option_type::ROLE, s(&w.roles[0])).option("emoji", option_type::STRING, s("✅")).build()).await;
    assert_eq!(w.last().embed_title(), Some("✅  Role Added"));
    assert!(w.ops.w().edits.is_empty());
    assert_eq!(w.ops.w().reacted, vec![(mid.to_string(), "✅".to_string())]);
    w.done().await;
}

#[tokio::test]
async fn d21_repost_converts_an_attached_panel() {
    let Some(w) = W::new(56).await else { return };
    w.t.install(registry());
    let mut draft = w.panel("NORMAL", "REACTIONS", None, &[Some("🔔")]).await;
    rr::move_panel(w.t.db(), &w.g, &draft.id, &w.channel, &draft.message_id, true).await.unwrap();
    draft.attached = true;
    // restore only: the reactions come back
    route_interaction(&w.t, slash(&w, "repost").option("panel", option_type::STRING, s(&draft.id)).build()).await;
    assert_eq!(w.last().embed_title(), Some("✅  Reactions Restored"));
    assert_eq!(w.ops.w().reacted, vec![(draft.message_id.clone(), "🔔".to_string())]);
    // the message is gone: the bot posts its own
    w.ops.w().messages.remove(&draft.message_id);
    route_interaction(&w.t, slash(&w, "repost").option("panel", option_type::STRING, s(&draft.id)).build()).await;
    let last = w.last();
    assert_eq!(last.embed_title(), Some("✅  Panel Reposted"), "{:?}", last.body);
    assert!(last.embed_description().unwrap().contains("It's now a message I posted, so `/reactionrole edit` can change its text."));
    let p = rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().unwrap();
    assert!(!p.group.attached && p.group.message_missing_at.is_none() && p.group.message_id != draft.message_id);
    w.done().await;
}

async fn wait_collector(t: &DbTestApp) {
    for _ in 0..200 {
        if !t.collectors().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no collector");
}

fn nonce_of(call: &bot_core::interaction::RecordedCall) -> String {
    let id = call.body["components"][0]["components"][0]["custom_id"].as_str().unwrap().to_string();
    id.split(':').nth(1).unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn d22_delete_and_list() {
    let Some(w) = W::new(57).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None, None]).await;
    let app = w.t.app.clone();
    let del = slash(&w, "delete").option("panel", option_type::STRING, s(&draft.id)).build();
    let run = tokio::spawn(async move { route_interaction(&app, del).await });
    wait_collector(&w.t).await;
    let prompt = w.calls().into_iter().rev().find(|c| c.body["components"].is_array() && !c.body["components"].as_array().unwrap().is_empty()).unwrap();
    assert_eq!(w.calls()[0].method, "deferReply");
    assert!(w.calls()[0].ephemeral(), "the delete prompt is private");
    assert_eq!(prompt.embed_title(), Some("⚠️  Delete this panel?"));
    assert_eq!(prompt.embed_description(), Some("**Panel** and its 2 role(s) will be removed, and I'll delete the panel message. Members keep the roles they already have."));
    let yes = FakeInteraction::button(&format!("confirm:{}:yes", nonce_of(&prompt))).guild(&w.g).user(ADMIN).message_id("1000000000000000100").build();
    route_interaction(&w.t, yes).await;
    run.await.unwrap();
    let last = w.last();
    assert_eq!(last.method, "followUp");
    assert_eq!(last.embed_description(), Some("**Panel** was deleted, along with its message."));
    assert!(rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().is_none());
    assert_eq!(w.ops.w().deleted, vec![draft.message_id.clone()]);

    route_interaction(&w.t, slash(&w, "list").build()).await;
    assert_eq!(w.last().embed_title(), Some("ℹ️  No Panels"));
    w.panel("UNIQUE", "SELECT", Some(3), &[None]).await;
    route_interaction(&w.t, slash(&w, "list").build()).await;
    let page = w.calls().into_iter().rev().find(|c| c.embed_title() == Some("ℹ️  Reaction Role Panels (1)")).expect("a list page");
    assert!(page.body["embeds"][0]["fields"][0]["value"].as_str().unwrap().starts_with("Unique · Dropdown · 1 role(s)"));
    w.done().await;
}

#[tokio::test]
async fn d23_autocomplete_panels_and_items() {
    let Some(w) = W::new(58).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &[Some("🔔"), None]).await;
    let ac = FakeInteraction::autocomplete("reactionrole").guild(&w.g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD).subcommand("remove").focused("panel", "pan").build();
    route_interaction(&w.t, ac).await;
    let choices = w.last().choices();
    assert_eq!(choices, vec![json!({ "name": "Panel · Buttons · 2 role(s)", "value": draft.id })]);
    let ac = FakeInteraction::autocomplete("reactionrole")
        .guild(&w.g)
        .user(ADMIN)
        .roles(&[ADMIN_ROLE])
        .permissions(P::MANAGE_GUILD)
        .subcommand("remove")
        .option("panel", option_type::STRING, s(&draft.id))
        .focused("item", "role 2")
        .build();
    route_interaction(&w.t, ac).await;
    let choices = w.last().choices();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0]["name"], json!("Role 2 · @Role 2"));
    w.done().await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn js_writes_rust_reads_rows_and_the_cached_index() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else {
        eprintln!("skipped: TEST_DATABASE_URL / TEST_REDIS_URL not set");
        return;
    };
    let gid = g(60);
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([gid.as_str()]);
    t.reset_guilds(&[&gid]).await;
    t.cache().del_one(&rr::index_key(&gid)).await;
    let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["seed", &gid]);
    assert!(js.get("error").is_none(), "{js}");
    let ids = js["ids"].as_array().unwrap();
    for (i, id) in ids.iter().enumerate() {
        let rust = rr::get_panel(t.db(), &gid, id.as_str().unwrap()).await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(&rust).unwrap(), js["panels"][i], "getPanel");
    }
    assert_eq!(serde_json::to_value(rr::list_panels(t.db(), &gid).await.unwrap()).unwrap(), js["list"]);
    // the index the JS cached in Redis is read as is (no Postgres), and written back JSON-equal
    let raw: Value = serde_json::from_str(js["cacheRaw"].as_str().unwrap()).unwrap();
    let parsed = rr::parse_cached_index(&raw).expect("the JS value reads");
    assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
    sqlx::query(r#"UPDATE "ReactionRoleGroup" SET "title" = 'changed' WHERE "guildId" = $1"#).bind(&gid).execute(t.db()).await.unwrap();
    let index = rr::get_panel_index(&t, &gid).await.unwrap();
    assert!(index.values().all(|p| p.title.starts_with("JS ")), "served from the JS cache entry");
    let mut values: Vec<Value> = index.values().map(|p| serde_json::to_value(p).unwrap()).collect();
    values.sort_by_key(|v| v["id"].as_str().unwrap().to_string());
    let mut want = js["index"].as_array().unwrap().clone();
    want.sort_by_key(|v| v["id"].as_str().unwrap().to_string());
    assert_eq!(values, want);
    t.cache().del_one(&rr::index_key(&gid)).await;
    t.reset_guilds(&[&gid]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_writes_js_reads_rows_and_the_cached_index() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else {
        eprintln!("skipped: TEST_DATABASE_URL / TEST_REDIS_URL not set");
        return;
    };
    let gid = g(61);
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([gid.as_str()]);
    t.reset_guilds(&[&gid]).await;
    t.settings().ensure_guild_settings(&gid).await.unwrap();
    let id = bot_db::new_uuid();
    let items = vec![
        rr::draft_item(&id, &NewItem { role_id: "400000000000000201".into(), label: "One".into(), emoji: Some("❤️".into()), style: Some(2), description: Some("d".into()) }, 0).unwrap(),
        rr::draft_item(&id, &NewItem { role_id: "400000000000000202".into(), label: "Two".into(), emoji: Some("<a:wave:123456789012345678>".into()), ..Default::default() }, 1).unwrap(),
    ];
    let draft = PanelDraft {
        id: id.clone(),
        guild_id: gid.clone(),
        channel_id: "300000000000000201".into(),
        message_id: "500000000000000201".into(),
        title: "Rust panel".into(),
        description: Some("Desc".into()),
        kind: "REVERSE".into(),
        layout: "REACTIONS".into(),
        max_roles: None,
        plain_text: true,
        message_content: Some("Hi".into()),
        attached: false,
        placeholder: None,
        items,
    };
    rr::create_panel(t.db(), &draft).await.unwrap();
    let p = rr::get_panel(t.db(), &gid, &id).await.unwrap().unwrap();
    rr::add_item(t.db(), &id, &p.items, &NewItem { role_id: "400000000000000203".into(), label: "Three".into(), ..Default::default() }).await.unwrap();
    rr::remove_item(t.db(), &id, &p.items[0].id).await.unwrap();
    rr::update_panel(t.db(), &gid, &id, &PanelPatch { title: Some("Renamed".into()), max_roles: Some(Some(4)), ..Default::default() }).await.unwrap();
    rr::mark_panel_missing(&t, &gid, &id).await;
    let rust = rr::get_panel(t.db(), &gid, &id).await.unwrap().unwrap();
    let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["get", &gid, &id]);
    assert_eq!(serde_json::to_value(&rust).unwrap(), js, "the JS getPanel reads what Rust wrote");
    assert_eq!(rust.items.iter().map(|i| i.position).collect::<Vec<_>>(), vec![0, 1]);

    // Rust caches the index; the JS process reads it from Redis (the rows changed underneath, the cache wins)
    rr::invalidate_panel_index(&t, &gid).await;
    let index = rr::get_panel_index(&t, &gid).await.unwrap();
    sqlx::query(r#"UPDATE "ReactionRoleGroup" SET "title" = 'changed' WHERE "guildId" = $1"#).bind(&gid).execute(t.db()).await.unwrap();
    let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["index", &gid]);
    let want: Vec<Value> = index.values().map(|p| serde_json::to_value(p).unwrap()).collect();
    assert_eq!(js, Value::Array(want));
    assert_eq!(js[0]["title"], json!("Renamed"));
    assert_eq!(js[0]["items"][0]["key"], json!("123456789012345678"));
    t.cache().del_one(&rr::index_key(&gid)).await;
    t.reset_guilds(&[&gid]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rate_limit_window_is_shared_with_the_js() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else {
        eprintln!("skipped: TEST_DATABASE_URL / TEST_REDIS_URL not set");
        return;
    };
    let gid = g(62);
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([gid.as_str()]);
    let user = "700000000000000062";
    let other = "700000000000000063";
    t.cache().del_one(&rr::rate_key(&gid, user)).await;
    t.cache().del_one(&rr::rate_key(&gid, other)).await;
    // eight changes by the JS bot (its processReaction reaches the member read each time: under the limit)
    let js = run_node("rust/fixtures/gen/feat-reactionroles-js.mjs", &["rate", &gid, user, "8"]);
    assert_eq!(js["members"], json!(8), "{js}");
    // the ninth, in Rust, is over RATE_LIMIT (8 in 10 s): the same key and the same window
    assert!(rr::over_rate_limit(&t, &gid, user).await);
    assert!(!rr::over_rate_limit(&t, &gid, other).await, "another member has their own window");
    t.cache().del_one(&rr::rate_key(&gid, user)).await;
    t.cache().del_one(&rr::rate_key(&gid, other)).await;
}

#[tokio::test]
async fn d17b_add_a_role_that_new_role_just_created() {
    let Some(w) = W::new(59).await else { return };
    w.t.install(registry());
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    // created by new_role: in the resolved data, not yet in serenity's cache
    let fresh = "400000000000590099";
    let mut f = slash(&w, "add").option("panel", option_type::STRING, s(&draft.id)).option("role", option_type::ROLE, s(fresh));
    f.json_mut()["data"]["resolved"] = json!({ "roles": { fresh: {
        "id": fresh, "name": "Fresh", "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false, "mentionable": false, "flags": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    } } });
    route_interaction(&w.t, f.build()).await;
    let last = w.last();
    assert_eq!(last.embed_title(), Some("✅  Role Added"), "{:?}", last.body);
    let p = rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().unwrap();
    assert_eq!(p.items[1].label, "Fresh");
    w.done().await;
}

/// A stored description of `''` on a button panel with roles: the JS `buildPanelPayload` throws (discord.js refuses an
/// empty embed description, with no numeric code), so `syncPanel` is 'failed' with "Discord error unknown" and nothing
/// is sent; `/reactionrole add` then only warns instead of undoing the change on a 50035.
#[tokio::test]
async fn an_empty_description_fails_the_sync_before_anything_is_sent() {
    let Some(w) = W::new(60).await else { return };
    let draft = w.panel("NORMAL", "BUTTONS", None, &[None]).await;
    let patch = rr::PanelPatch { description: Some(Some(String::new())), ..Default::default() };
    rr::update_panel(w.t.db(), &w.g, &draft.id, &patch).await.unwrap().unwrap();
    let edits_before = w.ops.w().edits.len();
    assert_eq!(
        rr::sync_panel(&w.t, &w.g, PanelArg::Id(draft.id.clone()), None).await,
        rr::SyncResult { status: "failed", code: None, reason: Some("Discord error unknown".into()) }
    );
    assert_eq!(w.ops.w().edits.len(), edits_before, "nothing was sent");
    // A reaction panel lists its roles, and a panel without roles shows the empty-panel line: both still render.
    let view = rr::get_panel(w.t.db(), &w.g, &draft.id).await.unwrap().unwrap().view();
    assert!(rr::panel_payload_throws(&view));
    assert!(!rr::panel_payload_throws(&rr::PanelView { layout: "REACTIONS".into(), ..view.clone() }));
    assert!(!rr::panel_payload_throws(&rr::PanelView { items: Vec::new(), ..view.clone() }));
    assert!(!rr::panel_payload_throws(&rr::PanelView { plain_text: true, ..view }));
    w.done().await;
}
