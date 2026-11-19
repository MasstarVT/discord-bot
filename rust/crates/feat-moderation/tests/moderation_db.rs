//! Port of the `DB:` tests of `test/moderation.test.js` and the command flows of `test/moderation-commands.test.js`,
//! against the mock PostgreSQL / Redis, with Discord's REST replaced by [`FakeRest`] and the guild in an offline serenity
//! cache. Both cross-language directions: rows and jobs the JS service wrote run under Rust, and what Rust writes reads
//! back the same through Prisma.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use bot_core::error::BoxFuture;
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, database_url, offline_discord_as, redis_url, run_node};
use bot_db::{Infraction, InfractionType};
use feat_moderation::guild_ops::{AuditEntry, AuditKind, DiscordError, ModRest, Rest, UserRef, set_rest};
use feat_moderation::moderation_service::{self as svc, DmStatus, ModActionInput, ModCode};
use serde_json::{Value, json};
use serenity::all::{Ban, ChannelId, CreateMessage, GuildId, Member, Message, MessageId, Permissions as P, Role, RoleId, User, UserId};

// ── The fake Discord REST ─────────────────────────────────────────────────────

#[derive(Default)]
struct FakeRest {
    calls: Mutex<Vec<String>>,
    fail: Mutex<HashMap<String, u64>>,
    banned: Mutex<BTreeMap<u64, Option<String>>>,
    rest_members: Mutex<HashMap<u64, Value>>,
    dms: Mutex<Vec<(u64, Value)>>,
    sent: Mutex<Vec<(u64, Value)>>,
    audit: Mutex<Vec<AuditEntry>>,
    role_ids: Mutex<Vec<u64>>,
}

fn user_json(id: u64) -> Value {
    json!({ "id": id.to_string(), "username": format!("user{id}"), "discriminator": "0", "avatar": null })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeRest {
    fn record(&self, op: &str, detail: String) -> Result<(), DiscordError> {
        lock(&self.calls).push(format!("{op} {detail}"));
        match lock(&self.fail).get(op) {
            Some(code) => Err(DiscordError::code(*code, format!("{op} failed"))),
            None => Ok(()),
        }
    }
    fn fail_op(&self, op: &str, code: u64) {
        lock(&self.fail).insert(op.into(), code);
    }
    fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }
    fn ops(&self) -> Vec<String> {
        self.calls().iter().map(|c| c.split(' ').next().unwrap_or("").to_string()).collect()
    }
    fn ban_user(&self, id: &str, reason: Option<&str>) {
        lock(&self.banned).insert(id.parse().unwrap_or(1), reason.map(String::from));
    }
    fn is_banned(&self, id: &str) -> bool {
        lock(&self.banned).contains_key(&id.parse().unwrap_or(1))
    }
    fn sent_titles(&self) -> Vec<String> {
        lock(&self.sent).iter().map(|(_, m)| m["embeds"][0]["title"].as_str().unwrap_or("").to_string()).collect()
    }
}

fn done<T: Send + 'static>(r: Result<T, DiscordError>) -> Rest<T> {
    Box::pin(async move { r })
}

impl ModRest for FakeRest {
    fn get_member(&self, guild: GuildId, user: UserId) -> Rest<Member> {
        let r = self.record("get_member", user.to_string()).and_then(|()| {
            let mut m = lock(&self.rest_members).get(&user.get()).cloned().ok_or_else(|| DiscordError::code(10007, "Unknown Member"))?;
            m["guild_id"] = json!(guild.to_string());
            serde_json::from_value(m).map_err(|e| DiscordError::new(None, e.to_string()))
        });
        done(r)
    }
    fn get_user(&self, user: UserId) -> Rest<User> {
        let r = self.record("get_user", user.to_string()).and_then(|()| serde_json::from_value(user_json(user.get())).map_err(|e| DiscordError::new(None, e.to_string())));
        done(r)
    }
    fn get_ban(&self, _: GuildId, user: UserId) -> Rest<Option<Ban>> {
        let r = self.record("get_ban", user.to_string()).map(|()| {
            lock(&self.banned).get(&user.get()).map(|reason| serde_json::from_value(json!({ "reason": reason, "user": user_json(user.get()) })).expect("ban"))
        });
        done(r)
    }
    fn get_bans(&self, _: GuildId, after: Option<UserId>, limit: u16) -> Rest<Vec<Ban>> {
        let r = self.record("get_bans", format!("{after:?}")).map(|()| {
            let from = after.map(|a| a.get() + 1).unwrap_or(0);
            lock(&self.banned)
                .range(from..)
                .take(usize::from(limit))
                .map(|(id, reason)| serde_json::from_value(json!({ "reason": reason, "user": user_json(*id) })).expect("ban"))
                .collect()
        });
        done(r)
    }
    fn ban(&self, _: GuildId, user: UserId, secs: u32, reason: String) -> Rest<()> {
        let r = self.record("ban", format!("{user} {secs} {reason}"));
        if r.is_ok() {
            lock(&self.banned).insert(user.get(), Some(reason));
        }
        done(r)
    }
    fn unban(&self, _: GuildId, user: UserId, reason: String) -> Rest<()> {
        let r = self.record("unban", format!("{user} {reason}")).and_then(|()| match lock(&self.banned).remove(&user.get()) {
            Some(_) => Ok(()),
            None => Err(DiscordError::code(10026, "Unknown Ban")),
        });
        done(r)
    }
    fn kick(&self, _: GuildId, user: UserId, reason: String) -> Rest<()> {
        done(self.record("kick", format!("{user} {reason}")))
    }
    fn timeout(&self, _: GuildId, user: UserId, until: Option<chrono::DateTime<chrono::Utc>>, reason: String) -> Rest<()> {
        done(self.record("timeout", format!("{user} {} {reason}", if until.is_some() { "until" } else { "lift" })))
    }
    fn add_role(&self, _: GuildId, user: UserId, role: RoleId, reason: String) -> Rest<()> {
        done(self.record("add_role", format!("{user} {role} {reason}")))
    }
    fn remove_role(&self, _: GuildId, user: UserId, role: RoleId, reason: String) -> Rest<()> {
        done(self.record("remove_role", format!("{user} {role} {reason}")))
    }
    fn send_dm(&self, user: UserId, message: CreateMessage) -> Rest<(ChannelId, MessageId)> {
        let r = self.record("send_dm", user.to_string()).map(|()| {
            let mut dms = lock(&self.dms);
            dms.push((user.get(), serde_json::to_value(&message).unwrap_or(Value::Null)));
            (ChannelId::new(1), MessageId::new(dms.len() as u64))
        });
        done(r)
    }
    fn delete_message(&self, channel: ChannelId, message: MessageId) -> Rest<()> {
        done(self.record("delete_message", format!("{channel} {message}")))
    }
    fn send_message(&self, channel: ChannelId, message: CreateMessage) -> Rest<()> {
        let r = self.record("send_message", channel.to_string());
        if r.is_ok() {
            lock(&self.sent).push((channel.get(), serde_json::to_value(&message).unwrap_or(Value::Null)));
        }
        done(r)
    }
    fn audit_entries(&self, _: GuildId, kind: AuditKind, _: u8) -> Rest<Vec<AuditEntry>> {
        done(self.record("audit_entries", format!("{kind:?}")).map(|()| lock(&self.audit).clone()))
    }
    fn get_role_ids(&self, _: GuildId) -> Rest<Vec<RoleId>> {
        done(self.record("get_role_ids", String::new()).map(|()| lock(&self.role_ids).iter().map(|r| RoleId::new(*r)).collect()))
    }
    fn create_role(&self, guild: GuildId, name: String, reason: String) -> Rest<Role> {
        let r = self.record("create_role", format!("{name} {reason}")).map(|()| {
            serde_json::from_value(json!({
                "id": "830000000000000099", "guild_id": guild.to_string(), "name": name, "color": 0, "hoist": false, "position": 1,
                "permissions": "0", "managed": false, "mentionable": false, "flags": 0,
                "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
            }))
            .expect("role")
        });
        done(r)
    }
    fn deny_for_role(&self, channel: ChannelId, role: RoleId, allow: P, deny: P, _: String) -> Rest<()> {
        done(self.record("deny_for_role", format!("{channel} {role} {} {}", allow.bits(), deny.bits())))
    }
    fn get_messages(&self, channel: ChannelId, _: Option<MessageId>, _: u8) -> Rest<Vec<Message>> {
        done(self.record("get_messages", channel.to_string()).map(|()| Vec::new()))
    }
    fn bulk_delete(&self, channel: ChannelId, ids: Vec<MessageId>) -> Rest<()> {
        done(self.record("bulk_delete", format!("{channel} {}", ids.len())))
    }
    fn edit_slowmode(&self, channel: ChannelId, seconds: u16, _: String) -> Rest<()> {
        done(self.record("edit_slowmode", format!("{channel} {seconds}")))
    }
}

// ── The test guild ────────────────────────────────────────────────────────────

const OWNER: &str = "600000000000000099";
const MOD: &str = "600000000000000010";
const ALICE: &str = "600000000000000011";
const BOB: &str = "600000000000000012";
const CAROL: &str = "600000000000000013";
const OUTSIDER: &str = "600000000000000014";
const BOT_ROLE: &str = "820000000000000001";
const MOD_ROLE: &str = "820000000000000002";
const MUTE_ROLE: &str = "820000000000000003";
const IMMUNE_ROLE: &str = "820000000000000004";
const LOG: &str = "810000000000000001";
const GENERAL: &str = "810000000000000002";

fn bot_perms() -> P {
    P::BAN_MEMBERS | P::KICK_MEMBERS | P::MODERATE_MEMBERS | P::MANAGE_ROLES | P::VIEW_AUDIT_LOG | P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS
}

fn fake_guild(g: &str) -> Value {
    let guild = FakeGuild::new(g, OWNER)
        .everyone(P::VIEW_CHANNEL)
        .role(BOT_ROLE, 10, bot_perms())
        .role(MOD_ROLE, 5, P::MODERATE_MEMBERS | P::KICK_MEMBERS | P::BAN_MEMBERS)
        .role(MUTE_ROLE, 2, P::empty())
        .role(IMMUNE_ROLE, 1, P::empty())
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .member(MOD, &[MOD_ROLE])
        .member(ALICE, &[])
        .member(BOB, &[IMMUNE_ROLE])
        .member(CAROL, &[MUTE_ROLE])
        .member(OWNER, &[])
        .channel(LOG, 0, "mod-log", &[])
        .channel(GENERAL, 0, "general", &[]);
    let mut json = guild.json();
    if let Some(members) = json["members"].as_array_mut() {
        for m in members.iter_mut() {
            if m["user"]["id"] == CAROL {
                m["communication_disabled_until"] = json!("2099-01-01T00:00:00.000000+00:00");
            }
        }
    }
    json
}

struct Room {
    t: DbTestApp,
    g: &'static str,
    rest: Arc<FakeRest>,
}

impl Room {
    async fn new(g: &'static str) -> Option<Self> {
        Self::build(g, false).await
    }

    async fn build(g: &'static str, redis: bool) -> Option<Self> {
        Self::build_with(g, redis, None).await
    }

    /// A room whose serenity `Http` goes to a scripted [`bot_core::testing::FakeRest`] (the discord handle is set once).
    async fn with_discord_rest(g: &'static str, discord_rest: &bot_core::testing::FakeRest) -> Option<Self> {
        Self::build_with(g, false, Some(discord_rest.discord(&[fake_guild(g)]))).await
    }

    async fn build_with(g: &'static str, redis: bool, discord: Option<bot_core::discord::DiscordHandle>) -> Option<Self> {
        let url = database_url()?;
        let t = if redis {
            let r = redis_url()?;
            DbTestApp::with_redis(&url, &r).await.with_guilds([g])
        } else {
            DbTestApp::new(&url).await.with_guilds([g])
        };
        t.reset_guilds(&[g]).await;
        t.set_discord(discord.unwrap_or_else(|| offline_discord_as(&[fake_guild(g)])));
        let rest = Arc::new(FakeRest::default());
        set_rest(&t, rest.clone());
        t.ext::<svc::AuditDelay>().set(0);
        t.settings()
            .update_settings(g, SettingsPatch::new().set("modEnabled", true).set("modLogChannelId", LOG).set("muteRoleId", MUTE_ROLE).set("immuneRoleIds", json!([IMMUNE_ROLE])))
            .await
            .expect("settings");
        Some(Self { t, g, rest })
    }

    fn gid(&self) -> GuildId {
        GuildId::new(self.g.parse().unwrap_or(1))
    }

    fn moderator(&self) -> UserRef {
        UserRef { id: MOD.into(), username: Some("moduser".into()), tag: Some("moduser".into()), ..Default::default() }
    }

    fn input(&self, target: &str, action: &str) -> ModActionInput {
        ModActionInput::new(self.g, target, self.moderator(), action)
    }

    async fn rows(&self) -> Vec<Infraction> {
        sqlx::query_as::<_, Infraction>(r#"SELECT * FROM "Infraction" WHERE "guildId" = $1 ORDER BY "caseId""#).bind(self.g).fetch_all(self.t.db()).await.expect("rows")
    }

    async fn job(&self, key: &str) -> Option<bot_db::ScheduledJob> {
        self.t.scheduler().get_job(key).await
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn js(args: &[&str]) -> Value {
    let v = run_node("rust/fixtures/gen/feat-moderation-js.mjs", args);
    assert!(v.get("error").is_none(), "JS failed: {v}");
    v
}

// ── Case allocation ───────────────────────────────────────────────────────────

#[tokio::test]
async fn raw_sql_runs_and_case_numbers_are_unique_under_concurrency() {
    let Some(r) = Room::new("930000000000000001").await else { return };
    for (name, sql, sample) in svc::RAW_SQL {
        let mut tx = r.t.db().begin().await.expect("tx");
        sqlx::query(*sql).bind(*sample).execute(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        tx.rollback().await.expect("rollback");
    }
    let mut handles = Vec::new();
    for i in 0..12 {
        let app = (*r.t).clone();
        let g = r.g;
        handles.push(tokio::spawn(async move {
            svc::record_infraction(&app, svc::NewInfraction { reason: Some(format!("r{i}")), ..svc::NewInfraction::new(g, ALICE, MOD, InfractionType::Warn) }).await
        }));
    }
    for h in handles {
        h.await.expect("join").expect("recorded");
    }
    // The JS allocates on the same lock: its rows continue the numbering.
    let from_js = js(&["record", r.g, &json!([{ "targetUserId": ALICE, "moderatorId": MOD, "type": "NOTE", "reason": "  " }]).to_string()]);
    assert_eq!(from_js[0]["caseId"], 13);
    assert_eq!(from_js[0]["reason"], "No reason provided");
    let bulk = vec![
        svc::BulkInfraction { target_user_id: "1".into(), moderator_id: MOD.into(), r#type: InfractionType::Ban, reason: Some("[Imported] a".into()), active: Some(true), expires_at: None },
        svc::BulkInfraction { target_user_id: "2".into(), moderator_id: MOD.into(), r#type: InfractionType::Kick, reason: None, active: None, expires_at: None },
    ];
    assert_eq!(svc::record_infractions_bulk(&r.t, r.g, bulk).await.expect("bulk"), 2);
    let mut cases: Vec<i32> = r.rows().await.iter().map(|i| i.case_id).collect();
    cases.dedup();
    assert_eq!(cases, (1..=15).collect::<Vec<_>>());
    let rows = r.rows().await;
    assert!(!rows[14].active, "a KICK row is inactive by ACTION_META");
    assert_eq!(rows[14].reason.as_deref(), Some("No reason provided"));
    assert_eq!(svc::resolve_infractions(&r.t, r.g, ALICE, &[InfractionType::Warn]).await.expect("resolve"), 12);
    assert_eq!(svc::resolve_infractions(&r.t, r.g, ALICE, &[InfractionType::Warn]).await.expect("resolve"), 0);
    r.done().await;
}

// ── executeModAction ──────────────────────────────────────────────────────────

#[tokio::test]
async fn execute_mod_action_discord_first_then_record_and_failures_record_nothing() {
    let Some(r) = Room::new("930000000000000002").await else { return };
    // Input validation happens before anything is touched.
    let bad = svc::execute_mod_action(&r.t, ModActionInput { proof_url: Some("ftp://x".into()), ..r.input(ALICE, "KICK") }).await;
    assert_eq!(bad.map(|_| ()).map_err(|e| e.error), Err(svc::msg::BAD_PROOF.to_string()));
    assert_eq!(svc::execute_mod_action(&r.t, r.input("12", "KICK")).await.err().map(|e| e.error), Some("Invalid target user.".into()));
    assert_eq!(svc::execute_mod_action(&r.t, ModActionInput { source: "nope".into(), ..r.input(ALICE, "KICK") }).await.err().map(|e| e.error), Some("Unknown moderation source.".into()));
    assert_eq!(
        svc::execute_mod_action(&r.t, ModActionInput { delete_message_seconds: Some(700_000), ..r.input(ALICE, "BAN") }).await.err().map(|e| e.error),
        Some("Message deletion must be between 0 and 7 days.".into())
    );
    assert!(r.rest.calls().is_empty());

    // KICK: DM first, then the kick, then the case and the case log.
    let ok = svc::execute_mod_action(&r.t, ModActionInput { reason: Some("spam".into()), ..r.input(ALICE, "KICK") }).await.expect("kicked");
    assert_eq!((ok.infraction.case_id, ok.infraction.r#type, ok.dm, ok.action.as_str()), (1, InfractionType::Kick, DmStatus::Sent, "KICK"));
    assert_eq!(r.rest.ops(), vec!["send_dm", "kick", "send_message"]);
    assert!(r.rest.calls()[1].ends_with("spam | by moduser"), "{:?}", r.rest.calls());
    let dm = lock(&r.rest.dms)[0].1.clone();
    assert_eq!(dm["embeds"][0]["title"], "👢 You were kicked in Test Guild");
    assert_eq!(dm["embeds"][0]["fields"][1], json!({ "name": "Moderator", "value": "moduser", "inline": true }), "PUBLIC shows the moderator");
    assert_eq!(r.rest.sent_titles(), vec!["👢 Kick | Case #1"]);

    // A failed Discord action records nothing and takes the DM back.
    lock(&r.rest.calls).clear();
    r.rest.fail_op("ban", 50013);
    let err = svc::execute_mod_action(&r.t, r.input(ALICE, "BAN")).await.expect_err("refused");
    assert_eq!(err.code, ModCode::NotPermitted);
    assert_eq!(r.rest.ops(), vec!["get_ban", "send_dm", "ban", "delete_message"]);
    assert_eq!(r.rows().await.len(), 1);
    lock(&r.rest.fail).clear();

    // TEMPBAN schedules its expiry with the JS payload and dedupe key.
    let tb = svc::execute_mod_action(&r.t, ModActionInput { duration_ms: Some(86_400_000.0), delete_message_seconds: Some(86_400), ..r.input(ALICE, "BAN") })
        .await
        .expect("temp banned");
    assert_eq!(tb.action, "TEMPBAN");
    assert_eq!(tb.infraction.r#type, InfractionType::Ban);
    assert!(tb.infraction.expires_at.is_some());
    assert!(r.rest.calls().iter().any(|c| c.starts_with(&format!("ban {ALICE} 86400 "))));
    let job = r.job(&svc::unban_key(r.g, ALICE)).await.expect("job");
    assert_eq!(job.r#type, "mod.unban");
    assert_eq!(job.payload, json!({ "userId": ALICE, "infractionId": tb.infraction.id }));
    assert_eq!(job.max_attempts, 20);
    assert!(svc::was_bot_ban_action(&r.t, r.g, ALICE, "ban"), "the bot's own ban is marked");

    // Already banned -> ALREADY; a permanent ban on top cancels nothing it should keep.
    let again = svc::execute_mod_action(&r.t, r.input(ALICE, "BAN")).await.expect_err("already");
    assert_eq!((again.code, again.error.as_str()), (ModCode::Already, svc::msg::ALREADY_BANNED));
    let soft = svc::execute_mod_action(&r.t, r.input(ALICE, "SOFTBAN")).await.expect_err("already");
    assert_eq!(soft.error, svc::msg::SOFTBAN_BANNED);

    // Preflight through the real guild: immune role, not a member, the owner, the bot.
    let immune = svc::execute_mod_action(&r.t, r.input(BOB, "WARN")).await.expect_err("immune");
    assert_eq!((immune.code, immune.error.as_str()), (ModCode::Immune, svc::msg::IMMUNE_ROLE));
    assert_eq!(svc::execute_mod_action(&r.t, ModActionInput { source: "automod".into(), ..r.input(BOB, "WARN") }).await.err().map(|e| e.error), Some(svc::msg::IMMUNE_AUTO.into()));
    assert_eq!(svc::execute_mod_action(&r.t, r.input(OUTSIDER, "KICK")).await.err().map(|e| e.code), Some(ModCode::NotInGuild));
    assert_eq!(svc::execute_mod_action(&r.t, r.input(OWNER, "KICK")).await.err().map(|e| e.error), Some(svc::msg::OWNER.into()));
    assert_eq!(svc::execute_mod_action(&r.t, r.input(TEST_BOT_ID, "KICK")).await.err().map(|e| e.error), Some(svc::msg::BOT_SELF.into()));
    let unknown_mod = ModActionInput { moderator: UserRef::id(OUTSIDER), ..r.input(ALICE, "WARN") };
    assert_eq!(svc::execute_mod_action(&r.t, unknown_mod).await.err().map(|e| e.error), Some(svc::msg::NO_EXECUTOR.into()));

    // A permanent ban by id (not a member) works and cancels a stale temp-ban job.
    r.rest.ban_user(ALICE, None);
    lock(&r.rest.banned).remove(&OUTSIDER.parse().unwrap_or(1));
    let by_id = svc::execute_mod_action(&r.t, r.input(OUTSIDER, "BAN")).await.expect("banned by id");
    assert_eq!(by_id.infraction.expires_at, None);
    assert_eq!(by_id.dm, DmStatus::Sent);
    r.done().await;
}

#[tokio::test]
async fn mute_uses_the_mute_role_and_a_long_timeout_falls_back_to_it() {
    let Some(r) = Room::new("930000000000000003").await else { return };
    let t1 = svc::execute_mod_action(&r.t, ModActionInput { duration_ms: Some(600_000.0), ..r.input(ALICE, "TIMEOUT") }).await.expect("timed out");
    assert_eq!(t1.action, "TIMEOUT");
    assert!(r.rest.calls().iter().any(|c| c.starts_with(&format!("timeout {ALICE} until"))));
    let job = r.job(&svc::expire_key(t1.infraction.id)).await.expect("expire job");
    assert_eq!(job.payload, json!({ "infractionId": t1.infraction.id }));

    let long = svc::execute_mod_action(&r.t, ModActionInput { duration_ms: Some(30.0 * 86_400_000.0), ..r.input(ALICE, "TIMEOUT") }).await.expect("muted");
    assert_eq!(long.action, "MUTE");
    assert_eq!(long.warnings, vec!["Longer than 28 days, so the mute role was used instead of a timeout.".to_string()]);
    assert!(r.rest.calls().iter().any(|c| c.starts_with(&format!("add_role {ALICE} {MUTE_ROLE} "))));
    let job = r.job(&svc::expire_key(long.infraction.id)).await.expect("expire job");
    assert_eq!(job.payload, json!({ "infractionId": long.infraction.id, "roleId": MUTE_ROLE }));

    // A second MUTE supersedes the first (its row closes and its job is cancelled).
    let second = svc::execute_mod_action(&r.t, r.input(ALICE, "MUTE")).await.expect("muted again");
    let rows = r.rows().await;
    assert!(!rows.iter().find(|i| i.id == long.infraction.id).expect("row").active);
    assert!(rows.iter().find(|i| i.id == second.infraction.id).expect("row").active);
    assert!(r.job(&svc::expire_key(long.infraction.id)).await.is_none());
    assert!(r.job(&svc::expire_key(second.infraction.id)).await.is_none(), "a permanent mute has no expiry");
    let dm = lock(&r.rest.dms).last().cloned().expect("dm").1;
    assert_eq!(dm["embeds"][0]["fields"][1]["value"], "Until a moderator unmutes you");
    assert_eq!(dm["embeds"][0]["footer"]["text"], format!("Case #{}", second.infraction.case_id));

    // The mute role re-applies on rejoin while a role mute is active.
    lock(&r.rest.calls).clear();
    let settings = r.t.settings().get_settings_safe(r.g).await;
    svc::reapply_mute_on_join(&r.t, r.gid(), UserId::new(ALICE.parse().unwrap_or(1)), settings.as_deref()).await;
    assert_eq!(r.rest.calls(), vec![format!("add_role {ALICE} {MUTE_ROLE} Re-applied mute after rejoin (case #{})", second.infraction.case_id)]);
    lock(&r.rest.calls).clear();
    svc::reapply_mute_on_join(&r.t, r.gid(), UserId::new(BOB.parse().unwrap_or(1)), settings.as_deref()).await;
    assert!(r.rest.calls().is_empty(), "no active mute, no role");
    r.done().await;
}

#[tokio::test]
async fn unban_lifts_the_ban_closes_rows_and_cancels_the_job() {
    let Some(r) = Room::new("930000000000000004").await else { return };
    let tb = svc::execute_mod_action(&r.t, ModActionInput { duration_ms: Some(86_400_000.0), ..r.input(OUTSIDER, "BAN") }).await.expect("temp banned");
    assert!(r.job(&svc::unban_key(r.g, OUTSIDER)).await.is_some());
    let res = svc::execute_unban(&r.t, r.g, OUTSIDER, &r.moderator(), Some("appeal"), "command").await.expect("unbanned");
    assert_eq!((res.cleared, res.infraction.r#type, res.infraction.reason.as_deref()), (1, InfractionType::Unban, Some("appeal")));
    assert!(!r.rest.is_banned(OUTSIDER));
    assert!(r.job(&svc::unban_key(r.g, OUTSIDER)).await.is_none());
    assert!(!r.rows().await.iter().find(|i| i.id == tb.infraction.id).expect("row").active);
    // Not banned: stale rows are closed and reported.
    svc::record_infraction(&r.t, svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Ban)).await.expect("row");
    let again = svc::execute_unban(&r.t, r.g, OUTSIDER, &r.moderator(), None, "command").await.expect_err("not banned");
    assert_eq!((again.code, again.error.as_str()), (ModCode::Already, "That user is not banned. 1 stale ban record(s) were closed."));
    assert_eq!(svc::execute_unban(&r.t, r.g, "x", &r.moderator(), None, "command").await.err().map(|e| e.error), Some("Give a user ID (17–20 digits) or a mention.".into()));
    r.done().await;
}

#[tokio::test]
async fn unmute_lifts_the_timeout_and_the_role_and_closes_records() {
    let Some(r) = Room::new("930000000000000005").await else { return };
    let carol = UserRef::id(CAROL);
    svc::record_infraction(&r.t, svc::NewInfraction::new(r.g, CAROL, MOD, InfractionType::Mute)).await.expect("row");
    let res = svc::execute_unmute(&r.t, r.g, &carol, None, &r.moderator(), None, "command").await.expect("unmuted");
    assert_eq!(res.removed, vec!["timeout".to_string(), "mute role".to_string()]);
    assert_eq!(res.cleared, 1);
    assert!(r.rest.ops().contains(&"timeout".to_string()) && r.rest.ops().contains(&"remove_role".to_string()));
    // Nothing to do for a plain member; records only for someone who left.
    let none = svc::execute_unmute(&r.t, r.g, &UserRef::id(ALICE), None, &r.moderator(), None, "command").await.expect_err("nothing");
    assert_eq!(none.error, "That member is not timed out or muted.");
    assert_eq!(svc::execute_unmute(&r.t, r.g, &UserRef::id(OUTSIDER), None, &r.moderator(), None, "command").await.err().map(|e| e.code), Some(ModCode::NotInGuild));
    svc::record_infraction(&r.t, svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Timeout)).await.expect("row");
    let records = svc::execute_unmute(&r.t, r.g, &UserRef::id(OUTSIDER), None, &r.moderator(), Some("left"), "command").await.expect("records only");
    assert_eq!(records.note.as_deref(), Some("The user is not in the server, so only the records were closed."));
    assert_eq!(records.infraction.reason.as_deref(), Some("left (records only)"));
    r.done().await;
}

#[tokio::test]
async fn escalation_fires_once_per_window_with_severity() {
    let Some(r) = Room::new("930000000000000006").await else { return };
    // The tiers as the JS wrote them (legacy shape through the JS settings service).
    let tiers = json!([{ "id": 1, "warnCount": 2, "withinSeconds": 86400, "action": "TIMEOUT", "durationSeconds": 3600, "countFrom": null }]);
    js(&["settings-update", r.g, &json!({ "infractionEscalation": tiers }).to_string()]);
    r.t.settings().invalidate_settings(r.g).await;
    let first = svc::execute_mod_action(&r.t, ModActionInput { reason: Some("one".into()), ..r.input(ALICE, "WARN") }).await.expect("warned");
    assert!(first.escalation.is_none());
    let second = svc::execute_mod_action(&r.t, r.input(ALICE, "WARN")).await.expect("warned");
    let e = second.escalation.expect("escalated");
    assert_eq!((e.tier_id, e.action.as_str(), e.ok), (1, "TIMEOUT", true));
    let rows = r.rows().await;
    let esc = rows.iter().find(|i| i.r#type == InfractionType::Timeout).expect("timeout row");
    assert_eq!(esc.escalation_tier_id, Some(1));
    assert_eq!(esc.reason.as_deref(), Some("[Escalation T1] Reached 2 active warnings within 1 day"));
    assert_eq!(esc.moderator_id, TEST_BOT_ID);
    // The tier already fired in this window.
    let third = svc::execute_mod_action(&r.t, r.input(ALICE, "WARN")).await.expect("warned");
    assert!(third.escalation.is_none());
    assert_eq!(svc::count_active_warns(&r.t, r.g, ALICE).await.expect("count"), 3);
    r.done().await;
}

// ── Jobs: JS-written rows run under Rust ──────────────────────────────────────

#[tokio::test]
async fn js_written_unban_and_expire_jobs_run_under_rust() {
    let Some(r) = Room::new("930000000000000007").await else { return };
    r.t.install({
        let mut reg = Registry::new();
        feat_moderation::register(&mut reg);
        reg
    });
    let now = chrono::Utc::now().timestamp_millis();
    // The JS records a temp ban and a role mute that have both just expired, then schedules their jobs as it does.
    let rows = js(&[
        "record",
        r.g,
        &json!([
            { "targetUserId": OUTSIDER, "moderatorId": MOD, "type": "BAN", "reason": "tb", "expiresAt": now - 1000 },
            { "targetUserId": CAROL, "moderatorId": MOD, "type": "MUTE", "reason": "m", "expiresAt": now - 1000 },
            { "targetUserId": ALICE, "moderatorId": MOD, "type": "TIMEOUT", "reason": "t", "expiresAt": now - 1000 }
        ])
        .to_string(),
    ]);
    let (ban_id, mute_id, timeout_id) = (rows[0]["id"].to_string(), rows[1]["id"].to_string(), rows[2]["id"].to_string());
    js(&["schedule-unban", r.g, OUTSIDER, &ban_id, &(now - 1000).to_string()]);
    js(&["schedule-expire", r.g, &mute_id, MUTE_ROLE, &(now - 1000).to_string()]);
    js(&["schedule-expire", r.g, &timeout_id, "null", &(now - 1000).to_string()]);
    r.rest.ban_user(OUTSIDER, Some("tb"));

    for key in [svc::unban_key(r.g, OUTSIDER), format!("mod.expire:{mute_id}"), format!("mod.expire:{timeout_id}")] {
        let job = r.job(&key).await.unwrap_or_else(|| panic!("job {key}"));
        r.t.scheduler().run_job(&r.t, job).await;
        assert!(r.job(&key).await.is_none(), "{key} finished and was deleted");
    }
    assert!(!r.rest.is_banned(OUTSIDER));
    assert!(r.rest.calls().iter().any(|c| c == &format!("unban {OUTSIDER} Temporary ban expired (case #1)")));
    assert!(r.rest.calls().iter().any(|c| c == &format!("remove_role {CAROL} {MUTE_ROLE} Mute expired (case #2)")));
    let rows = r.rows().await;
    assert!(rows.iter().take(3).all(|i| !i.active), "all three closed");
    let new: Vec<(InfractionType, Option<&str>, &str)> = rows.iter().skip(3).map(|i| (i.r#type, i.reason.as_deref(), i.moderator_id.as_str())).collect();
    assert_eq!(
        new,
        vec![
            (InfractionType::Unban, Some("Temporary ban expired (case #1)"), TEST_BOT_ID),
            (InfractionType::Unmute, Some("Mute expired (case #2)"), TEST_BOT_ID)
        ],
        "a timeout expires without a new case"
    );
    r.done().await;
}

#[tokio::test]
async fn job_handlers_back_off_reschedule_early_runs_and_refuse_bad_payloads() {
    let Some(r) = Room::new("930000000000000008").await else { return };
    let job = |payload: Value| Job {
        id: "j".into(),
        guild_id: r.g.into(),
        job_type: "mod.unban".into(),
        payload,
        run_at: bot_db::now_utc(),
        attempts: 1,
        max_attempts: 20,
        dedupe_key: None,
        version: 0,
    };
    let ctx = || JobCtx { app: (*r.t).clone(), guild_id: r.g.into(), late_by_ms: 0, attempt: 1 };
    let bad = svc::run_unban_job(job(json!({ "userId": "x" })), ctx()).await.expect_err("bad payload");
    assert!(PermanentJobError::is(&bad));
    let bad = svc::run_expire_job(job(json!({ "infractionId": "x" })), ctx()).await.expect_err("bad payload");
    assert!(PermanentJobError::is(&bad));
    // Ran early: reschedule at the row's expiry.
    let at = bot_db::serde_js::from_epoch_ms(chrono::Utc::now().timestamp_millis() + 3_600_000).expect("date");
    let inf = svc::record_infraction(&r.t, svc::NewInfraction { expires_at: Some(at), ..svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Ban) }).await.expect("row");
    let out = svc::run_unban_job(job(json!({ "userId": OUTSIDER, "infractionId": inf.id })), ctx()).await.expect("ok");
    assert_eq!(out, JobOutcome::Reschedule { next_run_at: inf.expires_at.expect("at"), payload: None });
    // Already lifted or resolved: done.
    assert_eq!(svc::run_unban_job(job(json!({ "userId": OUTSIDER, "infractionId": 999_999 })), ctx()).await.expect("ok"), JobOutcome::Done);
    // Missing Ban Members: hourly back-off with permRetries.
    r.rest.ban_user(OUTSIDER, None);
    r.rest.fail_op("unban", 50013);
    let out = svc::run_unban_job(job(json!({ "userId": OUTSIDER, "infractionId": null, "permRetries": 3 })), ctx()).await.expect("backoff");
    match out {
        JobOutcome::Reschedule { payload, .. } => assert_eq!(payload, Some(json!({ "userId": OUTSIDER, "infractionId": null, "permRetries": 4 }))),
        other => panic!("{other:?}"),
    }
    let out = svc::run_unban_job(job(json!({ "userId": OUTSIDER, "permRetries": 168 })), ctx()).await.expect_err("given up");
    assert!(PermanentJobError::is(&out));
    r.done().await;
}

#[tokio::test]
async fn reconcile_expiries_schedules_revives_skips_ancient_bans_and_closes_expired_timeouts() {
    let Some(r) = Room::new("930000000000000009").await else { return };
    let ms = |d: i64| bot_db::serde_js::from_epoch_ms(chrono::Utc::now().timestamp_millis() + d).expect("date");
    let tb = svc::record_infraction(&r.t, svc::NewInfraction { expires_at: Some(ms(3_600_000)), ..svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Ban) }).await.expect("row");
    let mute = svc::record_infraction(&r.t, svc::NewInfraction { expires_at: Some(ms(7_200_000)), ..svc::NewInfraction::new(r.g, CAROL, MOD, InfractionType::Mute) }).await.expect("row");
    let expired = svc::record_infraction(&r.t, svc::NewInfraction { expires_at: Some(ms(-60_000)), ..svc::NewInfraction::new(r.g, ALICE, MOD, InfractionType::Timeout) }).await.expect("row");
    svc::record_infraction(&r.t, svc::NewInfraction { expires_at: Some(ms(-31 * 86_400_000)), ..svc::NewInfraction::new(r.g, BOB, MOD, InfractionType::Ban) }).await.expect("row");
    let out = svc::reconcile_expiries(&r.t, r.g).await;
    assert_eq!(out, svc::Reconciled { scheduled: 2, expired: 1, overdue: 1 });
    assert_eq!(r.job(&svc::unban_key(r.g, OUTSIDER)).await.map(|j| j.payload), Some(json!({ "userId": OUTSIDER, "infractionId": tb.id })));
    assert_eq!(r.job(&svc::expire_key(mute.id)).await.map(|j| j.payload), Some(json!({ "infractionId": mute.id })));
    assert!(!r.rows().await.iter().find(|i| i.id == expired.id).expect("row").active);
    assert_eq!(svc::reconcile_expiries(&r.t, r.g).await.scheduled, 0, "healthy jobs keep their place");
    // The JS reconciler sees the same state: nothing to schedule, the same overdue count.
    let from_js = js(&["reconcile", r.g]);
    assert_eq!(from_js, json!({ "scheduled": 0, "expired": 0, "overdue": 1 }));
    assert_eq!(js(&["overdue", r.g]), json!(svc::count_overdue_bans(&r.t, r.g).await));
    // A FAILED job is revived.
    sqlx::query(r#"UPDATE "ScheduledJob" SET "status" = 'FAILED' WHERE "dedupeKey" = $1"#).bind(svc::expire_key(mute.id)).execute(r.t.db()).await.expect("fail it");
    assert_eq!(svc::reconcile_expiries(&r.t, r.g).await.scheduled, 1);
    r.done().await;
}

// ── Ban import and the external ban hooks ─────────────────────────────────────

#[tokio::test]
async fn sync_guild_bans_imports_in_pages_never_duplicates_and_closes_gone_bans() {
    let Some(r) = Room::new("930000000000000010").await else { return };
    for i in 0..1205u64 {
        let id = (700_000_000_000_000_000 + i).to_string();
        r.rest.ban_user(&id, if i % 2 == 0 { Some("old raid") } else { None });
    }
    // A ban row whose ban is gone, older than the sync.
    let gone = svc::record_infraction(&r.t, svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Ban)).await.expect("row");
    sqlx::query(r#"UPDATE "Infraction" SET "createdAt" = "createdAt" - interval '1 hour' WHERE "id" = $1"#).bind(gone.id).execute(r.t.db()).await.expect("age it");
    let out = svc::sync_guild_bans(&r.t, r.gid(), TEST_BOT_ID).await.expect("synced");
    assert_eq!(out, svc::BanSync { imported: 1205, closed: 1, complete: true });
    assert_eq!(r.rest.ops().iter().filter(|o| *o == "get_bans").count(), 2, "two pages of 1000");
    let rows = r.rows().await;
    assert_eq!(rows.iter().filter(|i| i.reason.as_deref() == Some("[Imported] old raid")).count(), 603);
    assert_eq!(rows.iter().filter(|i| i.reason.as_deref() == Some("[Imported] Pre-bot ban — reason unknown")).count(), 602);
    assert!(!rows.iter().find(|i| i.id == gone.id).expect("row").active);
    let again = svc::sync_guild_bans(&r.t, r.gid(), TEST_BOT_ID).await.expect("synced");
    assert_eq!(again.imported, 0, "never duplicates");
    r.done().await;
}

#[tokio::test]
async fn external_ban_and_unban_are_recorded() {
    let Some(r) = Room::new("930000000000000011").await else { return };
    let user: User = serde_json::from_value(user_json(OUTSIDER.parse().unwrap_or(1))).expect("user");
    lock(&r.rest.audit).push(AuditEntry {
        target_id: Some(OUTSIDER.into()),
        executor_id: Some(OWNER.into()),
        executor: serde_json::from_value(user_json(OWNER.parse().unwrap_or(1))).ok(),
        reason: Some("by hand".into()),
        created_ms: chrono::Utc::now().timestamp_millis(),
    });
    let h = svc::record_external_ban(&r.t, r.gid(), &user, None).await.expect("detached");
    h.await.expect("join");
    let rows = r.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].r#type, rows[0].reason.as_deref(), rows[0].moderator_id.as_str()), (InfractionType::Ban, Some("[External] by hand"), OWNER));
    assert!(svc::record_external_ban(&r.t, r.gid(), &user, None).await.is_none(), "an active row exists");
    // The bot's own ban is skipped by its marker.
    svc::mark_bot_ban_action(&r.t, r.g, ALICE, "ban", None);
    let alice: User = serde_json::from_value(user_json(ALICE.parse().unwrap_or(1))).expect("user");
    assert!(svc::record_external_ban(&r.t, r.gid(), &alice, None).await.is_none());

    // Unban: rows closed always; the UNBAN case while the module is on.
    lock(&r.rest.audit).clear();
    let settings = r.t.settings().get_settings_safe(r.g).await;
    let h = svc::handle_external_unban(&r.t, r.gid(), &user, settings.as_deref()).await.expect("detached");
    h.await.expect("join");
    let rows = r.rows().await;
    assert!(!rows[0].active);
    assert_eq!((rows[1].r#type, rows[1].reason.as_deref()), (InfractionType::Unban, Some("[External] No reason provided (moderator unknown)")));
    assert!(svc::handle_external_unban(&r.t, r.gid(), &user, settings.as_deref()).await.is_none(), "nothing left to close");
    r.done().await;
}

// ── The plugin's guildReady hook and its Redis key (cross-language) ───────────

#[tokio::test]
async fn guild_ready_claims_the_ban_sync_like_the_js() {
    let Some(r) = Room::build("930000000000000012", true).await else { return };
    let key = feat_moderation::plugin::bansync_key(r.g);
    r.t.cache().del(std::slice::from_ref(&key)).await;
    // The JS claimed it first: Rust skips the import on startup but always runs it on a join.
    assert_eq!(js(&["cache", "setnx", &key]), json!(true));
    assert!(!feat_moderation::plugin::claim_ban_sync(&r.t, r.g, GuildReadyReason::Startup).await);
    assert!(feat_moderation::plugin::claim_ban_sync(&r.t, r.g, GuildReadyReason::Join).await);
    assert_eq!(js(&["cache", "get", &key]), json!("1"), "Rust wrote what the JS reads");
    r.t.cache().del(std::slice::from_ref(&key)).await;
    assert!(feat_moderation::plugin::claim_ban_sync(&r.t, r.g, GuildReadyReason::Available).await);
    assert_eq!(js(&["cache", "setnx", &key]), json!(false), "the JS sees Rust's claim");

    // The hook itself: reconcile always, the import while the module is on.
    r.t.cache().del(std::slice::from_ref(&key)).await;
    r.rest.ban_user(OUTSIDER, Some("x"));
    r.t.install({
        let mut reg = Registry::new();
        feat_moderation::register(&mut reg);
        reg
    });
    let ctx = r.t.run_hooks(GuildReady { guild_id: r.gid(), reason: GuildReadyReason::Startup }).await;
    let _ = ctx;
    for _ in 0..50 {
        if !r.rows().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(r.rows().await.first().and_then(|i| i.reason.clone()).as_deref(), Some("[Imported] x"));
    r.t.cache().del(std::slice::from_ref(&key)).await;
    r.done().await;
}

// ── Rust writes, the JS reads ─────────────────────────────────────────────────

#[tokio::test]
async fn rust_written_cases_and_jobs_read_the_same_through_prisma() {
    let Some(r) = Room::new("930000000000000013").await else { return };
    let tb = svc::execute_mod_action(
        &r.t,
        ModActionInput { duration_ms: Some(86_400_000.0), reason: Some("tb".into()), proof_url: Some("https://example.com/p".into()), ..r.input(OUTSIDER, "BAN") },
    )
    .await
    .expect("temp banned");
    let mute = svc::execute_mod_action(&r.t, ModActionInput { duration_ms: Some(30.0 * 86_400_000.0), ..r.input(ALICE, "TIMEOUT") }).await.expect("muted");
    let back = js(&["read", r.g]);
    let ours: Vec<Value> = r.rows().await.iter().map(|i| serde_json::to_value(i).unwrap_or(Value::Null)).collect();
    assert_eq!(back["infractions"], json!(ours), "the rows read the same through Prisma and Rust");
    let jobs = back["jobs"].as_array().cloned().unwrap_or_default();
    let shape: Vec<(Value, Value, Value, Value)> = jobs.iter().map(|j| (j["type"].clone(), j["dedupeKey"].clone(), j["payload"].clone(), j["maxAttempts"].clone())).collect();
    assert_eq!(
        shape,
        vec![
            (json!("mod.expire"), json!(svc::expire_key(mute.infraction.id)), json!({ "infractionId": mute.infraction.id, "roleId": MUTE_ROLE }), json!(20)),
            (json!("mod.unban"), json!(svc::unban_key(r.g, OUTSIDER)), json!({ "userId": OUTSIDER, "infractionId": tb.infraction.id }), json!(20)),
        ]
    );
    // The JS service resolves what Rust recorded.
    assert_eq!(js(&["resolve", r.g, OUTSIDER]), json!(1));
    r.done().await;
}

// ── The commands through the router ───────────────────────────────────────────

fn slash(name: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user(MOD).roles(&[MOD_ROLE]).permissions(P::MODERATE_MEMBERS | P::KICK_MEMBERS | P::BAN_MEMBERS).channel(GENERAL)
}

fn admin(name: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user(OWNER).permissions(P::ADMINISTRATOR).channel(GENERAL)
}

fn with_user(mut i: FakeInteraction, id: &str) -> FakeInteraction {
    i.json_mut()["data"]["resolved"]["users"][id] = json!({ "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null });
    i
}

fn with_role(mut i: FakeInteraction, id: &str) -> FakeInteraction {
    i.json_mut()["data"]["resolved"]["roles"][id] = json!({
        "id": id, "name": "r", "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false, "mentionable": false, "flags": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    });
    i
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_moderation::register(&mut reg);
    reg
}

#[tokio::test]
async fn warn_case_and_unban_through_the_router() {
    use bot_core::command::option_type::{INTEGER, STRING, USER};
    let Some(r) = Room::new("930000000000000014").await else { return };
    r.t.install(registry());
    let route = |i| bot_core::router::route_interaction(&r.t, i);

    route(with_user(slash("warn", r.g).option("user", USER, json!(ALICE)).option("reason", STRING, json!("rude")), ALICE).build()).await;
    let calls = r.t.responses.calls();
    assert_eq!(r.t.responses.names(), vec!["deferReply", "deleteReply", "followUp"], "PUBLIC: the deferred reply is replaced by a public post");
    assert!(calls[0].ephemeral());
    assert_eq!(calls[2].embed_title(), Some("✅  Warning Issued"));
    assert_eq!(
        calls[2].embed_description(),
        Some(format!("**User:** <@{ALICE}> `user{ALICE}`\n**Reason:** rude\n**Active warnings:** 1\n**Case:** #1").as_str())
    );

    r.t.responses.clear();
    route(slash("case", r.g).subcommand("view").option("id", INTEGER, json!(1)).build()).await;
    let calls = r.t.responses.calls();
    assert_eq!(calls.last().and_then(|c| c.embed_title()), Some("⚠️ Case #1 | Warn"));
    r.t.responses.clear();
    route(slash("case", r.g).subcommand("view").option("id", INTEGER, json!(99)).build()).await;
    assert_eq!(r.t.responses.calls().last().and_then(|c| c.embed_description()), Some("Case #99 doesn't exist in this server."));
    r.t.responses.clear();
    route(slash("case", r.g).subcommand("reason").option("id", INTEGER, json!(1)).option("reason", STRING, json!("very\nrude")).build()).await;
    assert_eq!(r.t.responses.calls().last().and_then(|c| c.embed_description()), Some("Case #1 reason is now:\n> very\n> rude"));
    assert_eq!(r.rows().await[0].reason.as_deref(), Some("very\nrude"));
    assert!(r.rest.sent_titles().contains(&"📝 Case #1 edited".to_string()));
    r.t.responses.clear();
    route(slash("case", r.g).subcommand("proof").option("id", INTEGER, json!(1)).option("url", STRING, json!("javascript:x")).build()).await;
    assert_eq!(r.t.responses.calls().last().and_then(|c| c.embed_title()), Some("❌  Invalid Link"));

    // PRIVATE visibility: lookups edit the private reply; history pages are private.
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("modVisibility", "PRIVATE")).await.expect("settings");
    r.t.responses.clear();
    route(with_user(slash("case", r.g).subcommand("history").option("user", USER, json!(ALICE)), ALICE).build()).await;
    let calls = r.t.responses.calls();
    assert_eq!(r.t.responses.names()[..2], ["deferReply", "editReply"]);
    assert_eq!(calls[1].embed_title(), Some(format!("📋 Infractions | user{ALICE}").as_str()));
    assert_eq!(calls[1].body["embeds"][0]["description"], "⚠️ 1× Warn\n**Active warnings:** 1");
    r.t.responses.clear();
    route(with_user(slash("case", r.g).subcommand("history").option("user", USER, json!(BOB)), BOB).build()).await;
    assert_eq!(r.t.responses.calls().last().and_then(|c| c.embed_description()), Some(format!("No moderation history for <@{BOB}>.").as_str()));

    // /unban of a user who is not banned.
    r.t.responses.clear();
    route(slash("unban", r.g).option("user_id", STRING, json!(format!("<@{OUTSIDER}>"))).build()).await;
    assert_eq!(r.t.responses.calls().last().and_then(|c| c.embed_title()), Some("❌  Nothing To Do"));
    assert_eq!(feat_moderation::commands::unban::clean_user_id(" <@!123> "), "123");

    // A moderator may not clear the history (ADMIN only).
    r.t.responses.clear();
    route(with_user(slash("case", r.g).subcommand("clear-history").option("user", USER, json!(ALICE)), ALICE).build()).await;
    assert_eq!(r.t.responses.names(), vec!["reply"], "the router refuses before the command runs");
    r.done().await;
}

#[tokio::test]
async fn modconfig_through_the_router() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, INTEGER, ROLE, STRING};
    let Some(r) = Room::new("930000000000000015").await else { return };
    r.t.install(registry());
    let route = |i| bot_core::router::route_interaction(&r.t, i);
    let last = || r.t.responses.calls().last().cloned().expect("a call");

    route(admin("modconfig", r.g).group("escalation", "add").option("warns", INTEGER, json!(3)).option("action", STRING, json!("TIMEOUT")).build()).await;
    assert_eq!(last().embed_description(), Some("Timeout tiers need a duration between 1 minute and 28 days. For longer mutes use the Mute action with a mute role."));
    assert_eq!(last().embed_title(), Some("❌  Not Saved"));
    assert!(!r.t.responses.calls()[0].ephemeral(), "admin config commands answer publicly");
    r.t.responses.clear();
    route(
        admin("modconfig", r.g)
            .group("escalation", "add")
            .option("warns", INTEGER, json!(3))
            .option("action", STRING, json!("TIMEOUT"))
            .option("within", STRING, json!("7d"))
            .option("duration", STRING, json!("1h"))
            .build(),
    )
    .await;
    assert_eq!(last().embed_title(), Some("✅  Escalation Tier Added"));
    assert_eq!(last().embed_description(), Some("All tiers (the one just saved is in bold):\n**T1: 3 warnings within 1 week → Timeout for 1 hour**"));
    let saved = r.t.settings().get_settings(r.g).await.expect("settings");
    assert_eq!(
        saved.infraction_escalation,
        Some(json!([{ "id": 1, "warnCount": 3, "withinSeconds": 604800, "action": "TIMEOUT", "durationSeconds": 3600, "countFrom": null }]))
    );
    // The JS reads the tiers Rust saved exactly as its own normalizer would write them.
    let back = js(&["pure", &json!({ "tiers": [saved.infraction_escalation] }).to_string()]);
    assert_eq!(back["tiers"][0], saved.infraction_escalation.clone().unwrap_or(Value::Null));
    r.t.responses.clear();
    route(admin("modconfig", r.g).group("escalation", "list").build()).await;
    assert_eq!(last().embed_title(), Some("📈 Escalation Tiers"));
    r.t.responses.clear();
    route(admin("modconfig", r.g).group("escalation", "remove").option("tier", INTEGER, json!(9)).build()).await;
    assert_eq!(last().embed_description(), Some("There is no tier T9. See `/modconfig escalation list`."));
    r.t.responses.clear();
    route(FakeInteraction::autocomplete("modconfig").guild(r.g).user(OWNER).permissions(P::ADMINISTRATOR).group("escalation", "remove").focused("tier", "week").build()).await;
    assert_eq!(last().choices().len(), 1);
    r.t.responses.clear();
    route(admin("modconfig", r.g).group("escalation", "remove").option("tier", INTEGER, json!(1)).build()).await;
    assert_eq!(last().embed_description(), Some("Removed tier T1.\n\nNo escalation tiers. Add one with `/modconfig escalation add`."));

    r.t.responses.clear();
    route(with_role(admin("modconfig", r.g).group("immune", "add").option("role", ROLE, json!(MOD_ROLE)), MOD_ROLE).build()).await;
    assert_eq!(last().embed_title(), Some("✅  Role Added"));
    r.t.responses.clear();
    route(with_role(admin("modconfig", r.g).group("immune", "add").option("role", ROLE, json!(MOD_ROLE)), MOD_ROLE).build()).await;
    assert_eq!(last().embed_title(), Some("ℹ️  Already Listed"));
    *lock(&r.rest.role_ids) = vec![MOD_ROLE.parse().unwrap_or(1)];
    r.t.responses.clear();
    route(admin("modconfig", r.g).group("immune", "list").build()).await;
    assert_eq!(last().embed_description(), Some(format!("Removed 1 deleted role(s).\n<@&{MOD_ROLE}>").as_str()), "the stale immune role was pruned");
    r.t.responses.clear();
    route(with_role(admin("modconfig", r.g).group("immune", "remove").option("role", ROLE, json!(MUTE_ROLE)), MUTE_ROLE).build()).await;
    assert_eq!(last().embed_title(), Some("ℹ️  Not Listed"));

    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("visibility").option("mode", STRING, json!("ANONYMOUS")).build()).await;
    assert!(last().embed_description().unwrap_or("").starts_with("**Anonymous**\nA public notice"));
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("dm").option("enabled", BOOLEAN, json!(false)).build()).await;
    assert_eq!(last().embed_description(), Some("Members will no longer be DMed about moderation actions."));
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("logchannel").option("channel", CHANNEL, json!(LOG)).build()).await;
    assert_eq!(last().embed_description(), Some(format!("Case logs will go to <#{LOG}>. They are posted even when the Audit Logs module is off.").as_str()));
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("muterole").build()).await;
    assert_eq!(last().embed_description(), Some(format!("The mute role is <@&{MUTE_ROLE}>.").as_str()));
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("muterole").option("setup", BOOLEAN, json!(true)).build()).await;
    assert_eq!(last().embed_title(), Some("✅  Mute Role Saved"));
    assert!(last().embed_description().unwrap_or("").contains("**Channel overrides:** 2 applied"), "{:?}", last().embed_description());
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("show").build()).await;
    let fields: Vec<(String, String)> = last().body["embeds"][0]["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into()))
        .collect();
    assert_eq!(fields[0], ("Module".into(), "✅ On".into()));
    assert_eq!(fields[2], ("Visibility".into(), "ANONYMOUS".into()));
    assert_eq!(fields[3], ("DM members".into(), "Off".into()));
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("muterole").option("clear", BOOLEAN, json!(true)).build()).await;
    assert_eq!(last().embed_title(), Some("✅  Mute Role Cleared"));
    assert_eq!(r.t.settings().get_settings(r.g).await.expect("settings").mute_role_id, None);
    r.done().await;
}

#[tokio::test]
async fn deliver_mod_reply_honours_anonymous_and_private_visibility() {
    use bot_core::command::option_type::{STRING, USER};
    let Some(r) = Room::new("930000000000000016").await else { return };
    r.t.install(registry());
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("modVisibility", "ANONYMOUS")).await.expect("settings");
    let kick = || with_user(slash("kick", r.g).option("user", USER, json!(ALICE)).option("reason", STRING, json!("x")), ALICE).build();
    bot_core::router::route_interaction(&r.t, kick()).await;
    assert_eq!(r.t.responses.names(), vec!["deferReply", "editReply"]);
    let desc = r.t.responses.calls()[1].embed_description().unwrap_or_default().to_string();
    assert!(desc.ends_with("\n\n*Posted in this channel without your name.*"), "{desc}");
    assert!(r.rest.sent_titles().contains(&"✅  Member Kicked".to_string()), "the public notice went to the channel");
    let dm = lock(&r.rest.dms)[0].1.clone();
    assert_eq!(dm["embeds"][0]["fields"].as_array().map(Vec::len), Some(1), "no moderator in the DM unless PUBLIC");

    r.t.settings().update_settings(r.g, SettingsPatch::new().set("modVisibility", "PRIVATE").set("modDmTargets", false)).await.expect("settings");
    r.t.responses.clear();
    bot_core::router::route_interaction(&r.t, kick()).await;
    assert_eq!(r.t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(r.t.responses.calls()[1].embed_title(), Some("✅  Member Kicked"));
    assert_eq!(lock(&r.rest.dms).len(), 1, "modDmTargets off: no DM");

    // An error result is always the private edit.
    r.t.responses.clear();
    bot_core::router::route_interaction(&r.t, with_user(slash("kick", r.g).option("user", USER, json!(OWNER)), OWNER).build()).await;
    assert_eq!(r.t.responses.calls()[1].embed_title(), Some("❌  Not Allowed"));
    assert_eq!(r.t.responses.calls()[1].embed_description(), Some(svc::msg::OWNER));
    let _: Option<BoxFuture<()>> = None;
    r.done().await;
}

// ── More from moderation.test.js, sec-moderation.test.js and moderation-commands.test.js ──

#[tokio::test]
async fn reconcile_is_not_starved_by_500_ancient_bans() {
    let Some(r) = Room::new("930000000000000020").await else { return };
    let old = bot_db::serde_js::from_epoch_ms(chrono::Utc::now().timestamp_millis() - 40 * 86_400_000).expect("date");
    let rows: Vec<svc::BulkInfraction> = (0..510u64)
        .map(|i| svc::BulkInfraction {
            target_user_id: (700_000_000_000_000_000 + i).to_string(),
            moderator_id: MOD.into(),
            r#type: InfractionType::Ban,
            reason: None,
            active: Some(true),
            expires_at: Some(old),
        })
        .collect();
    svc::record_infractions_bulk(&r.t, r.g, rows[..500].to_vec()).await.expect("bulk");
    svc::record_infractions_bulk(&r.t, r.g, rows[500..].to_vec()).await.expect("bulk");
    let fresh = bot_db::serde_js::from_epoch_ms(chrono::Utc::now().timestamp_millis() + 3_600_000).expect("date");
    svc::record_infraction(&r.t, svc::NewInfraction { expires_at: Some(fresh), ..svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Ban) }).await.expect("row");
    let out = svc::reconcile_expiries(&r.t, r.g).await;
    assert_eq!((out.scheduled, out.overdue), (1, 510));
    assert!(r.job(&svc::unban_key(r.g, OUTSIDER)).await.is_some());
    r.done().await;
}

const FORGED: &str = "[Escalation T1] Reached 3 active warnings within 1 week";

#[tokio::test]
async fn escalation_ignores_forged_tags_and_only_run_escalation_sets_the_tier() {
    let Some(r) = Room::new("930000000000000021").await else { return };
    let tiers = json!([{ "id": 1, "warnCount": 3, "withinSeconds": 604800, "action": "TIMEOUT", "durationSeconds": 3600, "countFrom": null }]);
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("infractionEscalation", tiers)).await.expect("settings");
    let warn = |reason: &str| svc::execute_mod_action(&r.t, ModActionInput { reason: Some(reason.into()), ..r.input(ALICE, "WARN") });
    // A forged tag in a warn reason does not count as a fired tier.
    assert!(warn(FORGED).await.expect("warned").escalation.is_none());
    // Nor in an UNMUTE / UNBAN reason.
    svc::record_infraction(&r.t, svc::NewInfraction { reason: Some(FORGED.into()), ..svc::NewInfraction::new(r.g, ALICE, MOD, InfractionType::Unmute) }).await.expect("row");
    assert!(warn("rule 2").await.expect("warned").escalation.is_none());
    let third = warn("rule 3").await.expect("warned");
    assert_eq!(third.escalation.as_ref().map(|e| (e.tier_id, e.ok)), Some((1, true)), "T1 still fires");
    let esc = r.rows().await.into_iter().find(|i| i.r#type == InfractionType::Timeout).expect("escalation case");
    assert_eq!(esc.escalation_tier_id, Some(1));
    assert!(esc.reason.as_deref().unwrap_or("").starts_with("[Escalation T1] Reached 3 active warnings within 1 week"));
    // Editing the escalation case's reason does not re-arm the tier.
    sqlx::query(r#"UPDATE "Infraction" SET "reason" = 'harmless note' WHERE "id" = $1"#).bind(esc.id).execute(r.t.db()).await.expect("edit");
    assert!(warn("rule 4").await.expect("warned").escalation.is_none(), "T1 fires once per window");

    // Only runEscalation (the bot, source escalation) can set escalationTierId.
    let person = svc::execute_mod_action(&r.t, ModActionInput { escalation_tier_id: Some(1), skip_escalation: true, ..r.input(CAROL, "WARN") }).await.expect("warned");
    assert_eq!(person.infraction.escalation_tier_id, None);
    let dash = svc::execute_mod_action(&r.t, ModActionInput { escalation_tier_id: Some(1), source: "dashboard".into(), skip_escalation: true, ..r.input(CAROL, "WARN") }).await.expect("warned");
    assert_eq!(dash.infraction.escalation_tier_id, None);
    let spoof = svc::execute_mod_action(&r.t, ModActionInput { escalation_tier_id: Some(1), source: "escalation".into(), ..r.input(CAROL, "WARN") }).await.expect("warned");
    assert_eq!(spoof.infraction.escalation_tier_id, None, "not the bot: ignored even with source escalation");
    let bot = UserRef::id(TEST_BOT_ID);
    let real = svc::execute_mod_action(&r.t, ModActionInput { escalation_tier_id: Some(2), source: "escalation".into(), moderator: bot, ..r.input(CAROL, "WARN") }).await.expect("warned");
    assert_eq!(real.infraction.escalation_tier_id, Some(2));
    r.done().await;
}

#[tokio::test]
async fn unmute_while_away_means_no_mute_role_on_rejoin() {
    let Some(r) = Room::new("930000000000000022").await else { return };
    svc::record_infraction(&r.t, svc::NewInfraction::new(r.g, OUTSIDER, MOD, InfractionType::Mute)).await.expect("row");
    let settings = r.t.settings().get_settings_safe(r.g).await;
    let outsider = UserId::new(OUTSIDER.parse().unwrap_or(1));
    svc::reapply_mute_on_join(&r.t, r.gid(), outsider, settings.as_deref()).await;
    assert_eq!(r.rest.ops(), vec!["add_role"], "still muted: the role comes back");
    lock(&r.rest.calls).clear();
    let away = svc::execute_unmute(&r.t, r.g, &UserRef::id(OUTSIDER), None, &r.moderator(), Some("appealed"), "command").await.expect("records closed");
    assert_eq!(away.cleared, 1);
    lock(&r.rest.calls).clear();
    svc::reapply_mute_on_join(&r.t, r.gid(), outsider, settings.as_deref()).await;
    assert!(r.rest.calls().is_empty(), "unmuted while away: no role on rejoin");
    r.done().await;
}

#[tokio::test]
async fn softban_incomplete_and_dm_failures_are_reported() {
    use bot_core::command::option_type::USER;
    let Some(r) = Room::new("930000000000000023").await else { return };
    r.t.install(registry());
    r.rest.fail_op("unban", 500);
    r.rest.fail_op("send_dm", 50007);
    bot_core::router::route_interaction(&r.t, with_user(slash("softban", r.g).option("user", USER, json!(ALICE)), ALICE).build()).await;
    assert_eq!(r.t.responses.names(), vec!["deferReply", "deleteReply", "followUp", "followUp"]);
    let calls = r.t.responses.calls();
    assert_eq!(calls[2].embed_title(), Some("✅  User Banned (Softban Incomplete)"));
    assert!(calls[3].ephemeral());
    assert_eq!(calls[3].embed_title(), Some("⚠️  Note"));
    assert_eq!(
        calls[3].embed_description(),
        Some("• I couldn't DM the user.\n• The ban was applied, but the automatic unban failed, so the user is still banned. Use `/unban` to lift it.")
    );
    let rows = r.rows().await;
    assert_eq!((rows[0].r#type, rows[0].active), (InfractionType::Ban, true));
    assert!(rows[0].reason.as_deref().unwrap_or("").starts_with("[Softban: unban failed] "));
    assert_eq!(r.rest.ops().iter().filter(|o| *o == "unban").count(), 2, "one retry");
    assert!(r.rest.calls().iter().any(|c| c.starts_with(&format!("ban {ALICE} 604800 "))), "a softban deletes 7 days by default");
    r.done().await;
}

#[tokio::test]
async fn case_history_public_filters_and_live_hints() {
    use bot_core::command::option_type::{STRING, USER};
    let Some(r) = Room::new("930000000000000024").await else { return };
    r.t.install(registry());
    svc::execute_mod_action(&r.t, ModActionInput { reason: Some("line one\n  line two".into()), ..r.input(ALICE, "WARN") }).await.expect("warned");
    let route = |i| bot_core::router::route_interaction(&r.t, i);
    // PUBLIC: the deferred reply is deleted and the page goes out as a follow-up.
    route(with_user(slash("case", r.g).subcommand("history").option("user", USER, json!(ALICE)), ALICE).build()).await;
    assert_eq!(r.t.responses.names(), vec!["deferReply", "deleteReply", "followUp"]);
    let page = r.t.responses.calls()[2].body["embeds"][0].clone();
    assert_eq!(page["footer"]["text"], "1 record(s)");
    assert_eq!(page["fields"][0]["name"], "⚠️ Case #1 | Warn 🔴 active");
    assert!(page["fields"][0]["value"].as_str().unwrap_or("").ends_with(&format!("by <@{MOD}>\n> line one line two")));
    // A filter that matches nothing, for someone with records, never claims a missing ban.
    r.rest.ban_user(ALICE, Some("raid"));
    r.t.responses.clear();
    route(with_user(slash("case", r.g).subcommand("history").option("user", USER, json!(ALICE)).option("type", STRING, json!("BAN")), ALICE).build()).await;
    assert_eq!(r.t.responses.calls()[2].embed_description(), Some(format!("No moderation history for <@{ALICE}> (filter: Bans).").as_str()));
    // No records at all: the live ban is explained.
    r.rest.ban_user(OUTSIDER, Some("raid `x`"));
    r.t.responses.clear();
    route(with_user(slash("case", r.g).subcommand("history").option("user", USER, json!(OUTSIDER)), OUTSIDER).build()).await;
    assert_eq!(
        r.t.responses.calls()[2].embed_description(),
        Some(
            format!(
                "No moderation history for <@{OUTSIDER}>.\n\n⚠️ **This user is currently banned**, but the ban is not in the records yet (it will be imported at the next ban sync).\nBan reason on record: raid 'x'"
            )
            .as_str()
        )
    );
    // A member timed out by someone else.
    r.t.responses.clear();
    route(with_user(slash("case", r.g).subcommand("history").option("user", USER, json!(CAROL)), CAROL).build()).await;
    assert!(r.t.responses.calls()[2].embed_description().unwrap_or("").contains("⚠️ **This member is currently timed out** (not by this bot).\nTimeout ends: <t:"));
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case_clear_history_pardons_after_a_confirmation() {
    use bot_core::command::option_type::{STRING, USER};
    let Some(r) = Room::new("930000000000000025").await else { return };
    r.t.install(registry());
    for _ in 0..2 {
        svc::execute_mod_action(&r.t, r.input(ALICE, "WARN")).await.expect("warned");
    }
    let interaction = with_user(admin("case", r.g).subcommand("clear-history").option("user", USER, json!(ALICE)).option("reason", STRING, json!("fresh start")), ALICE).build();
    let app: App = (*r.t).clone();
    let run = tokio::spawn(async move { bot_core::router::route_interaction(&app, interaction).await });
    for _ in 0..200 {
        if !r.t.collectors().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = r.t.responses.calls().last().cloned().expect("the prompt");
    assert_eq!(prompt.embed_title(), Some("⚠️  Clear warnings?"));
    assert!(prompt.embed_description().unwrap_or("").starts_with(&format!("This pardons **2** active warning(s) for <@{ALICE}>.")));
    let yes = prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or("").to_string();
    assert!(yes.starts_with("confirm:") && yes.ends_with(":yes"), "{yes}");
    let click = FakeInteraction::button(&yes).guild(r.g).user(OWNER).permissions(P::ADMINISTRATOR).message_id("1000000000000000100").build();
    bot_core::router::route_interaction(&r.t, click).await;
    run.await.expect("join");
    assert_eq!(svc::count_active_warns(&r.t, r.g, ALICE).await.expect("count"), 0);
    let note = r.rows().await.into_iter().find(|i| i.r#type == InfractionType::Note).expect("pardon note");
    assert_eq!(note.reason.as_deref(), Some("[Pardon] Cleared 2 warning(s): fresh start"));
    let last = r.t.responses.calls().last().cloned().expect("reply");
    assert_eq!(last.embed_title(), Some("✅  Warnings Cleared"));
    assert_eq!(
        last.embed_description(),
        Some(format!("**User:** <@{ALICE}> `user{ALICE}`\n**Warnings cleared:** 2\n**Reason:** fresh start\n**Case:** #3").as_str())
    );
    r.done().await;
}

#[tokio::test]
async fn js_reconciled_mute_job_without_a_role_uses_the_mute_role_setting() {
    let Some(r) = Room::new("930000000000000026").await else { return };
    r.t.install(registry());
    let now = chrono::Utc::now().timestamp_millis();
    let rows = js(&["record", r.g, &json!([{ "targetUserId": CAROL, "moderatorId": MOD, "type": "MUTE", "reason": "m", "expiresAt": now - 1000 }]).to_string()]);
    let id = rows[0]["id"].as_i64().unwrap_or(0) as i32;
    // The JS service schedules the expiry itself (as it does at startup).
    assert_eq!(js(&["reconcile", r.g])["scheduled"], 1);
    let job = r.job(&svc::expire_key(id)).await.expect("JS-written job");
    assert_eq!(job.payload, json!({ "infractionId": id }), "no roleId: the handler reads muteRoleId");
    r.t.scheduler().run_job(&r.t, job).await;
    assert!(r.job(&svc::expire_key(id)).await.is_none());
    assert!(r.rest.calls().iter().any(|c| c == &format!("remove_role {CAROL} {MUTE_ROLE} Mute expired (case #1)")));
    let rows = r.rows().await;
    assert_eq!((rows[0].active, rows[1].r#type), (false, InfractionType::Unmute));
    // The JS reads what Rust closed and recorded.
    let back = js(&["read", r.g]);
    assert_eq!(back["infractions"][0]["active"], false);
    assert_eq!(back["infractions"][1]["reason"], "Mute expired (case #1)");
    r.done().await;
}

#[tokio::test]
async fn the_reply_helpers_survive_a_deleted_interaction() {
    use bot_core::command::option_type::{STRING, USER};
    let Some(r) = Room::new("930000000000000027").await else { return };
    r.t.install(registry());
    r.t.responses.fail_next("followUp", 10062);
    bot_core::router::route_interaction(&r.t, with_user(slash("warn", r.g).option("user", USER, json!(ALICE)).option("reason", STRING, json!("x")), ALICE).build()).await;
    assert_eq!(r.rows().await.len(), 1, "the case is recorded even though the reply could not be sent");
    r.t.responses.clear();
    r.t.responses.fail_next("editReply", 10062);
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("modVisibility", "PRIVATE")).await.expect("settings");
    bot_core::router::route_interaction(&r.t, with_user(slash("kick", r.g).option("user", USER, json!(OWNER)), OWNER).build()).await;
    assert_eq!(r.t.responses.names(), vec!["deferReply", "editReply"], "replyModError tried once and gave up quietly");
    r.done().await;
}

// ── Audit: channels missing from the cache are fetched over REST, as discord.js does ──────────────────────────────

/// A text channel of the guild that is NOT in the cached guild; the bot role may manage it.
fn late_channel(g: &str, id: &str) -> Value {
    json!({
        "id": id, "type": 0, "guild_id": g, "name": "late", "position": 9, "rate_limit_per_user": 0, "nsfw": false,
        "permission_overwrites": [{ "id": BOT_ROLE, "type": 0, "allow": P::MANAGE_CHANNELS.bits().to_string(), "deny": "0" }]
    })
}

#[tokio::test]
async fn uncached_channels_are_fetched_like_guild_channels_fetch() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, STRING};
    // One uncached channel per step: a fetch caches the channel, so a later step must not ride on an earlier fetch.
    const LATE: &str = "810000000000000071";
    const SHOW_LATE: &str = "810000000000000072";
    const PICK_LATE: &str = "810000000000000073";
    const SLOW_LATE: &str = "810000000000000074";
    const G: &str = "930000000000000041";
    let discord_rest = bot_core::testing::FakeRest::new();
    for id in [LATE, SHOW_LATE, PICK_LATE, SLOW_LATE] {
        discord_rest.on("GET", &format!("/channels/{id}"), 200, late_channel(G, id));
    }
    let Some(r) = Room::with_discord_rest(G, &discord_rest).await else { return };
    r.t.install(registry());
    let route = |i| bot_core::router::route_interaction(&r.t, i);
    let last = || r.t.responses.calls().last().cloned().expect("a call");

    // postModLog: `guild.channels.cache.get(id) ?? await guild.channels.fetch(id)`.
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("modLogChannelId", LATE)).await.expect("settings");
    svc::post_mod_log(&r.t, r.gid(), serenity::all::CreateEmbed::new().title("late log")).await;
    assert_eq!(lock(&r.rest.sent).iter().filter(|(c, _)| c.to_string() == LATE).count(), 1, "the case log reached the uncached channel");

    // /modconfig show: the uncached case-log channel is usable, not "⚠️ unusable".
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("modLogChannelId", SHOW_LATE)).await.expect("settings");
    route(admin("modconfig", r.g).subcommand("show").build()).await;
    let fields = last().body["embeds"][0]["fields"].clone();
    assert_eq!(fields[1]["value"], format!("<#{SHOW_LATE}>"));

    // /modconfig logchannel channel:<uncached>: checked on the fetched channel, then saved.
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("logchannel").option("channel", CHANNEL, json!(PICK_LATE)).build()).await;
    assert_eq!(
        last().embed_description(),
        Some(format!("Case logs will go to <#{PICK_LATE}>. They are posted even when the Audit Logs module is off.").as_str())
    );

    // /slowmode channel:<uncached>: a bounded REST fetch, then the edit (the JS answered "Channel Not Found" only when
    // the fetch failed too).
    r.t.responses.clear();
    route(admin("slowmode", r.g).option("duration", STRING, json!("10s")).option("channel", CHANNEL, json!(SLOW_LATE)).build()).await;
    assert!(r.rest.calls().contains(&format!("edit_slowmode {SLOW_LATE} 10")), "{:?} {:?}", r.rest.calls(), r.t.responses.calls());

    // /modconfig muterole setup:true with no mute role: the created role is put into the cache (guild.roles.create).
    r.t.settings().update_settings(r.g, SettingsPatch::new().set("muteRoleId", Value::Null)).await.expect("settings");
    r.t.responses.clear();
    route(admin("modconfig", r.g).subcommand("muterole").option("setup", BOOLEAN, json!(true)).build()).await;
    assert_eq!(last().embed_title(), Some("✅  Mute Role Saved"));
    let cached = feat_moderation::guild_ops::load_guild(&r.t, r.gid()).and_then(|lg| lg.view.role("830000000000000099").cloned());
    assert!(cached.is_some(), "the new mute role is cached at once");
    r.done().await;
}
