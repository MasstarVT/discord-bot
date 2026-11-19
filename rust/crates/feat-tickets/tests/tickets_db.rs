//! Database tests of feat-tickets (`TEST_DATABASE_URL`): the DB cases of `test/tickets.test.js` with a fake Discord
//! (`common::FakeIo`) around the real service and the mock PostgreSQL, the flows through the router, and both
//! cross-language directions (`rust/fixtures/gen/feat-tickets-js.mjs`).

mod common;

use std::sync::{Arc, Mutex};

use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, database_url, offline_discord_as, run_node};
use bot_db::sqlx;
use bot_db::{TicketPanelStyle, TicketStatus};
use common::FakeIo;
use feat_tickets::ticket_service as svc;
use serde_json::{Value, json};
use serenity::all::{GuildId, Member, Permissions, UserId};

const OWNER: &str = "955020000000000900";
const ADMIN_ROLE: &str = "955020000000000201";
const STAFF_ROLE: &str = "955020000000000202";
const ADMIN: &str = "955020000000000301";
const STAFF: &str = "955020000000000302";
const ALICE: &str = "955020000000000303";
const BOB: &str = "955020000000000304";
const CAROL: &str = "955020000000000305";
const STAFF2: &str = "955020000000000306";
const MGUILD: &str = "955020000000000307";
/// A manager role nobody may ping (not mentionable).
const MGR2: &str = "955020000000000204";
/// Manage Server without Mention Everyone: the Admin level, but no right to ping unmentionable roles.
const MGUILD_ROLE: &str = "955020000000000205";

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_tickets::register(&mut reg);
    reg
}

/// JSON without the defaults Discord fills in (`animated: false`, `disabled: false`).
fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter().filter(|(k, x)| !((k.as_str() == "animated" || k.as_str() == "disabled") && **x == json!(false))).map(|(k, x)| (k.clone(), norm(x))).collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        x => x.clone(),
    }
}

fn gid(g: &str) -> GuildId {
    GuildId::new(g.parse().unwrap())
}

/// A migrated database, a cached fake guild (the bot is an administrator) and the fake Discord seam. `None` without
/// `TEST_DATABASE_URL`.
async fn setup(g: &str, base: u64, text_channels: &[&str]) -> Option<(DbTestApp, Arc<FakeIo>)> {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return None;
    };
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let mut guild = FakeGuild::new(g, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .role(ADMIN_ROLE, 3, Permissions::ADMINISTRATOR)
        .role_with(STAFF_ROLE, 2, Permissions::empty(), |v| v["mentionable"] = json!(true))
        .member(TEST_BOT_ID, &[ADMIN_ROLE])
        .member(ADMIN, &[ADMIN_ROLE])
        .member(STAFF, &[STAFF_ROLE])
        .member(ALICE, &[])
        .member(BOB, &[])
        .member(CAROL, &[])
        .role(MGR2, 1, Permissions::empty())
        .role(MGUILD_ROLE, 1, Permissions::MANAGE_GUILD)
        .member(STAFF2, &[STAFF_ROLE])
        .member(MGUILD, &[MGUILD_ROLE]);
    for (i, c) in text_channels.iter().enumerate() {
        guild = guild.channel(c, 0, &format!("text-{i}"), &[]);
    }
    t.set_discord(offline_discord_as(&[guild.json()]));
    t.install(registry());
    let io = FakeIo::new(&t, base);
    svc::set_delete_delay_ms(&t, 5);
    Some((t, io))
}

fn member(t: &DbTestApp, g: &str, user: &str) -> Member {
    cached_member(t, g, user)
}

fn cached_member(t: &DbTestApp, g: &str, user: &str) -> Member {
    let guild = t.discord().unwrap().cache.guild(gid(g)).unwrap();
    guild.members.get(&UserId::new(user.parse().unwrap())).unwrap().clone()
}

fn actx(t: &DbTestApp, g: &str, ticket_id: &str, user: &str, source: svc::Source) -> svc::ActionCtx {
    svc::ActionCtx { guild_id: gid(g), ticket_id: ticket_id.into(), actor: svc::Actor::of(t, gid(g), &member(t, g, user)), settings: None, source }
}

fn opener(t: &DbTestApp, g: &str, user: &str, n: u64) -> svc::Opener {
    let m = member(t, g, user);
    svc::Opener { interaction_id: format!("{g}{n}"), user: m.user.clone(), member: Some(m) }
}

async fn new_panel(t: &DbTestApp, g: &str, name: &str, managers: bool) -> svc::PanelWithOptions {
    let p = svc::create_panel(t, svc::NewPanel::new(g, name)).await.unwrap().unwrap();
    if managers {
        svc::update_managers(t, &p.panel.id, gid(g), |cur| Ok([cur, vec![STAFF_ROLE.to_string()]].concat())).await.unwrap().unwrap();
    }
    svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap()
}

async fn ticket_row(t: &DbTestApp, g: &str, id: &str) -> svc::TicketFull {
    svc::get_ticket_by_id(t.db(), g, id).await.unwrap().unwrap()
}

async fn open(t: &DbTestApp, g: &str, panel: &svc::PanelWithOptions, user: &str, n: u64) -> svc::Opened {
    svc::open_ticket(t, gid(g), &opener(t, g, user, n), panel, panel.options.first(), None, None).await.unwrap()
}

// ── RAW_SQL ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_raw_sql_statement_executes() {
    let Some((t, _io)) = setup("955020000000001001", 960_001_000_000_000_000, &[]).await else { return };
    for (name, sql, params) in svc::RAW_SQL {
        let mut tx = t.pool.begin().await.unwrap();
        let mut q = sqlx::query(sql);
        for p in params {
            q = q.bind(*p);
        }
        q.fetch_all(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.unwrap();
    }
}

// ── Panels, options, managers ─────────────────────────────────────────────────

#[tokio::test]
async fn create_panel_enforces_the_name_and_the_cap_under_parallel_creates() {
    let g = "955020000000001002";
    let Some((t, _io)) = setup(g, 960_002_000_000_000_000, &[]).await else { return };
    let first = svc::create_panel(&t, svc::NewPanel::new(g, "Support")).await.unwrap().unwrap();
    assert_eq!(first.options.len(), 1);
    assert_eq!(first.options[0].label, "Open Ticket");
    assert_eq!(first.options[0].emoji.as_deref(), Some("✉️"));
    let dup = svc::create_panel(&t, svc::NewPanel::new(g, "SUPPORT")).await.unwrap().unwrap_err();
    assert_eq!(dup.code, "NAME_TAKEN");

    let mut handles = Vec::new();
    for i in 0..30 {
        let app = t.app.clone();
        handles.push(tokio::spawn(async move { svc::create_panel(&app, svc::NewPanel::new(g, &format!("Panel {i}"))).await.unwrap() }));
    }
    let mut ok = 0;
    let mut limit = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(f) => {
                assert_eq!(f.code, "PANEL_LIMIT");
                limit += 1;
            }
        }
    }
    assert_eq!((ok, limit), (24, 6));
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "TicketPanel" WHERE "guildId" = $1"#).bind(g).fetch_one(&t.pool).await.unwrap();
    assert_eq!(count, 25);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn update_fields_find_panel_and_autocomplete() {
    let g = "955020000000001003";
    let Some((t, _io)) = setup(g, 960_003_000_000_000_000, &[]).await else { return };
    let a = new_panel(&t, g, "Support", false).await;
    let b = new_panel(&t, g, "Billing Desk", false).await;
    let clash = svc::update_panel_fields(&t, &b.panel.id, g, &[("name", json!("support"))]).await.unwrap().unwrap_err();
    assert_eq!(clash.code, "NAME_TAKEN");
    assert_eq!(clash.data, json!({ "name": "support" }));
    svc::update_panel_fields(&t, &a.panel.id, g, &[("style", json!("SELECT")), ("maxOpenPerUser", json!(3))]).await.unwrap().unwrap();
    let missing = svc::update_panel_fields(&t, "nope", g, &[("askReason", json!(true))]).await.unwrap().unwrap_err();
    assert_eq!(missing.code, "PANEL_NOT_FOUND");

    let by_name = svc::find_panel(t.db(), g, "SUPPORT").await.unwrap().unwrap();
    assert_eq!(by_name.panel.id, a.panel.id);
    assert_eq!(by_name.panel.style, TicketPanelStyle::Select);
    assert_eq!(by_name.panel.max_open_per_user, 3);
    assert!(svc::find_panel(t.db(), "955020000000009999", &a.panel.id).await.unwrap().is_none(), "guild-scoped");

    let all = svc::find_panels_for_autocomplete(t.db(), g, "").await.unwrap();
    assert_eq!(all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["Billing Desk", "Support"]);
    let some = svc::find_panels_for_autocomplete(t.db(), g, "desk").await.unwrap();
    assert_eq!(some.len(), 1);
    svc::add_option(&t, &a.panel.id, g, svc::NewOption { label: "Bug Report".into(), button_style: 1, ..Default::default() }).await.unwrap().unwrap();
    let opts = svc::find_options_for_autocomplete(t.db(), g, "support", "bug").await.unwrap();
    assert_eq!(opts.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), vec!["Bug Report"]);
    let opts = svc::find_options_for_autocomplete(t.db(), g, &a.panel.id, "").await.unwrap();
    assert_eq!(opts.len(), 2);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn options_are_capped_unique_and_ordered_under_their_lock() {
    let g = "955020000000001004";
    let Some((t, _io)) = setup(g, 960_004_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let mut handles = Vec::new();
    for i in 0..26 {
        let (app, pid) = (t.app.clone(), p.panel.id.clone());
        handles.push(tokio::spawn(async move {
            svc::add_option(&app, &pid, g, svc::NewOption { label: format!("Topic {i}"), button_style: 2, ..Default::default() }).await.unwrap()
        }));
    }
    let results: Vec<_> = futures_join(handles).await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 24);
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|f| f.code == "OPTION_LIMIT"));
    let panel = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    let positions: Vec<i32> = panel.options.iter().map(|o| o.position).collect();
    let mut sorted = positions.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 25, "distinct positions: {positions:?}");

    let p2 = new_panel(&t, g, "Other", false).await;
    let added = svc::add_option(&t, &p2.panel.id, g, svc::NewOption { label: "Billing".into(), button_style: 1, ..Default::default() }).await.unwrap().unwrap();
    assert_eq!(added.1, 2);
    assert_eq!(added.0.position, 1);
    let dup = svc::add_option(&t, &p2.panel.id, g, svc::NewOption { label: "billing".into(), button_style: 1, ..Default::default() }).await.unwrap().unwrap_err();
    assert_eq!((dup.code.as_str(), dup.data.clone()), ("OPTION_DUP", json!({ "label": "billing" })));
    let first = p2.options[0].id.clone();
    let dup = svc::update_option(&t, &p2.panel.id, g, &first, vec![("label", json!("BILLING"))]).await.unwrap().unwrap_err();
    assert_eq!(dup.code, "OPTION_DUP");
    let renamed = svc::update_option(&t, &p2.panel.id, g, &first, vec![("label", json!("General")), ("emoji", Value::Null)]).await.unwrap().unwrap();
    assert_eq!((renamed.label.as_str(), renamed.emoji.as_deref()), ("General", None));
    svc::remove_option(&t, &p2.panel.id, g, &added.0.id).await.unwrap().unwrap();
    let last = svc::remove_option(&t, &p2.panel.id, g, &first).await.unwrap().unwrap_err();
    assert_eq!(last.code, "LAST_OPTION");
    let gone = svc::remove_option(&t, &p2.panel.id, g, "nope").await.unwrap().unwrap_err();
    assert_eq!(gone.code, "OPTION_NOT_FOUND");
    t.reset_guilds(&[g]).await;
}

async fn futures_join<T: Send + 'static>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

#[tokio::test]
async fn manager_roles_are_added_removed_capped_and_pruned() {
    let g = "955020000000001005";
    let Some((t, _io)) = setup(g, 960_005_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    // A role that no longer exists is pruned before the change.
    sqlx::query(r#"UPDATE "TicketPanel" SET "managerRoleIds" = $1 WHERE "id" = $2"#)
        .bind(vec!["955020000000000999".to_string()])
        .bind(&p.panel.id)
        .execute(&t.pool)
        .await
        .unwrap();
    let r = svc::update_managers(&t, &p.panel.id, gid(g), |cur| Ok([cur, vec![STAFF_ROLE.to_string()]].concat())).await.unwrap().unwrap();
    assert_eq!(r, vec![STAFF_ROLE.to_string()]);
    let again = svc::update_managers(&t, &p.panel.id, gid(g), |cur| {
        if cur.contains(&STAFF_ROLE.to_string()) { Err(svc::Fail::new("ALREADY_MANAGER")) } else { Ok(cur) }
    })
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(again.code, "ALREADY_MANAGER");
    let missing = svc::update_managers(&t, "nope", gid(g), Ok).await.unwrap().unwrap_err();
    assert_eq!(missing.code, "PANEL_NOT_FOUND");
    let r = svc::update_managers(&t, &p.panel.id, gid(g), |cur| Ok(cur.into_iter().filter(|r| r != STAFF_ROLE).collect())).await.unwrap().unwrap();
    assert!(r.is_empty());
    t.reset_guilds(&[g]).await;
}

// ── Opening ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn opening_creates_a_private_numbered_channel_in_an_on_demand_category() {
    let g = "955020000000001006";
    let Some((t, io)) = setup(g, 960_006_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = svc::open_ticket(&t, gid(g), &opener(&t, g, ALICE, 1), &p, p.options.first(), Some("my printer is on fire".into()), None).await.unwrap();
    assert_eq!(o.ticket.number, 1);
    assert_eq!(o.channel.name, "ticket-0001");
    assert!(o.notes.is_empty(), "{:?}", o.notes);
    let created = io.w().created.clone();
    assert_eq!(created[0].name, "Tickets");
    assert_eq!(created[0].kind, 4);
    let spec = &created[1];
    let overwrites: Vec<Value> = spec.overwrites.iter().map(svc::OverwriteSpec::to_json).collect();
    assert_eq!(overwrites[0], json!({ "id": g, "type": 0, "deny": ["ViewChannel"] }));
    assert_eq!(overwrites[1]["id"], json!(TEST_BOT_ID));
    assert_eq!(overwrites[2]["id"], json!(ALICE));
    assert_eq!(overwrites[3], json!({ "id": STAFF_ROLE, "type": 0, "allow": svc::STAFF_ALLOW }));
    assert!(spec.topic.as_deref().unwrap().starts_with(&format!("Ticket #0001 · opened by user{ALICE} ({ALICE}) · Open Ticket")));
    assert_eq!(spec.parent.map(|c| c.to_string()), Some(created_category(&t, g, &p.panel.id).await));

    let intro = &io.sent(o.channel.id)[0];
    assert_eq!(intro["content"], json!(format!("<@{ALICE}>")));
    assert_eq!(intro["embeds"][0]["title"], json!("Ticket #0001"));
    assert_eq!(intro["embeds"][0]["fields"][0], json!({ "name": "Reason", "value": "my printer is on fire", "inline": false }));
    assert_eq!(intro["components"][0]["components"][0]["custom_id"], json!(format!("ticket:close:{}", o.ticket.id)));
    assert_eq!(intro["allowedMentions"]["users"], json!([ALICE]));
    let row = ticket_row(&t, g, &o.ticket.id).await;
    assert_eq!(row.ticket.channel_id, o.channel.id.to_string());
    assert_eq!(row.ticket.reason.as_deref(), Some("my printer is on fire"));

    // The member's cap (1): a second click is refused with a link to the open ticket.
    let r = svc::open_ticket(&t, gid(g), &opener(&t, g, ALICE, 2), &p, p.options.first(), None, None).await.unwrap_err();
    assert_eq!(r.code, "THROTTLED");
    t.reset_guilds(&[g]).await;
}

async fn created_category(t: &DbTestApp, g: &str, panel_id: &str) -> String {
    svc::find_panel(t.db(), g, panel_id).await.unwrap().unwrap().panel.open_category_id.unwrap()
}

#[tokio::test]
async fn parallel_opens_by_different_members_get_distinct_numbers() {
    let g = "955020000000001007";
    let Some((t, _io)) = setup(g, 960_007_000_000_000_000, &[]).await else { return };
    let p = Arc::new(new_panel(&t, g, "Support", false).await);
    let mut handles = Vec::new();
    for (i, u) in [ALICE, BOB, CAROL, STAFF].into_iter().enumerate() {
        let (app, p, op) = (t.app.clone(), p.clone(), opener(&t, g, u, 10 + i as u64));
        handles.push(tokio::spawn(async move { svc::open_ticket(&app, gid(g), &op, &p, p.options.first(), None, None).await.unwrap().ticket.number }));
    }
    let mut numbers = futures_join(handles).await;
    numbers.sort();
    assert_eq!(numbers, vec![1, 2, 3, 4]);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn a_cache_miss_is_not_a_deletion_only_a_confirmed_10003_frees_a_slot() {
    let g = "955020000000001008";
    let Some((t, _io)) = setup(g, 960_008_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let first = open(&t, g, &p, ALICE, 1).await;

    // A second app (a fresh open throttle) whose cache does not hold the ticket channel.
    let url = database_url().unwrap();
    let t2 = DbTestApp::new(&url).await.with_guilds([g]);
    let guild = FakeGuild::new(g, OWNER).role(ADMIN_ROLE, 3, Permissions::ADMINISTRATOR).member(TEST_BOT_ID, &[ADMIN_ROLE]).member(ALICE, &[]);
    t2.set_discord(offline_discord_as(&[guild.json()]));
    let io2 = FakeIo::new(&t2, 960_008_500_000_000_000);
    io2.w().fetch_error = Some(svc::IoError::timed_out("fetch"));
    let m = cached_member(&t2, g, ALICE);
    let op = svc::Opener { interaction_id: "x1".into(), user: m.user.clone(), member: Some(m.clone()) };
    let r = svc::open_ticket(&t2, gid(g), &op, &p, p.options.first(), None, None).await.unwrap_err();
    assert_eq!(r.code, "OPEN_LIMIT", "an unknown channel still counts");
    assert_eq!(r.data["ids"], json!([first.channel.id.to_string()]));
    assert_eq!(ticket_row(&t, g, &first.ticket.id).await.status(), "OPEN");

    // Discord confirms the channel is gone: the row is written off and the member may open again.
    let t3 = DbTestApp::new(&url).await.with_guilds([g]);
    t3.set_discord(offline_discord_as(&[guild.json()]));
    let _io3 = FakeIo::new(&t3, 960_008_600_000_000_000);
    let m = cached_member(&t3, g, ALICE);
    let op = svc::Opener { interaction_id: "x2".into(), user: m.user.clone(), member: Some(m) };
    let again = svc::open_ticket(&t3, gid(g), &op, &p, p.options.first(), None, None).await.unwrap();
    assert_eq!(again.ticket.number, 2);
    assert_eq!(ticket_row(&t, g, &first.ticket.id).await.status(), "DELETED");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn failures_delete_the_reserved_row_and_map_to_friendly_codes() {
    let g = "955020000000001009";
    let Some((t, io)) = setup(g, 960_009_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    // Create the category first so the error queue hits the ticket channel.
    svc::ensure_category(&t, gid(g), &p.panel.id, svc::CategoryKind::Open).await;
    let cases = [(30013, "GUILD_FULL"), (50013, "BOT_PERMS"), (50035, "CREATE_FAILED")];
    for (i, (code, want)) in cases.into_iter().enumerate() {
        io.w().create_errors.push_back(svc::IoError::code(code, "x"));
        let users = [ALICE, BOB, CAROL];
        let r = svc::open_ticket(&t, gid(g), &opener(&t, g, users[i], 100 + i as u64), &p, p.options.first(), None, None).await.unwrap_err();
        assert_eq!(r.code, want);
    }
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Ticket" WHERE "guildId" = $1"#).bind(g).fetch_one(&t.pool).await.unwrap();
    assert_eq!(count, 0, "every reserved row was removed");

    // A full category: the ticket is created outside it, with a note.
    let cat = created_category(&t, g, &p.panel.id).await;
    io.w().full_parents.insert(cat);
    let o = svc::open_ticket(&t, gid(g), &opener(&t, g, STAFF, 200), &p, p.options.first(), None, None).await.unwrap();
    assert_eq!(o.notes, vec![svc::FULL_NOTE.to_string()]);
    assert_eq!(o.channel.parent_id, None);
    t.reset_guilds(&[g]).await;
}

// ── Lifecycle ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn claim_close_reopen_add_remove_rename_and_transcripts() {
    let g = "955020000000001010";
    let Some((t, io)) = setup(g, 960_010_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    let id = o.ticket.id.clone();
    let ch = o.channel.id;

    // The opener may not claim; staff may.
    let r = svc::claim_ticket(&t, actx(&t, g, &id, ALICE, svc::Source::Command)).await.unwrap_err();
    assert_eq!(r.code, "NOT_STAFF");
    svc::claim_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command)).await.unwrap();
    let row = ticket_row(&t, g, &id).await;
    assert_eq!(row.ticket.status, TicketStatus::Claimed);
    assert_eq!(row.ticket.claimed_by_id.as_deref(), Some(STAFF));
    let moved = io.calls("setParent");
    assert_eq!(moved.len(), 1);
    let claimed_cat = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap().panel.claimed_category_id.unwrap();
    assert_eq!(moved[0].data["parent"], json!(claimed_cat));
    let notice = io.sent(ch).last().cloned().unwrap();
    assert_eq!(notice["embeds"][0]["title"], json!("🙋 Ticket Claimed"));
    let r = svc::claim_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command)).await.unwrap_err();
    assert_eq!(r.code, "ALREADY_CLAIMED_SELF");
    // An admin takes over.
    svc::claim_ticket(&t, actx(&t, g, &id, ADMIN, svc::Source::Command)).await.unwrap();
    let notice = io.sent(ch).last().cloned().unwrap();
    assert_eq!(notice["embeds"][0]["description"], json!(format!("<@{ADMIN}> took over this ticket from <@{STAFF}>.")));

    // The opener closes: read-only access, the closed category, the Reopen / Transcript / Delete row.
    svc::close_ticket(&t, actx(&t, g, &id, ALICE, svc::Source::Command), Some("solved".into())).await.unwrap();
    let row = ticket_row(&t, g, &id).await;
    assert_eq!(row.ticket.status, TicketStatus::Closed);
    assert_eq!(row.ticket.closed_by_id.as_deref(), Some(ALICE));
    assert!(row.ticket.closed_at.is_some());
    let edits = io.calls("overwrite.edit");
    assert_eq!(edits.last().unwrap().data["edits"], json!({ "ViewChannel": true, "SendMessages": false, "ReadMessageHistory": true }));
    let notice = io.sent(ch).last().cloned().unwrap();
    assert_eq!(notice["embeds"][0]["fields"][0]["value"], json!("solved"));
    assert_eq!(notice["components"][0]["components"][0]["custom_id"], json!(format!("ticket:reopen:{id}")));
    let r = svc::close_ticket(&t, actx(&t, g, &id, ALICE, svc::Source::Command), None).await.unwrap_err();
    assert_eq!(r.code, "ALREADY_CLOSED");

    // Reopen: back to CLAIMED (it had been claimed).
    svc::reopen_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command)).await.unwrap();
    let row = ticket_row(&t, g, &id).await;
    assert_eq!(row.ticket.status, TicketStatus::Claimed);
    assert_eq!(row.ticket.closed_at, None);

    // Add and remove.
    let bob = member(&t, g, BOB);
    svc::add_member(&t, actx(&t, g, &id, STAFF, svc::Source::Command), bob.user.clone(), true).await.unwrap();
    assert_eq!(ticket_row(&t, g, &id).await.ticket.added_user_ids, vec![BOB.to_string()]);
    let r = svc::add_member(&t, actx(&t, g, &id, STAFF, svc::Source::Command), bob.user.clone(), true).await.unwrap_err();
    assert_eq!((r.code.as_str(), r.data.clone()), ("ALREADY_ADDED", json!({ "userId": BOB })));
    let alice = member(&t, g, ALICE);
    let r = svc::add_member(&t, actx(&t, g, &id, STAFF, svc::Source::Command), alice.user.clone(), true).await.unwrap_err();
    assert_eq!(r.code, "IS_OWNER");
    let r = svc::add_member(&t, actx(&t, g, &id, STAFF, svc::Source::Command), member(&t, g, CAROL).user, false).await.unwrap_err();
    assert_eq!(r.code, "NOT_MEMBER");
    svc::remove_member(&t, actx(&t, g, &id, STAFF, svc::Source::Command), bob.user.id).await.unwrap();
    assert!(ticket_row(&t, g, &id).await.ticket.added_user_ids.is_empty());
    let r = svc::remove_member(&t, actx(&t, g, &id, STAFF, svc::Source::Command), bob.user.id).await.unwrap_err();
    assert_eq!(r.code, "NOT_ADDED");

    // Rename: sanitised, two per ten minutes.
    let r = svc::rename_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command), "!!!".into()).await.unwrap_err();
    assert_eq!(r.code, "BAD_NAME");
    svc::rename_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command), "Printer Fire".into()).await.unwrap();
    assert_eq!(io.calls("setName")[0].data["name"], json!("printer-fire"));
    svc::rename_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command), "second".into()).await.unwrap();
    let r = svc::rename_ticket(&t, actx(&t, g, &id, STAFF, svc::Source::Command), "third".into()).await.unwrap_err();
    assert_eq!(r.code, "RENAME_LIMIT");
    assert!(r.data["retryAt"].as_f64().is_some());

    // Transcripts: staff only, throttled, built from the history.
    io.add_history(&ch.to_string(), 3, ALICE);
    let full = ticket_row(&t, g, &id).await;
    let channel = bot_core::interaction_helpers::cached_channel(&t, gid(g), ch).unwrap();
    let alice_actor = svc::Actor::of(&t, gid(g), &alice);
    let r = svc::transcript_for_actor(&t, gid(g), &full, &channel, None, &alice_actor).await.err().unwrap();
    assert_eq!(r.code, "NOT_STAFF");
    let staff_actor = svc::Actor::of(&t, gid(g), &member(&t, g, STAFF));
    let ok = svc::transcript_for_actor(&t, gid(g), &full, &channel, None, &staff_actor).await.ok().unwrap();
    let file = &ok.payload.files[0];
    assert_eq!(file.filename, "transcript-ticket-0001.html");
    let html = String::from_utf8(file.data.clone()).unwrap();
    assert!(html.contains("message 0") && html.contains("message 2") && html.contains("3 message(s)"));
    let r = svc::transcript_for_actor(&t, gid(g), &full, &channel, None, &staff_actor).await.err().unwrap();
    assert_eq!(r.code, "TRANSCRIPT_THROTTLED");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn lifecycle_actions_refuse_without_bot_permissions_and_change_nothing() {
    let g = "955020000000001011";
    let Some((t, _io)) = setup(g, 960_011_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    // A bot that is not an administrator, denied Manage Channels in the ticket channel.
    let url = database_url().unwrap();
    let t2 = DbTestApp::new(&url).await.with_guilds([g]);
    let bot_role = "955020000000000203";
    let perms = Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY | Permissions::EMBED_LINKS | Permissions::ATTACH_FILES;
    let guild = FakeGuild::new(g, OWNER)
        .role(bot_role, 3, perms)
        .role(STAFF_ROLE, 2, Permissions::empty())
        .member(TEST_BOT_ID, &[bot_role])
        .member(STAFF, &[STAFF_ROLE])
        .channel(&o.channel.id.to_string(), 0, "ticket-0001", &[(bot_role, false, Permissions::empty(), Permissions::MANAGE_CHANNELS)]);
    t2.set_discord(offline_discord_as(&[guild.json()]));
    let _io2 = FakeIo::new(&t2, 960_011_500_000_000_000);
    let staff = cached_member(&t2, g, STAFF);
    let ctx = svc::ActionCtx { guild_id: gid(g), ticket_id: o.ticket.id.clone(), actor: svc::Actor::of(&t2, gid(g), &staff), settings: None, source: svc::Source::Command };
    let r = svc::claim_ticket(&t2, ctx.clone()).await.unwrap_err();
    assert_eq!(r.code, "BOT_PERMS");
    assert_eq!(r.data, json!({ "names": "Manage Channels", "channelId": o.channel.id.to_string() }));
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "OPEN", "nothing changed");
    // Add needs Manage Roles, which the bot still has.
    let bob: serenity::all::User = serde_json::from_value(json!({ "id": BOB, "username": "bob", "discriminator": "0", "avatar": null })).unwrap();
    svc::add_member(&t2, ctx, bob, true).await.unwrap();
    t.reset_guilds(&[g]).await;
}

/// A ticket channel and a transcript channel that only a REST fetch finds (discord.js `guild.channels.fetch` caches
/// the result, and `missingBotPermissions(channel)` / `checkChannelUsable(channel)` judge the object): the bot's
/// permissions are computed from the fetched channel's own overwrites, not refused because it is not cached.
#[tokio::test]
async fn rest_fetched_channels_are_judged_by_their_own_overwrites() {
    let g = "955020000000001051";
    let tc = "955020000000001951";
    let Some((t, _io)) = setup(g, 960_051_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    svc::update_panel_fields(&t, &p.panel.id, g, &[("transcriptChannelId", json!(tc))]).await.unwrap().unwrap();
    let url = database_url().unwrap();
    let t2 = DbTestApp::new(&url).await.with_guilds([g]);
    let bot_role = "955020000000000203";
    let perms = Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY | Permissions::EMBED_LINKS | Permissions::ATTACH_FILES;
    // The guild is cached, the ticket and transcript channels are not.
    let guild = FakeGuild::new(g, OWNER).role(bot_role, 3, perms).role(STAFF_ROLE, 2, Permissions::empty()).member(TEST_BOT_ID, &[bot_role]).member(STAFF, &[STAFF_ROLE]);
    t2.set_discord(offline_discord_as(&[guild.json()]));
    let io2 = FakeIo::new(&t2, 960_051_500_000_000_000);
    svc::set_delete_delay_ms(&t2, 5);
    let ticket_ch = common::channel_json(o.channel.id.get(), g, 0, "ticket-0001", None, json!([]), None);
    let deny_files = json!([{ "id": bot_role, "type": 0, "allow": "0", "deny": Permissions::ATTACH_FILES.bits().to_string() }]);
    let transcript_ch = common::channel_json(tc.parse().unwrap(), g, 0, "transcripts", None, deny_files, None);
    io2.w().rest_only.insert(o.channel.id.to_string(), ticket_ch);
    io2.w().rest_only.insert(tc.to_string(), transcript_ch);
    let staff = cached_member(&t2, g, STAFF);
    let ctx = svc::ActionCtx { guild_id: gid(g), ticket_id: o.ticket.id.clone(), actor: svc::Actor::of(&t2, gid(g), &staff), settings: None, source: svc::Source::Command };
    // Claim: the bot holds every permission in the fetched channel, so it goes ahead.
    svc::claim_ticket(&t2, ctx.clone()).await.unwrap();
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "CLAIMED");
    // Delete (STAFF is a manager of the panel): the transcript channel denies Attach Files.
    let r = svc::delete_ticket(&t2, ctx, None).await.unwrap_err();
    assert_eq!(r.code, "TRANSCRIPT_CHANNEL");
    assert_eq!(r.data, json!({ "channelId": tc, "reason": "missing Attach Files" }));

    // A transcript of a fetched channel that denies Read Message History names only that permission.
    let deny_history = json!([{ "id": bot_role, "type": 0, "allow": "0", "deny": Permissions::READ_MESSAGE_HISTORY.bits().to_string() }]);
    let ch = common::channel_json(o.channel.id.get(), g, 0, "ticket-0001", None, deny_history, None);
    let row = ticket_row(&t, g, &o.ticket.id).await;
    let actor = svc::Actor::of(&t2, gid(g), &staff);
    let r = svc::transcript_for_actor(&t2, gid(g), &row, &ch, None, &actor).await.err().unwrap();
    assert_eq!((r.code.as_str(), r.data.clone()), ("TRANSCRIPT_FAILED", json!({ "reason": "missing Read Message History" })));

    // Refreshing a panel published in a fetched channel that denies Send Messages is NO_ACCESS: nothing is edited
    // and the panel keeps its message.
    let pc = "955020000000001952";
    let deny_send = json!([{ "id": bot_role, "type": 0, "allow": "0", "deny": Permissions::SEND_MESSAGES.bits().to_string() }]);
    io2.w().rest_only.insert(pc.to_string(), common::channel_json(pc.parse().unwrap(), g, 0, "panels", None, deny_send, None));
    svc::update_panel_fields(&t2, &p.panel.id, g, &[("channelId", json!(pc)), ("messageId", json!("955020000000001953"))]).await.unwrap().unwrap();
    assert_eq!(svc::refresh_panel_message(&t2, gid(g), &p.panel.id).await, svc::Refresh::NoAccess { channel_id: pc.to_string() });
    assert!(io2.calls("edit").is_empty(), "no edit was attempted");
    let fresh = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    assert_eq!(fresh.panel.message_id.as_deref(), Some("955020000000001953"), "the panel keeps its message");
    t.reset_guilds(&[g]).await;
}

/// `/ticket transcript` reads `interaction.channel`: a ticket channel missing from the cache is fetched over REST (and
/// cached), not answered with "Not a Ticket".
#[tokio::test]
async fn ticket_transcript_finds_an_uncached_ticket_channel_over_rest() {
    let g = "955020000000001053";
    let Some((t, _io)) = setup(g, 960_053_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    let ch = o.channel.id.to_string();
    let url = database_url().unwrap();
    let t2 = DbTestApp::new(&url).await.with_guilds([g]);
    let rest = bot_core::testing::FakeRest::new();
    let guild = FakeGuild::new(g, OWNER)
        .role(ADMIN_ROLE, 3, Permissions::ADMINISTRATOR)
        .role(STAFF_ROLE, 2, Permissions::empty())
        .member(TEST_BOT_ID, &[ADMIN_ROLE])
        .member(STAFF, &[STAFF_ROLE]);
    t2.set_discord(rest.discord(&[guild.json()]));
    t2.install(registry());
    let io2 = FakeIo::new(&t2, 960_053_500_000_000_000);
    io2.add_history(&ch, 2, ALICE);
    rest.on("GET", &format!("/channels/{ch}"), 200, json!({ "id": ch, "guild_id": g, "type": 0, "name": "ticket-0001", "position": 0, "permission_overwrites": [], "nsfw": false }));
    route(&t2, FakeInteraction::slash("ticket").guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(&ch).subcommand("transcript")).await;
    assert_eq!(t2.responses.calls()[1].embed_title(), Some("📄 Ticket #0001 transcript"), "{:?}", t2.responses.names());
    assert_eq!(rest.calls_to("GET", &format!("/channels/{ch}")).len(), 1);
    t.reset_guilds(&[g]).await;
}

/// `withTimeout(channel.setName(...), 15_000)` only stops waiting: a rate-limited rename still lands later, as the
/// note promises.
#[tokio::test]
async fn a_slow_rename_gets_the_note_and_still_happens() {
    let g = "955020000000001052";
    let Some((t, io)) = setup(g, 960_052_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    svc::set_rename_timeout_ms(&t, 50);
    io.w().rename_delay_ms = Some(300);
    let done = svc::rename_ticket(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command), "Slow Name".into()).await.unwrap();
    assert_eq!(done.notes, vec!["Discord is rate limiting renames; the new name will appear shortly.".to_string()]);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(io.late_renames.lock().unwrap().clone(), vec![(o.channel.id.to_string(), "slow-name".to_string())], "the rename was not cancelled");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn delete_saves_the_transcript_then_deletes_and_blocks_other_actions() {
    let g = "955020000000001012";
    let tc = "955020000000001912";
    let Some((t, io)) = setup(g, 960_012_000_000_000_000, &[tc]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    svc::update_panel_fields(&t, &p.panel.id, g, &[("transcriptChannelId", json!(tc)), ("dmTranscript", json!(true))]).await.unwrap().unwrap();
    let o = open(&t, g, &p, ALICE, 1).await;
    io.add_history(&o.channel.id.to_string(), 2, ALICE);

    // The delete button needs the ticket closed first.
    let r = svc::delete_ticket(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Button), None).await.unwrap_err();
    assert_eq!(r.code, "CLOSE_FIRST");

    let saved: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let s2 = saved.clone();
    let on_saved: svc::OnSaved = Box::new(move |text| {
        Box::pin(async move {
            *s2.lock().unwrap() = Some(text);
        })
    });
    svc::set_delete_delay_ms(&t, 300);
    let (app, ctx) = (t.app.clone(), actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command));
    let running = tokio::spawn(async move { svc::delete_ticket(&app, ctx, Some(on_saved)).await });
    // While it runs, other actions are told it is being deleted.
    let mut blocked = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Err(f) = svc::claim_ticket(&t, actx(&t, g, &o.ticket.id, ADMIN, svc::Source::Command)).await
            && f.code == "DELETING"
        {
            blocked = Some(f.code);
            break;
        }
    }
    assert_eq!(blocked.as_deref(), Some("DELETING"));
    running.await.unwrap().unwrap();

    let posted = io.sent(tc);
    assert_eq!(posted[0]["files"][0]["name"], json!("transcript-ticket-0001.html"));
    assert_eq!(posted[0]["embeds"][0]["title"], json!("📄 Ticket #0001 transcript"));
    let fields = posted[0]["embeds"][0]["fields"].as_array().unwrap();
    assert!(fields.iter().any(|f| f["name"] == "Deleted by" && f["value"] == json!(format!("<@{STAFF}>"))));
    let dms = io.w().dms.clone();
    assert_eq!(dms[0].0, ALICE);
    assert_eq!(dms[0].1["embeds"][0]["footer"]["text"], json!("Sent from Test Guild"));
    let text = saved.lock().unwrap().clone().unwrap();
    assert_eq!(text, format!("Deleted by <@{STAFF}>. This channel will be deleted in 5 seconds.\nThe transcript was saved in <#{tc}>.\nA copy was sent to the opener by DM."));
    assert_eq!(io.calls("delete").len(), 1);
    let row = ticket_row(&t, g, &o.ticket.id).await;
    assert_eq!(row.ticket.status, TicketStatus::Deleted);
    assert_eq!(row.ticket.closed_by_id.as_deref(), Some(STAFF));
    let r = svc::claim_ticket(&t, actx(&t, g, &o.ticket.id, ADMIN, svc::Source::Command)).await.unwrap_err();
    assert_eq!(r.code, "TICKET_DELETED");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn delete_reports_a_closed_dm_and_a_refused_channel_delete() {
    let g = "955020000000001013";
    let Some((t, io)) = setup(g, 960_013_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    svc::update_panel_fields(&t, &p.panel.id, g, &[("dmTranscript", json!(true))]).await.unwrap().unwrap();
    let o = open(&t, g, &p, ALICE, 1).await;
    io.w().dm_error = Some(svc::IoError::code(50007, "Cannot send messages to this user"));
    io.w().delete_error = Some(svc::IoError::code(50013, "Missing Permissions"));
    let r = svc::delete_ticket(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command), None).await.unwrap_err();
    assert_eq!(r.code, "DELETE_FAILED");
    assert_eq!(r.data, json!({ "reason": "I'm missing permissions there", "transcriptSaved": true }));
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "OPEN", "the row stays until the channel is gone");
    let notice = io.sent(o.channel.id).last().cloned().unwrap();
    assert_eq!(notice["embeds"][0]["description"], json!(format!("Deleted by <@{STAFF}>. This channel will be deleted in 5 seconds.")));
    t.reset_guilds(&[g]).await;
}

// ── channelDelete, reconcile, publishing, manager sync ────────────────────────

#[tokio::test]
async fn channel_delete_marks_tickets_and_clears_dead_references() {
    let g = "955020000000001014";
    let tc = "955020000000001914";
    let Some((t, _io)) = setup(g, 960_014_000_000_000_000, &[tc]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    let cat = created_category(&t, g, &p.panel.id).await;
    svc::update_panel_fields(&t, &p.panel.id, g, &[("transcriptChannelId", json!(tc)), ("channelId", json!(tc)), ("messageId", json!("1"))]).await.unwrap().unwrap();
    svc::update_option(&t, &p.panel.id, g, &p.options[0].id, vec![("categoryId", json!(cat))]).await.unwrap().unwrap();

    // Through the plugin hook (`always`, filtered on text, announcement and category channels).
    let ticket_channel = bot_core::interaction_helpers::cached_channel(&t, gid(g), o.channel.id).unwrap();
    t.run_hooks(bot_core::hooks::events::ChannelDelete { guild_id: gid(g), channel: ticket_channel }).await;
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "DELETED");

    svc::on_channel_delete(&t, g, tc, false).await;
    svc::on_channel_delete(&t, g, &cat, true).await;
    let panel = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    assert_eq!(panel.panel.transcript_channel_id, None);
    assert_eq!((panel.panel.channel_id.clone(), panel.panel.message_id.clone()), (None, None));
    assert_eq!(panel.panel.open_category_id, None);
    assert_eq!(panel.options[0].category_id, None);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn list_panels_view_reconciles_with_discord_first() {
    let g = "955020000000001015";
    let Some((t, io)) = setup(g, 960_015_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let live = open(&t, g, &p, ALICE, 1).await;
    let insert = |channel: &'static str, number: i32, created: chrono::NaiveDateTime| {
        let (pool, pid) = (t.pool.clone(), p.panel.id.clone());
        async move {
            sqlx::query(r#"INSERT INTO "Ticket" ("id", "guildId", "panelId", "number", "channelId", "ownerId", "createdAt", "updatedAt") VALUES ($1, $2, $3, $4, $5, $6, $7, $7)"#)
                .bind(bot_db::new_cuid())
                .bind(g)
                .bind(pid)
                .bind(number)
                .bind(channel)
                .bind(BOB)
                .bind(created)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    let now = chrono::Utc::now().naive_utc();
    insert("955020000000003001", 50, now).await; // gone (10003)
    insert("pending:stale", 51, now - chrono::Duration::minutes(10)).await;
    insert("pending:fresh", 52, now).await;
    let view = svc::list_panels_view(&t, gid(g)).await.unwrap();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].1, 2, "the live ticket and the fresh placeholder");
    let statuses: Vec<(String, String)> = sqlx::query_as(r#"SELECT "channelId", "status"::text FROM "Ticket" WHERE "guildId" = $1 ORDER BY "number""#).bind(g).fetch_all(&t.pool).await.unwrap();
    assert_eq!(
        statuses,
        vec![(live.channel.id.to_string(), "OPEN".into()), ("955020000000003001".into(), "DELETED".into()), ("pending:fresh".into(), "OPEN".into())]
    );
    assert!(!io.calls("fetch").is_empty(), "the cache miss was confirmed with Discord");
    let line = svc::panel_list_line(&t, gid(g), &view[0].0.panel, view[0].0.options.len(), view[0].1);
    assert_eq!(line, "**Support** — Buttons · 1 option(s) · 0 manager role(s) · 2 active ticket(s) · not published");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn publish_posts_edits_moves_and_recovers_from_a_deleted_message() {
    let g = "955020000000001016";
    let (c1, c2) = ("955020000000001916", "955020000000001917");
    let Some((t, io)) = setup(g, 960_016_000_000_000_000, &[c1, c2]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let ch1 = bot_core::interaction_helpers::cached_channel(&t, gid(g), c1.parse::<u64>().unwrap().into()).unwrap();
    let ch2 = bot_core::interaction_helpers::cached_channel(&t, gid(g), c2.parse::<u64>().unwrap().into()).unwrap();
    let r = svc::publish_panel(&t, gid(g), &p.panel.id, &ch1, None).await.unwrap();
    assert!(!r.edited());
    let posted = &io.sent(c1)[0];
    assert_eq!(posted["embeds"][0]["title"], json!("How can we help?"));
    assert_eq!(posted["components"][0]["components"][0]["custom_id"], json!(format!("ticket:open:{}:{}", p.panel.id, p.options[0].id)));
    assert!(svc::publish_panel(&t, gid(g), &p.panel.id, &ch1, None).await.unwrap().edited());
    // Moving deletes the old message and posts a new one.
    let moved = svc::publish_panel(&t, gid(g), &p.panel.id, &ch2, None).await.unwrap();
    assert!(!moved.edited());
    assert_eq!(io.calls("deleteMessage").len(), 1);
    let stored = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    assert_eq!(stored.panel.channel_id.as_deref(), Some(c2));
    assert_eq!(stored.panel.message_id, Some(moved.message().id.to_string()));

    // refreshPanelMessage edits; a deleted message clears the panel.
    assert_eq!(svc::refresh_panel_message(&t, gid(g), &p.panel.id).await, svc::Refresh::Ok { skipped: false });
    io.w().messages.clear();
    assert_eq!(svc::refresh_panel_message(&t, gid(g), &p.panel.id).await, svc::Refresh::Gone);
    let cleared = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    assert_eq!(cleared.panel.message_id, None);
    assert_eq!(svc::refresh_panel_message(&t, gid(g), &p.panel.id).await, svc::Refresh::Ok { skipped: true });
    let gone = svc::publish_panel(&t, gid(g), "nope", &ch1, None).await.unwrap_err();
    assert_eq!(gone, svc::PublishError::PanelGone);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn manager_overwrites_are_back_filled_by_the_command_path_and_the_job() {
    let g = "955020000000001017";
    let Some((t, io)) = setup(g, 960_017_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let a = open(&t, g, &p, ALICE, 1).await;
    let b = open(&t, g, &p, BOB, 2).await;
    let counts = svc::sync_manager_overwrites(&t, gid(g), &p.panel.id, STAFF_ROLE, true).await.unwrap();
    assert_eq!(counts, svc::SyncCounts { updated: 2, failed: 0, skipped: 0 });
    let edits = io.calls("overwrite.edit");
    assert_eq!(edits.iter().map(|c| c.channel.clone()).collect::<Vec<_>>(), vec![b.channel.id.to_string(), a.channel.id.to_string()], "newest first");

    // The job: the panel now lists the role, so it is added; a role it does not list is removed.
    svc::update_managers(&t, &p.panel.id, gid(g), |_| Ok(vec![STAFF_ROLE.to_string()])).await.unwrap().unwrap();
    let key = svc::manager_sync_dedupe_key(g);
    let run_at = chrono::Utc::now() + chrono::Duration::hours(1);
    for role in [STAFF_ROLE, "955020000000000777"] {
        t.scheduler()
            .schedule_job(
                bot_core::scheduler::ScheduleJob::new(g, bot_config::JobType::TicketManagersync, run_at)
                    .dedupe_key(key.clone())
                    .payload(svc::manager_sync_payload(&p.panel.id, &[role.to_string()]))
                    .merge(svc::merge_manager_sync),
            )
            .await
            .unwrap();
    }
    let row = t.scheduler().get_job(&key).await.unwrap();
    assert_eq!(row.payload["panels"][&p.panel.id]["rev"], json!(2));
    let job = bot_core::scheduler::Job {
        id: row.id.clone(),
        guild_id: g.into(),
        job_type: row.r#type.clone(),
        payload: row.payload.clone(),
        run_at: row.run_at,
        attempts: 1,
        max_attempts: row.max_attempts,
        dedupe_key: row.dedupe_key.clone(),
        version: row.version,
    };
    let before = io.calls("overwrite.edit").len();
    let outcome = svc::run_manager_sync_job(&t, &job, gid(g), svc::MANAGER_SYNC_JOB_BUDGET_MS).await.unwrap();
    assert_eq!(outcome, bot_core::scheduler::SettleOutcome::Done);
    assert_eq!(io.calls("overwrite.edit").len() - before, 2);
    assert_eq!(io.calls("overwrite.delete").len(), 2, "the unlisted role is removed from both tickets");
    assert!(t.scheduler().get_job(&key).await.is_none(), "the job settled itself");
    t.reset_guilds(&[g]).await;
}

// ── Through the router ────────────────────────────────────────────────────────

async fn route(t: &DbTestApp, it: FakeInteraction) {
    bot_core::router::route_interaction(t, it.build()).await;
}

#[tokio::test]
async fn the_open_button_defers_then_answers_with_a_link() {
    let g = "955020000000001018";
    let Some((t, io)) = setup(g, 960_018_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    t.settings().get_settings(g).await.unwrap();
    route(&t, FakeInteraction::button(&format!("ticket:open:{}:{}", p.panel.id, p.options[0].id)).guild(g).user(ALICE)).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"], "{calls:?}");
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Ticket Opened"));
    assert!(calls[1].embed_description().unwrap().starts_with("Your ticket is ready: <#"));
    assert_eq!(io.w().created.len(), 2);

    // With ask_reason the click shows the form first and nothing else.
    svc::update_panel_fields(&t, &p.panel.id, g, &[("askReason", json!(true))]).await.unwrap().unwrap();
    t.responses.clear();
    route(&t, FakeInteraction::button(&format!("ticket:open:{}:{}", p.panel.id, p.options[0].id)).guild(g).user(BOB)).await;
    assert_eq!(t.responses.names(), vec!["showModal"]);
    let modal = &t.responses.calls()[0].body;
    assert_eq!(modal["custom_id"], json!(format!("ticket-modal:{}:{}", p.panel.id, p.options[0].id)));

    // The form opens the ticket with the reason.
    t.responses.clear();
    route(&t, FakeInteraction::modal(&format!("ticket-modal:{}:{}", p.panel.id, p.options[0].id), &[("ticket-reason", "  broken  ")]).guild(g).user(BOB)).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    let reason: Option<String> = sqlx::query_scalar(r#"SELECT "reason" FROM "Ticket" WHERE "guildId" = $1 AND "ownerId" = $2"#).bind(g).bind(BOB).fetch_one(&t.pool).await.unwrap();
    assert_eq!(reason.as_deref(), Some("broken"));

    // An unknown option.
    t.responses.clear();
    route(&t, FakeInteraction::button(&format!("ticket:open:{}:nope", p.panel.id)).guild(g).user(CAROL)).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Option Not Found"));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn the_dropdown_resets_itself_with_update_and_answers_by_follow_up() {
    let g = "955020000000001019";
    let Some((t, _io)) = setup(g, 960_019_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    t.settings().get_settings(g).await.unwrap();
    // The clicked message carries the real panel dropdown: the update sends it back unchanged.
    let rows = svc::panel_components_json(&p.panel.id, TicketPanelStyle::Select, &p.options, &|_| true);
    let mut it = FakeInteraction::select(&format!("ticket-select:{}", p.panel.id), &[&p.options[0].id]).guild(g).user(ALICE);
    it.json_mut()["message"]["components"] = json!(rows);
    route(&t, it).await;
    assert_eq!(t.responses.names(), vec!["update", "followUp"]);
    assert_eq!(norm(&t.responses.calls()[0].body["components"]), norm(&json!(rows)), "the dropdown survives the round trip");
    assert!(t.responses.calls()[1].ephemeral());
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Ticket Opened"));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn lifecycle_buttons_defer_with_update_and_follow_up_on_errors() {
    let g = "955020000000001020";
    let Some((t, _io)) = setup(g, 960_020_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    t.settings().get_settings(g).await.unwrap();
    let o = open(&t, g, &p, ALICE, 1).await;
    let ch = o.channel.id.to_string();
    // The opener's claim: a follow-up error.
    route(&t, FakeInteraction::button(&format!("ticket:claim:{}", o.ticket.id)).guild(g).user(ALICE).channel(&ch)).await;
    assert_eq!(t.responses.names(), vec!["deferUpdate", "followUp"]);
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Staff Only"));
    // Staff claims: only the clicked message is edited.
    t.responses.clear();
    let mut it = FakeInteraction::button(&format!("ticket:claim:{}", o.ticket.id)).guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(&ch);
    it.json_mut()["message"]["components"] = json!([svc::intro_row_json(&o.ticket.id, false)]);
    route(&t, it).await;
    assert_eq!(t.responses.names(), vec!["deferUpdate", "editReply"]);
    let edited = t.responses.calls()[1].body["components"].clone();
    let mut want = svc::intro_row_json(&o.ticket.id, false);
    want["components"][1]["disabled"] = json!(true);
    assert_eq!(norm(&edited), norm(&json!([want])), "only the Claim button is disabled");
    // Close then disables every button of the clicked message.
    t.responses.clear();
    let mut it = FakeInteraction::button(&format!("ticket:close:{}", o.ticket.id)).guild(g).user(ALICE).channel(&ch);
    it.json_mut()["message"]["components"] = json!([svc::intro_row_json(&o.ticket.id, false)]);
    route(&t, it).await;
    let edited = t.responses.calls()[1].body["components"].clone();
    assert_eq!(edited[0]["components"][0]["disabled"], json!(true));
    assert_eq!(edited[0]["components"][1]["disabled"], json!(true));
    svc::reopen_ticket(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command)).await.unwrap();
    // A button in another channel does not reach the ticket.
    t.responses.clear();
    route(&t, FakeInteraction::button(&format!("ticket:close:{}", o.ticket.id)).guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel("955020000000000001")).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Ticket Not Found"));
    // The delete button on an open ticket: close it first.
    t.responses.clear();
    route(&t, FakeInteraction::button(&format!("ticket:delete:{}", o.ticket.id)).guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(&ch)).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("⚠️  Close It First"));
    // Transcript: a deferred reply with the file.
    t.responses.clear();
    route(&t, FakeInteraction::button(&format!("ticket:transcript:{}", o.ticket.id)).guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(&ch)).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(t.responses.calls()[1].embed_title(), Some("📄 Ticket #0001 transcript"));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn ticket_command_claim_close_and_not_a_ticket() {
    let g = "955020000000001021";
    let Some((t, _io)) = setup(g, 960_021_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    let ch = o.channel.id.to_string();
    route(&t, FakeInteraction::slash("ticket").guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(&ch).subcommand("claim")).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(t.responses.calls()[0].ephemeral());
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Ticket claimed."));
    t.responses.clear();
    route(
        &t,
        FakeInteraction::slash("ticket")
            .guild(g)
            .user(ALICE)
            .channel(&ch)
            .subcommand("close")
            .option("reason", bot_core::command::option_type::STRING, json!("  thanks  ")),
    )
    .await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Ticket closed."));
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "CLOSED");
    t.responses.clear();
    route(&t, FakeInteraction::slash("ticket").guild(g).user(ALICE).channel("955020000000000001").subcommand("claim")).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Not a Ticket"));
    // Deleting a closed ticket skips the dialog; the interaction is answered once the transcript is safe.
    t.responses.clear();
    route(&t, FakeInteraction::slash("ticket").guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(&ch).subcommand("delete")).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Deleting Ticket"));
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "DELETED");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn ticket_panel_commands_create_edit_options_managers_and_autocomplete() {
    let g = "955020000000001022";
    let c1 = "955020000000001922";
    let Some((t, io)) = setup(g, 960_022_000_000_000_000, &[c1]).await else { return };
    let admin = |it: FakeInteraction| it.guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR);
    let s = bot_core::command::option_type::STRING;
    route(
        &t,
        admin(FakeInteraction::slash("ticket-panel").subcommand("create"))
            .option("name", s, json!("Support"))
            .option("channel", bot_core::command::option_type::CHANNEL, json!(c1)),
    )
    .await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"], "{calls:?}");
    assert_eq!(calls[1].embed_title(), Some("✅  Panel Created"));
    assert!(calls[1].embed_description().unwrap().contains(&format!("Published it in <#{c1}>. [Jump to panel](")));
    assert_eq!(io.sent(c1).len(), 1);
    let panel = svc::find_panel(t.db(), g, "support").await.unwrap().unwrap();

    t.responses.clear();
    route(&t, admin(FakeInteraction::slash("ticket-panel").subcommand("create")).option("name", s, json!("support"))).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Name Taken"));

    t.responses.clear();
    route(&t, admin(FakeInteraction::slash("ticket-panel").subcommand("edit")).option("panel", s, json!(panel.panel.id)).option("style", s, json!("SELECT"))).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Style: **Dropdown**"));
    let edits = io.calls("edit");
    assert_eq!(edits.len(), 1, "the published panel was refreshed");
    assert_eq!(edits[0].data["payload"]["components"][0]["components"][0]["type"], json!(3));

    t.responses.clear();
    route(
        &t,
        admin(FakeInteraction::slash("ticket-panel").group("option", "add"))
            .option("panel", s, json!("Support"))
            .option("label", s, json!("Billing"))
            .option("emoji", s, json!("💳")),
    )
    .await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Added 💳 **Billing** to **Support** (2/25)."));

    t.responses.clear();
    route(&t, admin(FakeInteraction::slash("ticket-panel").group("option", "list")).option("panel", s, json!("Support"))).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("**1.** ✉️ **Open Ticket**\n**2.** 💳 **Billing**"));

    t.responses.clear();
    route(
        &t,
        admin(FakeInteraction::slash("ticket-panel").group("managers", "add"))
            .option("panel", s, json!("Support"))
            .option("role", bot_core::command::option_type::ROLE, json!(STAFF_ROLE)),
    )
    .await;
    let d = t.responses.calls()[1].embed_description().unwrap().to_string();
    assert!(d.starts_with(&format!("<@&{STAFF_ROLE}> can now see and handle tickets from **Support**.")), "{d}");
    assert!(d.ends_with(&format!("Manager roles: <@&{STAFF_ROLE}> (1/10)")));

    t.responses.clear();
    let mut ac = FakeInteraction::autocomplete("ticket-panel").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR).group("option", "edit");
    ac = ac.option("panel", s, json!("Support")).focused("option", "bil");
    route(&t, ac).await;
    let choices = t.responses.calls()[0].choices();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0]["name"], json!("💳 Billing"));

    t.responses.clear();
    route(&t, admin(FakeInteraction::slash("ticket-panel").subcommand("list"))).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("🎫 Ticket panels (1/25)"));

    // The template form saves the panel message and refreshes the published one.
    t.responses.clear();
    let form = format!("ticket-modal:tpl:{}:panel", panel.panel.id);
    route(&t, FakeInteraction::modal(&form, &[("tpl-content", ""), ("tpl-title", "Need help?"), ("tpl-description", "Pick one"), ("tpl-color", ""), ("tpl-footer", "")]).guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR)).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Message Saved"), "{calls:?}");
    let stored = svc::find_panel(t.db(), g, &panel.panel.id).await.unwrap().unwrap();
    assert_eq!(stored.panel.template.as_ref().unwrap()["embed"]["title"], json!("Need help?"));
    t.reset_guilds(&[g]).await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_writes_rust_reads_and_acts() {
    let g = "955020000000001023";
    let Some((t, io)) = setup(g, 960_023_000_000_000_000, &[]).await else { return };
    let seeded = run_node("rust/fixtures/gen/feat-tickets-js.mjs", &["seed", g, STAFF_ROLE, ALICE, "955020000000006000"]);
    assert!(seeded.get("error").is_none(), "{seeded}");
    let panel_id = seeded["panelId"].as_str().unwrap();
    let ticket_id = seeded["ticketId"].as_str().unwrap();
    let js = run_node("rust/fixtures/gen/feat-tickets-js.mjs", &["read", g, panel_id, ticket_id]);

    // The rows read the same in Rust.
    let panel = svc::find_panel(t.db(), g, panel_id).await.unwrap().unwrap();
    assert_eq!(panel.to_json(), js["panel"]);
    let ticket = ticket_row(&t, g, ticket_id).await;
    assert_eq!(ticket.to_json(), js["ticket"]);
    assert_eq!(ticket.ticket.status, TicketStatus::Claimed);
    assert_eq!(ticket.ticket.claimed_by_id.as_deref(), Some("955020000000000304"));
    // The same rows build the same components (the customIds the JS bot posted keep working).
    let comps = svc::panel_components_json(&panel.panel.id, panel.panel.style, &panel.options, &|_| true);
    assert_eq!(json!(comps), js["components"]);
    assert_eq!(svc::intro_row_json(ticket_id, false)["components"][0]["custom_id"], js["intro"]["components"][0]["custom_id"]);
    // The merged job the JS scheduled reads the same in Rust.
    let job = t.scheduler().get_job(&svc::manager_sync_dedupe_key(g)).await.unwrap();
    assert_eq!(svc::merge_manager_sync(&job.payload, &json!({})), js["job"]["reread"]);
    assert_eq!(job.payload["panels"][panel_id]["rev"], json!(2));

    // Rust acts on the JS ticket: the channel the JS bot created is in the cache.
    let mut ch = io.add_channel(g, 0, "ticket-js", None);
    ch.id = seeded["channelId"].as_str().unwrap().parse::<u64>().unwrap().into();
    svc::cache_channel(&t, &ch, false);
    svc::close_ticket(&t, actx(&t, g, ticket_id, STAFF, svc::Source::Command), None).await.unwrap();
    assert_eq!(ticket_row(&t, g, ticket_id).await.status(), "CLOSED");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn rust_writes_js_reads() {
    let g = "955020000000001024";
    let Some((t, _io)) = setup(g, 960_024_000_000_000_000, &[]).await else { return };
    let mut np = svc::NewPanel::new(g, "Rust Panel");
    np.style = TicketPanelStyle::Select;
    np.max_open_per_user = 2;
    np.channel_name_format = "{user.name}-{number}".into();
    let p = svc::create_panel(&t, np).await.unwrap().unwrap();
    svc::update_panel_fields(
        &t,
        &p.panel.id,
        g,
        &[("template", json!({ "content": "Hi", "embed": { "title": "T", "description": "D", "color": 0x112233 }, "buttons": [] })), ("dmTranscript", json!(true))],
    )
    .await
    .unwrap()
    .unwrap();
    svc::add_option(&t, &p.panel.id, g, svc::NewOption { label: "Billing".into(), emoji: Some("💳".into()), description: Some("Money".into()), button_style: 3, category_id: None })
        .await
        .unwrap()
        .unwrap();
    svc::update_managers(&t, &p.panel.id, gid(g), |_| Ok(vec![STAFF_ROLE.to_string()])).await.unwrap().unwrap();
    let panel = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    let o = svc::open_ticket(&t, gid(g), &opener(&t, g, ALICE, 1), &panel, panel.options.get(1), Some("from Rust".into()), None).await.unwrap();
    svc::claim_ticket(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command)).await.unwrap();
    svc::add_member(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command), member(&t, g, BOB).user, true).await.unwrap();
    let key = svc::manager_sync_dedupe_key(g);
    let run_at = chrono::Utc::now() + chrono::Duration::hours(1);
    for role in [STAFF_ROLE, "955020000000000777"] {
        t.scheduler()
            .schedule_job(
                bot_core::scheduler::ScheduleJob::new(g, bot_config::JobType::TicketManagersync, run_at)
                    .dedupe_key(key.clone())
                    .payload(svc::manager_sync_payload(&p.panel.id, &[role.to_string()]))
                    .merge(svc::merge_manager_sync),
            )
            .await
            .unwrap();
    }

    let js = run_node("rust/fixtures/gen/feat-tickets-js.mjs", &["read", g, &p.panel.id, &o.ticket.id]);
    assert!(js.get("error").is_none(), "{js}");
    let panel = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    assert_eq!(js["panel"], panel.to_json());
    assert_eq!(js["panel"]["options"][0]["label"], json!("Open Ticket"));
    assert_eq!(js["panel"]["managerRoleIds"], json!([STAFF_ROLE]));
    let ticket = ticket_row(&t, g, &o.ticket.id).await;
    assert_eq!(js["ticket"], ticket.to_json());
    assert_eq!(js["ticket"]["status"], json!("CLAIMED"));
    assert_eq!(js["ticket"]["addedUserIds"], json!([BOB]));
    assert_eq!(js["ticket"]["reason"], json!("from Rust"));
    assert_eq!(js["byChannel"], json!(o.ticket.id));
    assert_eq!(js["job"]["type"], json!("ticket.managersync"));
    let job = t.scheduler().get_job(&key).await.unwrap();
    assert_eq!(js["job"]["reread"], job.payload, "the JS reads the Rust-merged payload unchanged");
    assert_eq!(js["job"]["reread"]["panels"][&p.panel.id]["roles"], json!([STAFF_ROLE, "955020000000000777"]));
    assert_eq!(json!(svc::panel_components_json(&panel.panel.id, panel.panel.style, &panel.options, &|_| true)), js["components"]);
    t.reset_guilds(&[g]).await;
}

// ── More ported DB cases ──────────────────────────────────────────────────────

fn admin_slash(g: &str, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("ticket-panel").guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR).subcommand(sub)
}

/// Waits for a confirm prompt and clicks Confirm (or Cancel) as `user`.
async fn click_confirm(t: &DbTestApp, g: &str, user: &str, roles: &[&str], channel: &str, yes: bool) {
    for _ in 0..300 {
        if !t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let calls = t.responses.calls();
    let id = calls
        .iter()
        .rev()
        .find_map(|c| c.body["components"][0]["components"][0]["custom_id"].as_str().filter(|s| s.starts_with("confirm:")).map(str::to_string))
        .expect("a confirm prompt");
    let id = if yes { id } else { id.replace(":yes", ":no") };
    route(t, FakeInteraction::button(&id).guild(g).user(user).roles(roles).channel(channel).message_id("1000000000000000100")).await;
}

#[tokio::test]
async fn ticket_panel_categories_transcripts_publish_message_and_delete() {
    let g = "955020000000001025";
    let (text, other) = ("955020000000001925", "955020000000001926");
    let Some((t, io)) = setup(g, 960_025_000_000_000_000, &[text, other]).await else { return };
    let s = bot_core::command::option_type::STRING;
    let c = bot_core::command::option_type::CHANNEL;
    let b = bot_core::command::option_type::BOOLEAN;
    let p = new_panel(&t, g, "Support", false).await;
    let cat = io.add_channel(g, 4, "Help Desk", None);

    route(&t, admin_slash(g, "categories").option("panel", s, json!("Support"))).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("🎫 Support — categories"));
    assert!(calls[1].embed_description().unwrap().starts_with("**Open:** not set — I create \"Tickets\" when it is first needed"));

    t.responses.clear();
    route(&t, admin_slash(g, "categories").option("panel", s, json!("Support")).option("open", c, json!(cat.id.to_string()))).await;
    let d = t.responses.calls()[1].embed_description().unwrap().to_string();
    assert!(d.starts_with(&format!("**Open:** <#{}> (0/50 channels)", cat.id)), "{d}");
    assert!(d.ends_with("Existing tickets are not moved."));

    t.responses.clear();
    route(&t, admin_slash(g, "categories").option("panel", s, json!("Support")).option("create_missing", b, json!(true))).await;
    let d = t.responses.calls()[1].embed_description().unwrap().to_string();
    assert!(d.contains("Created **Claimed Tickets**.") && d.contains("Created **Closed Tickets**."), "{d}");
    assert!(!d.contains("Created **Tickets**."), "the open category was already set");

    t.responses.clear();
    route(&t, admin_slash(g, "transcripts").option("panel", s, json!("Support"))).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("🎫 Support — transcripts"));
    t.responses.clear();
    route(&t, admin_slash(g, "transcripts").option("panel", s, json!("Support")).option("channel", c, json!(text)).option("disable", b, json!(true))).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Conflicting Options"));
    t.responses.clear();
    route(&t, admin_slash(g, "transcripts").option("panel", s, json!("Support")).option("channel", c, json!(text)).option("dm_owner", b, json!(true))).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Transcripts Updated"));
    assert_eq!(
        t.responses.calls()[1].embed_description(),
        Some(format!("**Channel:** <#{text}>\n**DM the opener:** Yes\nTranscripts are made when a ticket is deleted, and with `/ticket transcript`.").as_str())
    );

    t.responses.clear();
    route(&t, admin_slash(g, "publish").option("panel", s, json!("Support")).option("channel", c, json!(other))).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Panel Published"));
    assert!(calls[1].embed_description().unwrap().contains(svc::MANAGERS_WARNING));

    // `message` opens the form first, with the right id, title and prefill.
    t.responses.clear();
    route(&t, admin_slash(g, "message").option("panel", s, json!("support"))).await;
    assert_eq!(t.responses.names(), vec!["showModal"]);
    let modal = t.responses.calls()[0].body.clone();
    assert_eq!(modal["custom_id"], json!(format!("ticket-modal:tpl:{}:panel", p.panel.id)));
    assert_eq!(modal["title"], json!("Panel message: Support"));
    assert_eq!(modal["components"][1]["components"][0]["value"], json!("How can we help?"));
    t.responses.clear();
    route(&t, admin_slash(g, "message").option("panel", s, json!("support")).option("type", s, json!("intro")).option("option", s, json!("open ticket"))).await;
    let modal = t.responses.calls()[0].body.clone();
    assert_eq!(modal["custom_id"], json!(format!("ticket-modal:tpl:{}:intro:{}", p.panel.id, p.options[0].id)));
    assert_eq!(modal["title"], json!("Intro: Open Ticket"));
    assert_eq!(modal["components"][1]["components"][0]["value"], json!("Ticket #{number}"));

    // `delete` asks first.
    t.responses.clear();
    let it = admin_slash(g, "delete").option("panel", s, json!("Support")).channel(text).build();
    let app = t.app.clone();
    let running = tokio::spawn(async move { bot_core::router::route_interaction(&app, it).await });
    click_confirm(&t, g, ADMIN, &[ADMIN_ROLE], text, true).await;
    running.await.unwrap();
    let calls = t.responses.calls();
    let last = calls.iter().rev().find(|c| c.embed_title().is_some()).unwrap();
    assert_eq!(last.embed_title(), Some("✅  Panel Deleted"), "{calls:?}");
    assert!(svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().is_none());
    t.reset_guilds(&[g]).await;
}

/// The intro template form as `user`, with these inputs.
fn intro_form(g: &str, panel_id: &str, text: &str, user: &str, roles: &[&str], content: &str, description: &str) -> FakeInteraction {
    let form = format!("ticket-modal:tpl:{panel_id}:intro");
    let fields = [("tpl-content", content), ("tpl-title", "T"), ("tpl-description", description), ("tpl-color", ""), ("tpl-footer", "")];
    FakeInteraction::modal(&form, &fields).guild(g).user(user).roles(roles).channel(text)
}

#[tokio::test]
async fn template_form_refusals_and_the_module_off() {
    let g = "955020000000001026";
    let text = "955020000000001927";
    let Some((t, _io)) = setup(g, 960_026_000_000_000_000, &[text]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    svc::update_managers(&t, &p.panel.id, gid(g), |_| Ok(vec![MGR2.to_string()])).await.unwrap().unwrap();
    let pid = p.panel.id.clone();

    // A stale form (an input is missing).
    let stale = FakeInteraction::modal(&format!("ticket-modal:tpl:{pid}:intro"), &[("tpl-content", "x")]).guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).channel(text);
    route(&t, stale).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"], "{calls:?}");
    assert_eq!(calls[1].embed_title(), Some("❌  Invalid Message"));
    assert!(calls[1].embed_description().unwrap().starts_with("• This form is out of date. Open it again."));

    // A description over 4000 characters.
    t.responses.clear();
    route(&t, intro_form(g, &pid, text, ADMIN, &[ADMIN_ROLE], "hi", &"d".repeat(4001))).await;
    assert!(t.responses.calls()[1].embed_description().unwrap().contains("Description is 4001 characters (max 4000)."));

    // {managers} pings an unmentionable role the admin may not ping.
    t.responses.clear();
    route(&t, intro_form(g, &pid, text, MGUILD, &[MGUILD_ROLE], "{managers}", "x")).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("❌  Role Not Pingable"), "{calls:?}");
    assert_eq!(
        calls[1].embed_description(),
        Some(format!("<@&{MGR2}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{text}>.").as_str())
    );

    // With the module off the template form still saves; the reason form refuses.
    t.settings().update_settings(g, bot_core::settings::SettingsPatch::new().set("ticketsEnabled", false)).await.unwrap();
    t.responses.clear();
    route(&t, intro_form(g, &pid, text, ADMIN, &[ADMIN_ROLE], "Hello {user}", "Welcome {nope}")).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Message Saved"));
    let d = calls[1].embed_description().unwrap();
    assert!(d.starts_with("Saved the ticket intro of **Support**."));
    assert!(d.contains("Unknown variables (left as typed): {nope}"), "{d}");
    assert_eq!(calls[2].method, "followUp");
    assert!(calls[2].content().unwrap().starts_with("**Preview** (nothing is pinged)\nHello <@"));
    let stored = svc::find_panel(t.db(), g, &pid).await.unwrap().unwrap();
    assert_eq!(stored.panel.intro_template.as_ref().unwrap()["content"], json!("Hello {user}"));
    t.responses.clear();
    route(&t, FakeInteraction::modal(&format!("ticket-modal:{pid}:{}", p.options[0].id), &[("ticket-reason", "x")]).guild(g).user(ALICE)).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("⚠️  Module Disabled"));

    // An empty form resets the message to its default.
    t.responses.clear();
    let empty = [("tpl-content", ""), ("tpl-title", ""), ("tpl-description", ""), ("tpl-color", ""), ("tpl-footer", "")];
    route(&t, FakeInteraction::modal(&format!("ticket-modal:tpl:{pid}:intro"), &empty).guild(g).user(ADMIN).roles(&[ADMIN_ROLE]).channel(text)).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Message Reset"));
    let stored = svc::find_panel(t.db(), g, &pid).await.unwrap().unwrap();
    assert_eq!(stored.panel.intro_template, None);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn post_intro_pings_managers_only_when_the_template_says_so() {
    let g = "955020000000001027";
    let Some((t, io)) = setup(g, 960_027_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    svc::update_managers(&t, &p.panel.id, gid(g), |_| Ok(vec![STAFF_ROLE.to_string(), MGR2.to_string()])).await.unwrap().unwrap();
    let p = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    let o = open(&t, g, &p, ALICE, 1).await;
    let intro = &io.sent(o.channel.id)[0];
    assert!(intro["allowedMentions"].get("roles").is_none_or(|r| r == &json!([])), "no {{managers}}: no role pings: {}", intro["allowedMentions"]);
    svc::update_panel_fields(&t, &p.panel.id, g, &[("introTemplate", json!({ "content": "{user} {managers}", "embed": null, "buttons": [] }))]).await.unwrap().unwrap();
    let p = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    let o = open(&t, g, &p, BOB, 2).await;
    let intro = &io.sent(o.channel.id)[0];
    assert_eq!(intro["content"], json!(format!("<@{BOB}> <@&{STAFF_ROLE}> <@&{MGR2}>")));
    // The bot is an administrator here, so it may ping both.
    assert_eq!(intro["allowedMentions"]["roles"], json!([STAFF_ROLE, MGR2]));
    assert_eq!(intro["allowedMentions"]["users"], json!([BOB]));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn concurrent_claims_one_wins_the_other_is_told_who() {
    let g = "955020000000001028";
    let Some((t, _io)) = setup(g, 960_028_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    let (a, b) = (actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command), actx(&t, g, &o.ticket.id, STAFF2, svc::Source::Command));
    let (app1, app2) = (t.app.clone(), t.app.clone());
    let (r1, r2) = tokio::join!(tokio::spawn(async move { svc::claim_ticket(&app1, a).await }), tokio::spawn(async move { svc::claim_ticket(&app2, b).await }));
    let results = [r1.unwrap(), r2.unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let loser = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert_eq!(loser.code, "ALREADY_CLAIMED");
    let winner = ticket_row(&t, g, &o.ticket.id).await.ticket.claimed_by_id.unwrap();
    assert_eq!(loser.data, json!({ "claimedById": winner }));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn reopening_an_unclaimed_ticket_returns_it_to_the_open_category() {
    let g = "955020000000001029";
    let Some((t, io)) = setup(g, 960_029_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    svc::close_ticket(&t, actx(&t, g, &o.ticket.id, ALICE, svc::Source::Command), None).await.unwrap();
    svc::reopen_ticket(&t, actx(&t, g, &o.ticket.id, STAFF, svc::Source::Command)).await.unwrap();
    let row = ticket_row(&t, g, &o.ticket.id).await;
    assert_eq!(row.ticket.status, TicketStatus::Open);
    let open_cat = created_category(&t, g, &p.panel.id).await;
    assert_eq!(io.calls("setParent").last().unwrap().data["parent"], json!(open_cat));
    assert_eq!(io.calls("overwrite.edit").last().unwrap().data["edits"]["SendMessages"], json!(true));
    let notice = io.sent(o.channel.id).last().cloned().unwrap();
    assert_eq!(notice["components"][0]["components"][1]["label"], json!("Claim"));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn an_option_category_overrides_the_panel_and_is_cleared_only_when_gone() {
    let g = "955020000000001030";
    let Some((t, io)) = setup(g, 960_030_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let cat = io.add_channel(g, 4, "Billing", None);
    svc::update_option(&t, &p.panel.id, g, &p.options[0].id, vec![("categoryId", json!(cat.id.to_string()))]).await.unwrap().unwrap();
    let p = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    let o = open(&t, g, &p, ALICE, 1).await;
    assert_eq!(o.channel.parent_id, Some(cat.id));

    // An option category Discord cannot confirm is kept; a confirmed one (10003) is cleared.
    let missing = "955020000000003555";
    svc::update_option(&t, &p.panel.id, g, &p.options[0].id, vec![("categoryId", json!(missing))]).await.unwrap().unwrap();
    let p = svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap();
    io.w().fetch_error = Some(svc::IoError::timed_out("fetch"));
    let o = open(&t, g, &p, BOB, 2).await;
    io.w().fetch_error = None;
    let open_cat = created_category(&t, g, &p.panel.id).await;
    assert_eq!(o.channel.parent_id.map(|c| c.to_string()), Some(open_cat), "the panel category is used meanwhile");
    assert_eq!(svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap().options[0].category_id.as_deref(), Some(missing));
    open(&t, g, &p, CAROL, 3).await;
    assert_eq!(svc::find_panel(t.db(), g, &p.panel.id).await.unwrap().unwrap().options[0].category_id, None);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn ensure_category_reuses_recreates_and_reports() {
    let g = "955020000000001031";
    let Some((t, io)) = setup(g, 960_031_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let existing = io.add_channel(g, 4, "tickets", None);
    let e = svc::ensure_category(&t, gid(g), &p.panel.id, svc::CategoryKind::Open).await;
    assert_eq!(e.category.as_ref().map(|c| c.id), Some(existing.id));
    assert!(e.reused && !e.created);
    assert!(io.w().created.is_empty(), "a same-named category is reused");
    assert_eq!(created_category(&t, g, &p.panel.id).await, existing.id.to_string());

    // The stored category was deleted (10003): a new one is created.
    svc::cache_channel(&t, &existing, true);
    let e = svc::ensure_category(&t, gid(g), &p.panel.id, svc::CategoryKind::Open).await;
    assert!(e.created);
    assert_ne!(e.category.as_ref().unwrap().id, existing.id);

    // 30013 and other failures come back as notes.
    io.w().create_errors.push_back(svc::IoError::code(30013, "Maximum number of guild channels reached"));
    let e = svc::ensure_category(&t, gid(g), &p.panel.id, svc::CategoryKind::Claimed).await;
    assert_eq!(e.note.as_deref(), Some("This server has 500 channels, so I couldn't create the \"Claimed Tickets\" category."));
    io.w().create_errors.push_back(svc::IoError::code(50035, "x"));
    let e = svc::ensure_category(&t, gid(g), &p.panel.id, svc::CategoryKind::Closed).await;
    assert_eq!(e.note.as_deref(), Some("I couldn't create the \"Closed Tickets\" category (Discord rejected the request)."));
    let missing = svc::ensure_category(&t, gid(g), "nope", svc::CategoryKind::Open).await;
    assert!(missing.category.is_none() && missing.note.is_none());
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn fetch_transcript_messages_pages_caps_and_returns_chronological_order() {
    let g = "955020000000001032";
    let text = "955020000000001932";
    let Some((t, io)) = setup(g, 960_032_000_000_000_000, &[text]).await else { return };
    io.add_history(text, 250, ALICE);
    let ch = serenity::all::ChannelId::new(text.parse().unwrap());
    let (all, truncated) = svc::fetch_transcript_messages(&t, ch, svc::MAX_TRANSCRIPT_MESSAGES, svc::TRANSCRIPT_DEADLINE_MS).await.unwrap();
    assert_eq!(all.len(), 250);
    assert_eq!(truncated, None);
    assert!(all.windows(2).all(|w| w[0].id < w[1].id), "chronological");
    let (some, truncated) = svc::fetch_transcript_messages(&t, ch, 120, svc::TRANSCRIPT_DEADLINE_MS).await.unwrap();
    assert_eq!((some.len(), truncated), (120, Some("limit")));
    assert_eq!(some.last().unwrap().id, all.last().unwrap().id, "the newest messages are kept");
    let (exact, truncated) = svc::fetch_transcript_messages(&t, ch, 200, svc::TRANSCRIPT_DEADLINE_MS).await.unwrap();
    assert_eq!((exact.len(), truncated), (200, Some("limit")));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn missing_guild_permissions_refuse_the_open_before_anything_is_reserved() {
    let g = "955020000000001033";
    let Some((t, _io)) = setup(g, 960_033_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", false).await;
    let url = database_url().unwrap();
    let t2 = DbTestApp::new(&url).await.with_guilds([g]);
    let bot_role = "955020000000000206";
    let guild = FakeGuild::new(g, OWNER)
        .role(bot_role, 3, Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS | Permissions::SEND_MESSAGES)
        .member(TEST_BOT_ID, &[bot_role])
        .member(ALICE, &[]);
    t2.set_discord(offline_discord_as(&[guild.json()]));
    let io2 = FakeIo::new(&t2, 960_033_500_000_000_000);
    let m = cached_member(&t2, g, ALICE);
    let op = svc::Opener { interaction_id: "p1".into(), user: m.user.clone(), member: Some(m) };
    let r = svc::open_ticket(&t2, gid(g), &op, &p, p.options.first(), None, None).await.unwrap_err();
    assert_eq!(r.code, "BOT_PERMS");
    assert_eq!(r.data, json!({ "names": "Manage Roles, Read Message History, Embed Links, Attach Files", "where": null }));
    assert!(io2.w().created.is_empty());
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Ticket" WHERE "guildId" = $1"#).bind(g).fetch_one(&t.pool).await.unwrap();
    assert_eq!(count, 0);
    t.reset_guilds(&[g]).await;
}

async fn run_delete(t: &DbTestApp, g: &str, ch: &str, yes: bool) {
    let it = FakeInteraction::slash("ticket").guild(g).user(STAFF).roles(&[STAFF_ROLE]).channel(ch).subcommand("delete").build();
    let app = t.app.clone();
    let running = tokio::spawn(async move { bot_core::router::route_interaction(&app, it).await });
    click_confirm(t, g, STAFF, &[STAFF_ROLE], ch, yes).await;
    running.await.unwrap();
}

#[tokio::test]
async fn ticket_delete_confirms_an_open_ticket_and_reports_a_refused_delete() {
    let g = "955020000000001034";
    let Some((t, io)) = setup(g, 960_034_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    let o = open(&t, g, &p, ALICE, 1).await;
    let ch = o.channel.id.to_string();
    // Cancelled: nothing happens.
    run_delete(&t, g, &ch, false).await;
    let calls = t.responses.calls();
    assert!(calls.iter().any(|c| c.embed_title() == Some("ℹ️  Cancelled")), "{calls:?}");
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "OPEN");

    // Confirmed, but Discord refuses the delete: reported on the still-valid interaction.
    io.w().delete_error = Some(svc::IoError::code(50013, "Missing Permissions"));
    t.responses.clear();
    run_delete(&t, g, &ch, true).await;
    let calls = t.responses.calls();
    let titles: Vec<&str> = calls.iter().filter_map(|c| c.embed_title()).collect();
    assert!(titles.contains(&"✅  Deleting Ticket"), "{titles:?}");
    assert_eq!(titles.last(), Some(&"❌  Delete Failed"));
    assert!(calls.iter().filter(|c| c.method == "followUp").all(|c| c.ephemeral()), "follow-ups after the dialog stay ephemeral");

    // Confirmed and allowed.
    io.w().delete_error = None;
    t.responses.clear();
    run_delete(&t, g, &ch, true).await;
    assert_eq!(ticket_row(&t, g, &o.ticket.id).await.status(), "DELETED");
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn the_manager_sync_job_runs_through_the_scheduler() {
    let g = "955020000000001035";
    let Some((t, io)) = setup(g, 960_035_000_000_000_000, &[]).await else { return };
    let p = new_panel(&t, g, "Support", true).await;
    open(&t, g, &p, ALICE, 1).await;
    let key = svc::manager_sync_dedupe_key(g);
    t.scheduler()
        .schedule_job(
            bot_core::scheduler::ScheduleJob::new(g, bot_config::JobType::TicketManagersync, chrono::Utc::now() - chrono::Duration::seconds(1))
                .dedupe_key(key.clone())
                .payload(svc::manager_sync_payload(&p.panel.id, &[STAFF_ROLE.to_string()]))
                .merge(svc::merge_manager_sync),
        )
        .await
        .unwrap();
    let before = io.calls("overwrite.edit").len();
    let mut ran = 0;
    for _ in 0..20 {
        ran += t.scheduler().tick(&t, false).await;
        if t.scheduler().get_job(&key).await.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ran >= 1);
    assert!(t.scheduler().get_job(&key).await.is_none(), "claimed, run and settled");
    assert_eq!(io.calls("overwrite.edit").len() - before, 1);
    t.reset_guilds(&[g]).await;
}
